//! Receive-limit and outstanding-request wire regressions.
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use radicom::{syntaxes, Dataset, Tag, Vr};
use radicom_network::assembler::{PduSplitter, Stream};
use radicom_network::association::sop_class;
use radicom_network::cfind::{CFindRequest, CFindResponse};
use radicom_network::dimse::{status, Command, CommandField};
use radicom_network::error::Result;
use radicom_network::pdu::{Associate, AsyncOperations, Pdu, Pdv, PresentationContext};
use radicom_network::server::{command_bytes, command_from_bytes, DicomServer, ServerOptions};
use radicom_network::services::{CStoreRequest, CStoreResponse, ServiceProvider};
use radicom_network::transport::{read_pdu, write_pdu};
use radicom_network::{PdvAssembler, SpoolConfig};
use tokio::net::TcpStream;

const STORAGE: &str = "1.2.840.10008.5.1.4.1.1.7";

fn data_pdv(length: usize, is_last: bool) -> Pdv {
    Pdv {
        context_id: 1,
        is_command: false,
        is_last,
        data: vec![0; length].into(),
    }
}

#[test]
fn armed_memory_body_obeys_hard_cap_before_threshold() {
    let mut assembler = PdvAssembler::with_spool(SpoolConfig {
        max_message_length: 1024,
        ..Default::default()
    });
    assembler.arm_spill(1);
    assert!(assembler.push_pdv(&data_pdv(2048, true)).is_err());
    assert!(assembler.is_idle());
}

#[test]
fn fragmented_armed_memory_body_obeys_hard_cap() {
    let mut assembler = PdvAssembler::with_spool(SpoolConfig {
        max_message_length: 1024,
        ..Default::default()
    });
    assembler.arm_spill(1);
    assert!(assembler.push_pdv(&data_pdv(768, false)).unwrap().is_none());
    assert!(assembler.push_pdv(&data_pdv(768, true)).is_err());
    assert!(assembler.is_idle());
}

struct Provider;
#[async_trait::async_trait]
impl ServiceProvider for Provider {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![
            STORAGE.into(),
            sop_class::STUDY_ROOT_QR_FIND.into(),
            sop_class::STUDY_ROOT_QR_GET.into(),
            sop_class::VERIFICATION.into(),
        ]
    }
    async fn on_c_store(&self, request: &CStoreRequest) -> Result<CStoreResponse> {
        Ok(CStoreResponse::success(request))
    }
    fn on_c_get<'a>(
        &'a self,
        request: &'a radicom_network::cget::CGetRequest,
    ) -> futures::stream::BoxStream<'a, Result<radicom_network::cget::CGetEvent>> {
        Box::pin(
            futures::stream::once(async move {
                Ok(radicom_network::cget::CGetEvent::Response(
                    radicom_network::cget::CGetResponse::pending(request, 0, Default::default()),
                ))
            })
            .chain(futures::stream::pending()),
        )
    }
    fn on_c_find<'a>(
        &'a self,
        request: &'a CFindRequest,
    ) -> futures::stream::BoxStream<'a, Result<CFindResponse>> {
        Box::pin(
            futures::stream::once(
                async move { Ok(CFindResponse::pending(request, Dataset::new())) },
            )
            .chain(futures::stream::pending()),
        )
    }
}

async fn start(
    options: ServerOptions,
    protocol_version: u16,
    sop: &str,
    syntax: &str,
) -> (TcpStream, tokio::task::JoinHandle<Result<()>>, Pdu) {
    let server = DicomServer::bind("127.0.0.1:0", Arc::new(Provider), options)
        .await
        .unwrap();
    let addr = server.local_addr();
    let task = tokio::spawn(async move { server.accept_one().await });
    let mut peer = TcpStream::connect(addr).await.unwrap();
    let mut request = Associate::new("SCP", "SCU");
    request.protocol_version = protocol_version;
    request.presentation_contexts = vec![
        PresentationContext::new(1, sop, vec![syntax.into()]),
        PresentationContext::new(3, sop, vec![syntax.into()]),
    ];
    request.user_information.async_operations = Some(AsyncOperations {
        invoked: 2,
        performed: 2,
    });
    write_pdu(&mut peer, &Pdu::AssociateRq(request))
        .await
        .unwrap();
    let answer = read_pdu(&mut peer).await.unwrap().unwrap();
    (peer, task, answer)
}

async fn send_request(peer: &mut TcpStream, command: &Command, body: Option<&[u8]>) {
    for pdata in PduSplitter::new(16384, 16)
        .split_message(1, &command_bytes(command).unwrap(), body)
        .unwrap()
    {
        write_pdu(peer, &Pdu::PData(pdata)).await.unwrap();
    }
}

async fn next_command(peer: &mut TcpStream) -> Command {
    let mut assembler = PdvAssembler::new();
    loop {
        let pdu = tokio::time::timeout(Duration::from_secs(2), read_pdu(peer))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let Pdu::PData(pdata) = pdu else {
            panic!("expected DIMSE response, got {pdu:?}")
        };
        for unit in assembler.push_pdata(&pdata).unwrap() {
            if unit.stream == Stream::Command {
                return command_from_bytes(&unit.bytes).unwrap();
            }
        }
    }
}

#[tokio::test]
async fn scp_accepts_supported_protocol_bit_with_other_bits() {
    let (mut peer, task, answer) = start(
        ServerOptions::default(),
        3,
        sop_class::VERIFICATION,
        syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.uid(),
    )
    .await;
    assert!(matches!(answer, Pdu::AssociateAc(_)), "{answer:?}");
    write_pdu(&mut peer, &Pdu::ReleaseRq).await.unwrap();
    assert!(matches!(
        read_pdu(&mut peer).await.unwrap(),
        Some(Pdu::ReleaseRp)
    ));
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn scp_rejects_protocol_without_supported_bit() {
    let (_peer, task, answer) = start(
        ServerOptions::default(),
        2,
        sop_class::VERIFICATION,
        syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.uid(),
    )
    .await;
    assert!(matches!(answer, Pdu::AssociateRj(_)), "{answer:?}");
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn duplicate_id_after_pending_aborts_association() {
    for (field, sop) in [
        (CommandField::CFindRq, sop_class::STUDY_ROOT_QR_FIND),
        (CommandField::CGetRq, sop_class::STUDY_ROOT_QR_GET),
    ] {
        let options = ServerOptions {
            async_operations: AsyncOperations {
                invoked: 2,
                performed: 2,
            },
            ..Default::default()
        };
        let (mut peer, task, answer) =
            start(options, 1, sop, syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.uid()).await;
        assert!(matches!(answer, Pdu::AssociateAc(_)));
        let mut command = Command::request(field, 42);
        command.affected_sop_class_uid = Some(sop.into());
        command.no_dataset = false;
        send_request(&mut peer, &command, Some(&[])).await;
        assert_eq!(next_command(&mut peer).await.status, Some(status::PENDING));
        send_request(&mut peer, &command, Some(&[])).await;
        let answer = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let pdu = read_pdu(&mut peer).await.unwrap().unwrap();
                if matches!(pdu, Pdu::Abort(_)) {
                    break pdu;
                }
                if let Pdu::PData(data) = &pdu {
                    assert!(
                        data.pdvs.iter().all(|pdv| !pdv.is_command),
                        "duplicate produced another command instead of abort: {pdu:?}"
                    );
                }
            }
        })
        .await
        .expect("duplicate must be rejected promptly");
        assert!(matches!(answer, Pdu::Abort(_)));
        assert!(task.await.unwrap().is_err());
    }
}

#[tokio::test]
async fn in_memory_deflated_store_obeys_configured_output_cap() {
    for memory_threshold in [1024 * 1024, 0] {
        let syntax = syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let options = ServerOptions {
            supported_transfer_syntaxes: vec![syntax.uid().into()],
            store_spool: SpoolConfig {
                memory_threshold,
                max_message_length: 1024,
                ..Default::default()
            },
            ..Default::default()
        };
        let (mut peer, task, answer) = start(options, 1, STORAGE, syntax.uid()).await;
        assert!(matches!(answer, Pdu::AssociateAc(_)));
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, STORAGE);
        dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4");
        dataset.insert_value(Tag::PIXEL_DATA, vec![0; 4096]);
        let bytes = radicom_network::message::encode_dataset(&dataset, &syntax).unwrap();
        assert!(bytes.len() < 1024);
        let mut command = Command::request(CommandField::CStoreRq, 7);
        command.affected_sop_class_uid = Some(STORAGE.into());
        command.affected_sop_instance_uid = Some("1.2.3.4".into());
        command.no_dataset = false;
        send_request(&mut peer, &command, Some(&bytes)).await;
        let response = next_command(&mut peer).await;
        assert_ne!(
            response.status,
            Some(status::SUCCESS),
            "threshold={memory_threshold}"
        );
        write_pdu(&mut peer, &Pdu::ReleaseRq).await.unwrap();
        assert!(matches!(
            read_pdu(&mut peer).await.unwrap(),
            Some(Pdu::ReleaseRp)
        ));
        task.await.unwrap().unwrap();
    }
}

#[test]
fn unarmed_memory_body_obeys_configured_hard_cap() {
    let mut assembler = PdvAssembler::with_spool(SpoolConfig {
        max_message_length: 1024,
        ..Default::default()
    });
    assert!(assembler.push_pdv(&data_pdv(2048, true)).is_err());
    assert!(assembler.is_idle());
}

#[tokio::test]
async fn duplicate_id_is_rejected_before_its_dataset_arrives() {
    let options = ServerOptions {
        async_operations: AsyncOperations {
            invoked: 2,
            performed: 2,
        },
        ..Default::default()
    };
    let (mut peer, task, answer) = start(
        options,
        1,
        sop_class::STUDY_ROOT_QR_FIND,
        syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.uid(),
    )
    .await;
    assert!(matches!(answer, Pdu::AssociateAc(_)));
    let mut command = Command::request(CommandField::CFindRq, 42);
    command.affected_sop_class_uid = Some(sop_class::STUDY_ROOT_QR_FIND.into());
    command.no_dataset = false;
    for context_id in [1, 3] {
        for pdata in PduSplitter::new(16384, 16)
            .split_message(context_id, &command_bytes(&command).unwrap(), None)
            .unwrap()
        {
            write_pdu(&mut peer, &Pdu::PData(pdata)).await.unwrap();
        }
    }
    let answer = tokio::time::timeout(Duration::from_secs(2), read_pdu(&mut peer))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(answer, Pdu::Abort(_)), "{answer:?}");
    assert!(task.await.unwrap().is_err());
}

#[tokio::test]
async fn deflated_find_obeys_configured_output_cap() {
    for memory_threshold in [1024 * 1024, 0] {
        let syntax = syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let options = ServerOptions {
            supported_transfer_syntaxes: vec![syntax.uid().into()],
            store_spool: SpoolConfig {
                memory_threshold,
                max_message_length: 1024,
                ..Default::default()
            },
            ..Default::default()
        };
        let (mut peer, task, answer) =
            start(options, 1, sop_class::STUDY_ROOT_QR_FIND, syntax.uid()).await;
        assert!(matches!(answer, Pdu::AssociateAc(_)));
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::PATIENT_NAME, Vr::Pn, "A".repeat(4096));
        let bytes = radicom_network::message::encode_dataset(&dataset, &syntax).unwrap();
        let mut command = Command::request(CommandField::CFindRq, 7);
        command.affected_sop_class_uid = Some(sop_class::STUDY_ROOT_QR_FIND.into());
        command.no_dataset = false;
        send_request(&mut peer, &command, Some(&bytes)).await;
        assert_eq!(
            next_command(&mut peer).await.status,
            Some(status::PROCESSING_FAILURE)
        );
        write_pdu(&mut peer, &Pdu::ReleaseRq).await.unwrap();
        assert!(matches!(
            read_pdu(&mut peer).await.unwrap(),
            Some(Pdu::ReleaseRp)
        ));
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn store_id_can_be_reused_after_terminal_response() {
    let syntax = syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone();
    let (mut peer, task, answer) = start(ServerOptions::default(), 1, STORAGE, syntax.uid()).await;
    assert!(matches!(answer, Pdu::AssociateAc(_)));
    let mut dataset = Dataset::new();
    dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, STORAGE);
    dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4");
    let bytes = radicom_network::message::encode_dataset(&dataset, &syntax).unwrap();
    for id in [0, 7] {
        let mut command = Command::request(CommandField::CStoreRq, id);
        command.affected_sop_class_uid = Some(STORAGE.into());
        command.affected_sop_instance_uid = Some("1.2.3.4".into());
        command.no_dataset = false;
        for _ in 0..2 {
            send_request(&mut peer, &command, Some(&bytes)).await;
            let response = next_command(&mut peer).await;
            assert_eq!(response.status, Some(status::SUCCESS));
            assert_eq!(response.message_id_being_responded_to, Some(id));
        }
    }
    write_pdu(&mut peer, &Pdu::ReleaseRq).await.unwrap();
    assert!(matches!(
        read_pdu(&mut peer).await.unwrap(),
        Some(Pdu::ReleaseRp)
    ));
    task.await.unwrap().unwrap();
}

#[test]
fn both_body_kinds_bound_deflated_non_storage_decode() {
    let syntax = syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let mut dataset = Dataset::new();
    dataset.insert_str(Tag::PATIENT_NAME, Vr::Pn, "A".repeat(4096));
    let bytes = radicom_network::message::encode_dataset(&dataset, &syntax).unwrap();
    for memory_threshold in [1024 * 1024, 0] {
        let mut assembler = PdvAssembler::with_spool(SpoolConfig {
            memory_threshold,
            max_message_length: 1024,
            ..Default::default()
        });
        assembler.arm_spill(1);
        let unit = assembler
            .push_pdv(&Pdv {
                context_id: 1,
                is_command: false,
                is_last: true,
                data: bytes.clone().into(),
            })
            .unwrap()
            .unwrap();
        assert_eq!(unit.spool.is_some(), memory_threshold == 0);
        let body = unit.body();
        assert!(radicom_network::store::decode_body_with_fallback_limited(
            &body, &syntax, None, 1024
        )
        .is_err());
    }
}
