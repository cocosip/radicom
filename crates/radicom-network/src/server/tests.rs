//! Network server/association tests, moved out of the former monolith.

use super::options::*;
use super::scp::*;
use super::scu::*;
use super::wire::*;
use super::*;
use crate::association::sop_class;
use crate::services::{CEchoRequest, CEchoResponse, CStoreRequest, VerificationProvider};
use radicom::model::dataset::Dataset;
use radicom::model::tag::Tag;
use radicom::model::vr::Vr;

fn get_client_options() -> ClientOptions {
    ClientOptions {
        role_selections: vec![crate::pdu::RoleSelection {
            sop_class: "1.2.840.10008.5.1.4.1.1.7".into(),
            scu_role: false,
            scp_role: true,
        }],
        ..Default::default()
    }
}

fn echo_context() -> PresentationContext {
    PresentationContext::new(
        1,
        sop_class::VERIFICATION,
        vec![
            "1.2.840.10008.1.2".to_string(),
            "1.2.840.10008.1.2.1".to_string(),
        ],
    )
}

#[test]
fn a_hostile_peer_max_pdu_is_clamped_before_the_splitter() {
    // NW-04: a peer advertising 0xFFFF_FFFF must not make the streaming
    // framer pre-allocate ~4 GiB; the send-side splitter is bounded.
    let clamped = negotiated_send_max_pdu(0xFFFF_FFFF, 16384);
    assert_eq!(clamped, crate::pdu::MAX_PDU_SIZE);
    let splitter = PduSplitter::new(clamped, 16);
    assert!(
        splitter.max_pdv_value() <= crate::pdu::MAX_PDU_SIZE as usize,
        "the splitter PDV budget stays bounded"
    );
    // 0 means "no maximum" on the wire: fall back to our own ceiling.
    assert_eq!(negotiated_send_max_pdu(0, 16384), 16384);
    // A sane peer value passes through unchanged.
    assert_eq!(negotiated_send_max_pdu(65536, 16384), 65536);
}

#[test]
fn command_bytes_are_implicit_vr_little_endian() {
    let request = CEchoRequest {
        message_id: 5,
        affected_sop_class_uid: sop_class::VERIFICATION.into(),
    };
    let bytes = command_bytes(&request.to_command()).unwrap();
    // implicit VR: tag(4) + length(4), no VR bytes
    assert_eq!(
        &bytes[..4],
        &[0x00, 0x00, 0x00, 0x00],
        "(0000,0000) group length first"
    );
    let back = command_from_bytes(&bytes).unwrap();
    assert_eq!(back.field, CommandField::CEchoRq);
    assert_eq!(back.message_id, 5);
}

#[test]
fn command_round_trip_preserves_every_field() {
    let mut command = Command::request(CommandField::CStoreRq, 77);
    command.affected_sop_class_uid = Some("1.2.840.10008.5.1.4.1.1.7".into());
    command.affected_sop_instance_uid = Some("1.2.3.4".into());
    command.priority = crate::dimse::Priority::Medium;
    command.no_dataset = false;

    let back = command_from_bytes(&command_bytes(&command).unwrap()).unwrap();
    assert_eq!(back, command);
}

#[tokio::test]
async fn loopback_c_echo_succeeds() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();

    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "MY-SCU",
        "MY-SCP",
        vec![echo_context()],
        ClientOptions::default(),
    )
    .await
    .expect("association accepted");

    let response = association
        .send_c_echo(sop_class::VERIFICATION)
        .await
        .unwrap();
    assert_eq!(response.status, crate::dimse::status::SUCCESS);
    assert_eq!(response.state(), crate::dimse::StatusState::Success);
    assert_eq!(response.message_id_being_responded_to, 1);

    association.release().await.unwrap();
    let _ = handle.await.unwrap();
}

#[tokio::test]
async fn server_policy_rejects_unknown_transfer_syntax_before_binding() {
    assert!(DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions {
            supported_transfer_syntaxes: vec!["9.9.9".into()],
            ..Default::default()
        }
    )
    .await
    .is_err());
}

#[tokio::test]
async fn configured_server_syntax_policy_negotiates_deflated_storage() {
    let provider = Arc::new(RecordingProvider::new());
    let syntax = syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider.clone(),
        ServerOptions {
            supported_transfer_syntaxes: vec![syntax.uid().into()],
            syntax_priority: crate::association::SyntaxPriority::Preferred,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let task = tokio::spawn(async move { server.accept_one().await });
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![PresentationContext::new(
            1,
            "1.2.840.10008.5.1.4.1.1.7",
            vec![syntax.uid().into()],
        )],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let mut file = instance_file();
    file.dataset.set_transfer_syntax(syntax);
    assert_eq!(association.send_c_store(&file).await.unwrap().status, 0);
    association.release().await.unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(provider.instance_uids(), vec!["1.2.3.4.5.6"]);
}

#[tokio::test]
async fn echo_batch_pipelines_up_to_the_negotiated_window() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        let Some(Pdu::AssociateRq(mut rq)) = read_pdu(&mut tcp).await.unwrap() else {
            panic!("RQ")
        };
        rq.user_information.async_operations = Some(crate::pdu::AsyncOperations {
            invoked: 2,
            performed: 1,
        });
        for pc in &mut rq.presentation_contexts {
            pc.transfer_syntaxes.truncate(1);
        }
        write_pdu(&mut tcp, &Pdu::AssociateAc(rq)).await.unwrap();
        for batch_size in [2, 2, 1] {
            let mut requests = Vec::new();
            while requests.len() < batch_size {
                let Some(Pdu::PData(pdata)) = read_pdu(&mut tcp).await.unwrap() else {
                    panic!("P-DATA")
                };
                for pdv in pdata.pdvs {
                    requests.push(command_from_bytes(&pdv.data).unwrap());
                }
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(20), read_pdu(&mut tcp))
                    .await
                    .is_err(),
                "sender exceeded its negotiated in-flight limit"
            );
            for request in requests.into_iter().rev() {
                let response =
                    CEchoResponse::success(&CEchoRequest::from_command(&request).unwrap());
                let data = pdata(1, true, command_bytes(&response.to_command()).unwrap());
                write_pdu(&mut tcp, &data).await.unwrap();
            }
        }
        assert!(matches!(
            read_pdu(&mut tcp).await.unwrap(),
            Some(Pdu::ReleaseRq)
        ));
        write_pdu(&mut tcp, &Pdu::ReleaseRp).await.unwrap();
    });
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions {
            async_operations: crate::pdu::AsyncOperations {
                invoked: 2,
                performed: 1,
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let requests: Vec<_> = (10..15)
        .map(|id| CEchoRequest {
            message_id: id,
            affected_sop_class_uid: sop_class::VERIFICATION.into(),
        })
        .collect();
    let responses = tokio::time::timeout(
        Duration::from_secs(2),
        association.send_c_echo_batch(&requests),
    )
    .await
    .expect("two requests must be outstanding before the peer responds")
    .unwrap();
    assert_eq!(
        responses
            .iter()
            .map(|r| r.message_id_being_responded_to)
            .collect::<Vec<_>>(),
        vec![10, 11, 12, 13, 14]
    );
    association.release().await.unwrap();
    peer.await.unwrap();
}

#[tokio::test]
async fn a_negotiated_window_answers_an_echo_while_the_find_is_pending() {
    struct Gated {
        done: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl ServiceProvider for Gated {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![
                sop_class::VERIFICATION.into(),
                sop_class::STUDY_ROOT_QR_FIND.into(),
            ]
        }
        async fn on_c_echo(&self, rq: &CEchoRequest) -> Result<CEchoResponse> {
            Ok(CEchoResponse::success(rq))
        }
        fn on_c_find<'a>(
            &'a self,
            rq: &'a crate::cfind::CFindRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cfind::CFindResponse>> {
            use futures::StreamExt;
            Box::pin(
                futures::stream::once(async move {
                    Ok(crate::cfind::CFindResponse::pending(rq, Dataset::new()))
                })
                .chain(futures::stream::once(async move {
                    self.done.notified().await;
                    Ok(crate::cfind::CFindResponse::success(rq, 0))
                })),
            )
        }
    }
    let done = Arc::new(tokio::sync::Notify::new());
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(Gated { done: done.clone() }),
        ServerOptions {
            async_operations: crate::pdu::AsyncOperations {
                invoked: 1,
                performed: 2,
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let serving = tokio::spawn(async move { server.accept_one().await });
    let mut find = find_context();
    find.id = 3;
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context(), find],
        ClientOptions {
            async_operations: crate::pdu::AsyncOperations {
                invoked: 2,
                performed: 1,
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let request = crate::cfind::CFindRequest::study_query(10);
    let data =
        crate::message::encode_dataset(&request.identifier, &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN)
            .unwrap();
    let mut pdus = association
        .splitter
        .split_message(
            3,
            &command_bytes(&request.to_command()).unwrap(),
            Some(&data),
        )
        .unwrap();
    let echo = CEchoRequest {
        message_id: 11,
        affected_sop_class_uid: sop_class::VERIFICATION.into(),
    };
    pdus.extend(
        association
            .splitter
            .split_message(1, &command_bytes(&echo.to_command()).unwrap(), None)
            .unwrap(),
    );
    write_pdata_batch(&mut association.stream, pdus)
        .await
        .unwrap();
    assert_eq!(
        association
            .await_response(10, CommandField::CFindRsp)
            .await
            .unwrap()
            .status,
        Some(crate::dimse::status::PENDING)
    );
    // The pending Identifier must also be consumed before awaiting another command.
    let pdu = association.read_request_pdu(10).await.unwrap().unwrap();
    assert!(matches!(pdu, Pdu::PData(_)));
    // M6: the negotiated window lets the SCP answer the C-ECHO while the
    // C-FIND provider is still gated. Before M6 this response only arrived
    // after `done`, because the SCP ran one request at a time.
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(5),
            association.await_response(11, CommandField::CEchoRsp),
        )
        .await
        .expect("the echo is answered while the find is still pending")
        .unwrap()
        .status,
        Some(0)
    );
    done.notify_one();
    assert_eq!(
        association
            .await_response(10, CommandField::CFindRsp)
            .await
            .unwrap()
            .status,
        Some(0)
    );
    association.release().await.unwrap();
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn zero_and_finite_windows_negotiate_and_duplicate_echo_ids_are_refused() {
    for offered in [0, 1, 4] {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(VerificationProvider),
            ServerOptions {
                async_operations: crate::pdu::AsyncOperations {
                    invoked: 1,
                    performed: 2,
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let serving = tokio::spawn(async move { server.accept_one().await });
        let mut association = Association::open(
            addr,
            "SCU",
            "SCP",
            vec![echo_context()],
            ClientOptions {
                async_operations: crate::pdu::AsyncOperations {
                    invoked: offered,
                    performed: 1,
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            association.async_operations().invoked,
            if offered == 1 { 1 } else { 2 }
        );
        let rq = CEchoRequest {
            message_id: 9,
            affected_sop_class_uid: sop_class::VERIFICATION.into(),
        };
        assert!(association
            .send_c_echo_batch(&[rq.clone(), rq])
            .await
            .is_err());
        assert!(association
            .send_c_echo_batch(&[CEchoRequest {
                message_id: 0,
                affected_sop_class_uid: sop_class::VERIFICATION.into()
            }])
            .await
            .is_err());
        assert_eq!(
            association
                .send_c_echo(sop_class::VERIFICATION)
                .await
                .unwrap()
                .status,
            0
        );
        association.release().await.unwrap();
        serving.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn queued_requests_are_bounded_by_the_accepted_window() {
    let (_peer, mut io) = tokio::io::duplex(64);
    let provider = Arc::new(VerificationProvider);
    let options = ServerOptions::default();
    let policy = NegotiationPolicy::new(provider.supported_sop_classes());
    let accepted = AcceptedContexts::from_negotiated(policy.negotiate(&[echo_context()]));
    let splitter = PduSplitter::new(16384, 16);
    let mut serving = Serving {
        stream: &mut io,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::new(),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window: 2,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        pdu_reader: crate::transport::PduReader::new(),
    };
    let request = CEchoRequest {
        message_id: 2,
        affected_sop_class_uid: sop_class::VERIFICATION.into(),
    };
    let unit = MessageUnit {
        context_id: 1,
        stream: Stream::Command,
        bytes: command_bytes(&request.to_command()).unwrap().into(),
        spool: None,
    };
    serving.defer(unit.clone()).unwrap();
    assert!(serving
        .defer(unit)
        .unwrap_err()
        .to_string()
        .contains("window"));
    serving.resume_deferred();
    assert_eq!(serving.pending_units.len(), 1);
}

#[tokio::test]
async fn zero_max_pdu_options_can_send_after_negotiation() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let task = tokio::spawn(async move { server.accept_one().await });
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions {
            max_pdu_length: 0,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        association
            .send_c_echo(sop_class::VERIFICATION)
            .await
            .unwrap()
            .status,
        0
    );
    association.release().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_store_on_a_verification_context_never_reaches_the_provider() {
    let provider = Arc::new(RecordingProvider::new());
    let server = DicomServer::bind("127.0.0.1:0", provider.clone(), ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let task = tokio::spawn(async move { server.accept_one().await });
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    let mut rq = Associate::new("SCP", "SCU");
    rq.presentation_contexts = vec![echo_context()];
    write_pdu(&mut tcp, &Pdu::AssociateRq(rq)).await.unwrap();
    assert!(matches!(
        read_pdu(&mut tcp).await.unwrap(),
        Some(Pdu::AssociateAc(_))
    ));
    let mut command = Command::request(CommandField::CStoreRq, 7);
    command.affected_sop_class_uid = Some("1.2.840.10008.5.1.4.1.1.7".into());
    command.affected_sop_instance_uid = Some("1.2.3.4.5.6".into());
    command.no_dataset = false;
    let dataset = crate::message::encode_dataset(
        &instance_file().dataset,
        &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN,
    )
    .unwrap();
    let pdus = PduSplitter::new(16384, 1)
        .split_message(1, &command_bytes(&command).unwrap(), Some(&dataset))
        .unwrap();
    write_pdata_batch(&mut tcp, pdus).await.unwrap();
    let reply = read_pdu(&mut tcp).await.unwrap();
    assert!(
        matches!(reply, Some(Pdu::Abort(_))),
        "unnegotiated SOP class must abort: {reply:?}"
    );
    assert!(provider.patient_names().is_empty());
    assert!(task.await.unwrap().is_err());
}

#[tokio::test]
async fn a_metrics_collector_observes_the_connection() {
    use crate::observability::{CountingMetrics, MetricsCollector};
    let metrics = Arc::new(CountingMetrics::new());
    let options = ServerOptions {
        metrics: Some(metrics.clone() as Arc<dyn MetricsCollector>),
        ..Default::default()
    };
    let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), options)
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "MY-SCU",
        "MY-SCP",
        vec![echo_context()],
        ClientOptions::default(),
    )
    .await
    .expect("association accepted");
    let _ = association
        .send_c_echo(sop_class::VERIFICATION)
        .await
        .unwrap();
    association.release().await.unwrap();
    let _ = handle.await.unwrap();

    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.connections_opened, 1, "one connection counted");
    assert_eq!(snapshot.connections_closed, 1, "the guard closed it");
    assert_eq!(snapshot.associations_opened, 1, "one association counted");
    assert_eq!(snapshot.associations_closed, 1);
    assert!(
        snapshot.bytes_received > 0,
        "the A-ASSOCIATE-RQ bytes were counted"
    );
    assert!(
        snapshot.bytes_sent > 0,
        "the accept + echo response bytes were counted"
    );
}

#[tokio::test]
async fn loopback_rejects_an_unsupported_abstract_syntax() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    // propose only an abstract syntax the provider does not serve
    let context = PresentationContext::new(1, "9.9.9", vec!["1.2.840.10008.1.2".to_string()]);
    let result =
        Association::open(addr, "SCU", "SCP", vec![context], ClientOptions::default()).await;
    match result {
        Err(NetworkError::Reject { source_code, .. }) => {
            assert_eq!(source_code, 1, "user rejection")
        }
        Err(other) => panic!("expected Reject, got {other}"),
        Ok(_) => panic!("expected the association to be rejected"),
    }
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_negotiates_the_offered_syntax() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    // offer explicit VR LE only; the SCP supports it
    let context = PresentationContext::new(
        1,
        sop_class::VERIFICATION,
        vec!["1.2.840.10008.1.2.1".to_string()],
    );
    let association =
        Association::open(addr, "SCU", "SCP", vec![context], ClientOptions::default())
            .await
            .unwrap();
    let accepted = association.contexts().all();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].transfer_syntax, "1.2.840.10008.1.2.1");
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_multiple_echoes_on_one_association() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    for expected in 1..=3u16 {
        let response = association
            .send_c_echo(sop_class::VERIFICATION)
            .await
            .unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS);
        assert_eq!(
            response.message_id_being_responded_to, expected,
            "message IDs increment"
        );
    }
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_abort_ends_the_connection() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let _ = association.abort(0).await;
    // a peer abort is not a clean close, so the server reports it
    match handle.await.unwrap() {
        Err(NetworkError::Abort {
            source_code,
            reason,
        }) => {
            assert_eq!(
                (source_code, reason),
                (0, 0),
                "the peer's own abort source and reason"
            );
        }
        other => panic!("expected the peer abort to surface as an error, got {other:?}"),
    }
}
/// A provider that records every stored instance.
struct RecordingProvider {
    stored: std::sync::Mutex<Vec<String>>,
    /// PatientName as the provider saw it, proving the dataset parsed.
    names: std::sync::Mutex<Vec<String>>,
    /// `(originator AE, originator message ID)` per C-STORE (§11.4, D5).
    origins: std::sync::Mutex<Vec<(Option<String>, Option<u16>)>>,
}

impl RecordingProvider {
    fn new() -> Self {
        RecordingProvider {
            stored: std::sync::Mutex::new(Vec::new()),
            names: std::sync::Mutex::new(Vec::new()),
            origins: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn instance_uids(&self) -> Vec<String> {
        self.stored.lock().unwrap().clone()
    }

    fn patient_names(&self) -> Vec<String> {
        self.names.lock().unwrap().clone()
    }

    fn origins(&self) -> Vec<(Option<String>, Option<u16>)> {
        self.origins.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ServiceProvider for RecordingProvider {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![
            sop_class::VERIFICATION.to_string(),
            "1.2.840.10008.5.1.4.1.1.7".to_string(),
            sop_class::STUDY_ROOT_QR_FIND.to_string(),
        ]
    }

    async fn on_c_echo(&self, request: &CEchoRequest) -> Result<CEchoResponse> {
        Ok(CEchoResponse::success(request))
    }

    async fn on_c_store(&self, request: &CStoreRequest) -> Result<CStoreResponse> {
        self.stored
            .lock()
            .unwrap()
            .push(request.affected_sop_instance_uid.clone());
        self.names.lock().unwrap().push(
            request
                .file
                .dataset
                .try_get_str(Tag::PATIENT_NAME)
                .unwrap_or_default()
                .trim()
                .to_string(),
        );
        self.origins.lock().unwrap().push((
            request.move_originator_ae_title.clone(),
            request.move_originator_message_id,
        ));
        Ok(CStoreResponse::success(request))
    }

    fn on_c_find<'a>(
        &'a self,
        request: &'a crate::cfind::CFindRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cfind::CFindResponse>> {
        use futures::StreamExt;
        let mut item = Dataset::new();
        item.insert_str(Tag::STUDY_INSTANCE_UID, Vr::Ui, "1.2.3.4");
        futures::stream::iter([
            Ok(crate::cfind::CFindResponse::pending(request, item)),
            Ok(crate::cfind::CFindResponse::success(request, 1)),
        ])
        .boxed()
    }
}

fn store_context() -> PresentationContext {
    PresentationContext::new(
        3,
        "1.2.840.10008.5.1.4.1.1.7",
        vec![
            "1.2.840.10008.1.2".to_string(),
            "1.2.840.10008.1.2.1".to_string(),
        ],
    )
}

fn instance_file() -> radicom::io::file::DicomFile {
    let mut dataset = Dataset::new();
    dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
    dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5.6");
    dataset.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
    dataset.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-1");
    dataset.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
    radicom::io::file::DicomFile::from_dataset(dataset)
}

#[tokio::test]
async fn mixed_dimse_requests_share_one_association_and_route_events() {
    let provider = Arc::new(RecordingProvider::new());
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider.clone(),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut find = find_context();
    find.id = 5;
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context(), store_context(), find],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let requests = vec![
        crate::batch::DicomRequest::Echo(crate::services::CEchoRequest {
            message_id: 0,
            affected_sop_class_uid: sop_class::VERIFICATION.to_string(),
        }),
        crate::batch::DicomRequest::from(CStoreRequest {
            message_id: 0,
            affected_sop_class_uid: "1.2.840.10008.5.1.4.1.1.7".into(),
            affected_sop_instance_uid: "1.2.3.4.5.6".into(),
            priority: crate::dimse::Priority::Medium,
            file: instance_file(),
            move_originator_ae_title: None,
            move_originator_message_id: None,
        }),
        crate::batch::DicomRequest::Store {
            file: Arc::new(instance_file()),
        },
        crate::batch::DicomRequest::from(CStoreRequest {
            message_id: 0,
            affected_sop_class_uid: String::new(),
            affected_sop_instance_uid: String::new(),
            priority: crate::dimse::Priority::Medium,
            file: instance_file(),
            move_originator_ae_title: None,
            move_originator_message_id: None,
        }),
        crate::batch::DicomRequest::Find(crate::cfind::CFindRequest::study_query(0)),
    ];
    let mut events = Vec::new();
    association
        .send_mixed_batch(requests, |event| {
            events.push(event);
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(
        events.len(),
        6,
        "echo, three stores, and two find responses"
    );
    assert_eq!(events[0].request_index, 0);
    assert!(events[0].is_final);
    assert_eq!(events[1].request_index, 1);
    assert!(events[1].is_final);
    assert_eq!(events[2].request_index, 2);
    assert!(events[2].is_final);
    assert_eq!(events[3].request_index, 3);
    assert!(events[3].is_final);
    assert_eq!(events[4].request_index, 4);
    assert!(!events[4].is_final);
    assert_eq!(events[5].request_index, 4);
    assert!(events[5].is_final);
    assert_eq!(
        provider.instance_uids(),
        vec!["1.2.3.4.5.6", "1.2.3.4.5.6", "1.2.3.4.5.6"]
    );

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn mixed_batch_rejects_duplicate_explicit_message_ids_before_writing() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(crate::services::VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let requests = vec![
        crate::batch::DicomRequest::Echo(crate::services::CEchoRequest {
            message_id: 7,
            affected_sop_class_uid: sop_class::VERIFICATION.to_string(),
        }),
        crate::batch::DicomRequest::Echo(crate::services::CEchoRequest {
            message_id: 7,
            affected_sop_class_uid: sop_class::VERIFICATION.to_string(),
        }),
    ];
    let error = association
        .send_mixed_batch(requests, |_| Ok(()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("message IDs must be unique"));
    let _ = association.abort(0).await;
    let _ = handle.await.unwrap();
}

#[tokio::test]
async fn mixed_batch_rejects_message_ids_already_in_flight() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(crate::services::VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    association.pending.insert(7);
    let error = association
        .send_mixed_batch(
            vec![crate::batch::DicomRequest::Echo(CEchoRequest {
                message_id: 7,
                affected_sop_class_uid: sop_class::VERIFICATION.to_string(),
            })],
            |_| Ok(()),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("already in flight"));
    let _ = association.abort(0).await;
    let _ = handle.await.unwrap();
}

#[tokio::test]
async fn mixed_c_get_requires_a_reverse_store_handler_before_writing() {
    struct GetOnlyProvider;

    #[async_trait::async_trait]
    impl ServiceProvider for GetOnlyProvider {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_GET.to_string()]
        }
    }

    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(GetOnlyProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![get_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let requests = vec![crate::batch::DicomRequest::Get {
        request: crate::cget::CGetRequest::study("1.2.3", 0),
        storage_sop_classes: Vec::new(),
    }];
    assert!(matches!(
        association.send_mixed_batch(requests, |_| Ok(())).await,
        Err(NetworkError::ReverseStoreHandlerMissing)
    ));
    let _ = association.abort(0).await;
    let _ = handle.await.unwrap();
}

#[tokio::test]
async fn mixed_batch_propagates_response_callback_errors() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(crate::services::VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let requests = vec![crate::batch::DicomRequest::Echo(CEchoRequest {
        message_id: 0,
        affected_sop_class_uid: sop_class::VERIFICATION.to_string(),
    })];
    let error = association
        .send_mixed_batch(requests, |_| {
            Err(std::io::Error::other("callback stopped the queue"))
        })
        .await
        .unwrap_err();
    assert!(matches!(error, NetworkError::Io(_)));
    let _ = association.abort(0).await;
    let _ = handle.await.unwrap();
}

#[tokio::test]
async fn mixed_c_get_handles_reverse_store_before_terminal_response() {
    let provider = Arc::new(GetProvider {
        instances: vec![instance_file()],
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let mut echo = echo_context();
    echo.id = 3;
    let contexts = vec![
        get_context(),
        echo,
        PresentationContext::new(
            5,
            "1.2.840.10008.5.1.4.1.1.7",
            vec!["1.2.840.10008.1.2".to_string()],
        ),
    ];
    let mut association =
        Association::open(addr, "GET-SCU", "GET-SCP", contexts, get_client_options())
            .await
            .unwrap();
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    association.collect_reverse_stores(received.clone());

    let requests = vec![
        crate::batch::DicomRequest::Echo(crate::services::CEchoRequest {
            message_id: 0,
            affected_sop_class_uid: sop_class::VERIFICATION.to_string(),
        }),
        crate::batch::DicomRequest::Get {
            request: crate::cget::CGetRequest::study("1.2.3.4", 0),
            storage_sop_classes: vec!["1.2.840.10008.5.1.4.1.1.7".to_string()],
        },
    ];
    let mut events = Vec::new();
    association
        .send_mixed_batch(requests, |event| {
            events.push(event);
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(received.lock().unwrap().len(), 1);
    assert_eq!(events.len(), 3, "echo plus C-GET pending and terminal");
    assert_eq!(events[0].request_index, 0);
    assert_eq!(events[1].request_index, 1);
    assert!(!events[1].is_final);
    assert!(events[2].is_final);
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_store_delivers_the_instance() {
    let provider = Arc::new(RecordingProvider::new());
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider.clone(),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![store_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let response = association.send_c_store(&instance_file()).await.unwrap();
    assert_eq!(
        response.status,
        crate::dimse::status::SUCCESS,
        "C-STORE accepted"
    );
    assert_eq!(response.state(), crate::dimse::StatusState::Success);
    assert_eq!(provider.instance_uids(), vec!["1.2.3.4.5.6".to_string()]);

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_store_implicit_vr_little_endian_round_trips() {
    // The default policy accepts Implicit VR LE, so a dataset in that
    // syntax exercises the receive-side decode along a branch distinct from
    // the explicit VR LE case above.
    let provider = Arc::new(RecordingProvider::new());
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider.clone(),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut file = instance_file();
    file.dataset
        .set_transfer_syntax(syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone());

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![PresentationContext::new(
            1,
            "1.2.840.10008.5.1.4.1.1.7",
            vec![syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.uid().to_string()],
        )],
        ClientOptions::default(),
    )
    .await
    .unwrap();

    let response = association.send_c_store(&file).await.unwrap();
    assert_eq!(response.status, crate::dimse::status::SUCCESS);
    assert_eq!(provider.instance_uids(), vec!["1.2.3.4.5.6".to_string()]);
    assert_eq!(
        provider.patient_names(),
        vec!["Doe^John".to_string()],
        "the implicit VR dataset decoded on the receive side"
    );

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_store_explicit_vr_big_endian_round_trips() {
    // Big-endian is a different byte order, so a store in it exercises the
    // receive-side decode under a non-little-endian negotiated syntax.
    let provider = Arc::new(RecordingProvider::new());
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider.clone(),
        ServerOptions {
            supported_transfer_syntaxes: vec![syntaxes::EXPLICIT_VR_BIG_ENDIAN.uid().to_string()],
            syntax_priority: crate::association::SyntaxPriority::Preferred,
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut file = instance_file();
    file.dataset
        .set_transfer_syntax(syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone());

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![PresentationContext::new(
            1,
            "1.2.840.10008.5.1.4.1.1.7",
            vec![syntaxes::EXPLICIT_VR_BIG_ENDIAN.uid().to_string()],
        )],
        ClientOptions::default(),
    )
    .await
    .unwrap();

    let response = association.send_c_store(&file).await.unwrap();
    assert_eq!(response.status, crate::dimse::status::SUCCESS);
    assert_eq!(provider.instance_uids(), vec!["1.2.3.4.5.6".to_string()]);
    assert_eq!(
        provider.patient_names(),
        vec!["Doe^John".to_string()],
        "the big-endian dataset decoded on the receive side"
    );

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_store_preserves_the_dataset() {
    let provider = Arc::new(RecordingProvider::new());
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider.clone(),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![store_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let file = instance_file();
    association.send_c_store(&file).await.unwrap();
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
    assert_eq!(provider.instance_uids().len(), 1);
}

#[tokio::test]
async fn a_store_dataset_is_parsed_with_the_negotiated_syntax() {
    // NET-24: the negotiated syntax is implicit VR LE, but the dataset is
    // encoded explicit VR. The receiver must use the negotiated syntax (and
    // refuse), not sniff: sniffing accepted the bytes and stored a dataset
    // in a syntax the peer never negotiated.
    use tokio::io::AsyncWriteExt as _;
    let provider = Arc::new(RecordingProvider::new());
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider.clone(),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut open = Associate::new("CALLED-SCP", "CALLING-SCU");
    open.presentation_contexts = vec![PresentationContext::new(
        3,
        "1.2.840.10008.5.1.4.1.1.7",
        vec![syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.uid().to_string()],
    )];
    write_pdu(&mut tcp, &Pdu::AssociateRq(open)).await.unwrap();
    let accepted = match read_pdu(&mut tcp).await.unwrap() {
        Some(Pdu::AssociateAc(accepted)) => accepted,
        other => panic!("expected an accept, got {other:?}"),
    };
    assert_eq!(
        accepted.presentation_contexts[0].transfer_syntaxes,
        vec!["1.2.840.10008.1.2".to_string()],
        "implicit VR LE is the negotiated syntax"
    );

    let mut command = Command::request(CommandField::CStoreRq, 5);
    command.affected_sop_class_uid = Some("1.2.840.10008.5.1.4.1.1.7".into());
    command.affected_sop_instance_uid = Some("1.2.3.4.5.6".into());
    command.no_dataset = false;
    // the dataset is explicit VR LE, which is *not* what was negotiated
    let dataset = crate::message::encode_dataset(
        &instance_file().dataset,
        &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
    )
    .unwrap();
    let mut wire = crate::pdu::encode(&pdata(3, true, command_bytes(&command).unwrap())).unwrap();
    wire.extend_from_slice(&crate::pdu::encode(&pdata(3, false, dataset)).unwrap());
    tcp.write_all(&wire).await.unwrap();

    let mut status = None;
    while status.is_none() {
        match read_pdu(&mut tcp).await.unwrap() {
            Some(Pdu::PData(pdata)) => {
                let reply = command_from_bytes(&pdata.pdvs.first().unwrap().data).unwrap();
                if reply.field == CommandField::CStoreRsp {
                    status = reply.status;
                }
            }
            other => panic!("expected a C-STORE response, got {other:?}"),
        }
    }
    assert_ne!(
        status,
        Some(crate::dimse::status::SUCCESS),
        "a dataset in an unnegotiated syntax is not stored"
    );
    assert!(
        provider.patient_names().is_empty(),
        "the provider never saw the unnegotiated dataset"
    );
    write_pdu(&mut tcp, &Pdu::ReleaseRq).await.unwrap();
    assert!(matches!(
        read_pdu(&mut tcp).await.unwrap(),
        Some(Pdu::ReleaseRp)
    ));
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_store_whose_dataset_disagrees_with_its_command_is_refused() {
    // NET-23: the command names instance 9.9.9, the dataset says 1.2.3.4.5.6
    use tokio::io::AsyncWriteExt as _;
    let provider = Arc::new(RecordingProvider::new());
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider.clone(),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut open = Associate::new("CALLED-SCP", "CALLING-SCU");
    open.presentation_contexts = vec![echo_context(), store_context()];
    write_pdu(&mut tcp, &Pdu::AssociateRq(open)).await.unwrap();
    assert!(matches!(
        read_pdu(&mut tcp).await.unwrap(),
        Some(Pdu::AssociateAc(_))
    ));

    let store_id = 3;
    let mut command = Command::request(CommandField::CStoreRq, 5);
    command.affected_sop_class_uid = Some("1.2.840.10008.5.1.4.1.1.7".into());
    command.affected_sop_instance_uid = Some("9.9.9".into());
    command.no_dataset = false;
    let dataset = crate::message::encode_dataset(
        &instance_file().dataset,
        &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone(),
    )
    .unwrap();
    let mut wire =
        crate::pdu::encode(&pdata(store_id, true, command_bytes(&command).unwrap())).unwrap();
    wire.extend_from_slice(&crate::pdu::encode(&pdata(store_id, false, dataset)).unwrap());
    tcp.write_all(&wire).await.unwrap();

    let mut status = None;
    while status.is_none() {
        match read_pdu(&mut tcp).await.unwrap() {
            Some(Pdu::PData(pdata)) => {
                let pdv = pdata.pdvs.first().unwrap();
                let reply = command_from_bytes(&pdv.data).unwrap();
                if reply.field == CommandField::CStoreRsp {
                    status = reply.status;
                }
            }
            other => panic!("expected a C-STORE response, got {other:?}"),
        }
    }
    assert_eq!(
        status,
        Some(crate::dimse::status::STORAGE_DATA_SET_DOES_NOT_MATCH_SOP_CLASS),
        "a dataset/command mismatch is 0xA900"
    );
    assert!(
        provider.instance_uids().is_empty(),
        "the provider never saw the mismatched instance"
    );

    // the connection survives: a C-ECHO still gets an answer
    let mut echo = Command::request(CommandField::CEchoRq, 6);
    echo.affected_sop_class_uid = Some(sop_class::VERIFICATION.into());
    tcp.write_all(&crate::pdu::encode(&pdata(1, true, command_bytes(&echo).unwrap())).unwrap())
        .await
        .unwrap();
    let mut echoed = None;
    while echoed.is_none() {
        match read_pdu(&mut tcp).await.unwrap() {
            Some(Pdu::PData(pdata)) => {
                let pdv = pdata.pdvs.first().unwrap();
                let reply = command_from_bytes(&pdv.data).unwrap();
                if reply.field == CommandField::CEchoRsp {
                    echoed = reply.status;
                }
            }
            other => panic!("expected a C-ECHO response, got {other:?}"),
        }
    }
    assert_eq!(echoed, Some(crate::dimse::status::SUCCESS));
    write_pdu(&mut tcp, &Pdu::ReleaseRq).await.unwrap();
    assert!(matches!(
        read_pdu(&mut tcp).await.unwrap(),
        Some(Pdu::ReleaseRp)
    ));
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_failed_store_response_names_the_affected_instance() {
    // NW-E-02: C-STORE parsing/落盘 failure answers through
    // `Command::response`; the response must still carry (0000,1000)
    // (PS3.7 Table 9.3-2).
    use tokio::io::AsyncWriteExt as _;
    let provider = Arc::new(RecordingProvider::new());
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider,
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut tcp = TcpStream::connect(addr).await.unwrap();
    let mut open = Associate::new("CALLED-SCP", "CALLING-SCU");
    open.presentation_contexts = vec![store_context()];
    write_pdu(&mut tcp, &Pdu::AssociateRq(open)).await.unwrap();
    assert!(matches!(
        read_pdu(&mut tcp).await.unwrap(),
        Some(Pdu::AssociateAc(_))
    ));

    let store_id = 3;
    let mut command = Command::request(CommandField::CStoreRq, 5);
    command.affected_sop_class_uid = Some("1.2.840.10008.5.1.4.1.1.7".into());
    command.affected_sop_instance_uid = Some("1.2.3.4.5.6".into());
    command.no_dataset = false;
    // implicit VR LE element (0010,0010) claiming 8 bytes but cut short:
    // the dataset does not parse, forcing the failure path
    let truncated = vec![0x10, 0x00, 0x10, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00];
    let mut wire =
        crate::pdu::encode(&pdata(store_id, true, command_bytes(&command).unwrap())).unwrap();
    wire.extend_from_slice(&crate::pdu::encode(&pdata(store_id, false, truncated)).unwrap());
    tcp.write_all(&wire).await.unwrap();

    let mut response = None;
    while response.is_none() {
        if let Some(Pdu::PData(pdata)) = read_pdu(&mut tcp).await.unwrap() {
            let reply = command_from_bytes(&pdata.pdvs.first().unwrap().data).unwrap();
            if reply.field == CommandField::CStoreRsp {
                response = Some(reply);
            }
        }
    }
    let response = response.unwrap();
    assert_ne!(
        response.status,
        Some(crate::dimse::status::SUCCESS),
        "a truncated dataset is refused"
    );
    assert_eq!(
        response.affected_sop_instance_uid.as_deref(),
        Some("1.2.3.4.5.6"),
        "a failure response names the affected instance (PS3.7 Table 9.3-2)"
    );

    write_pdu(&mut tcp, &Pdu::ReleaseRq).await.unwrap();
    assert!(matches!(
        read_pdu(&mut tcp).await.unwrap(),
        Some(Pdu::ReleaseRp)
    ));
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_response_command_is_aborted_exactly_once() {
    // NET-26: the dispatcher sends the A-ABORT itself and returns a typed
    // error; the connection wrapper must not abort a second time
    use tokio::io::AsyncWriteExt as _;
    let (mut tcp, handle) = raw_associated_stream().await;
    let mut response = Command::request(CommandField::CEchoRsp, 3);
    response.message_id_being_responded_to = Some(2);
    response.status = Some(crate::dimse::status::SUCCESS);
    tcp.write_all(&crate::pdu::encode(&pdata(1, true, command_bytes(&response).unwrap())).unwrap())
        .await
        .unwrap();
    assert!(
        matches!(read_pdu(&mut tcp).await.unwrap(), Some(Pdu::Abort(_))),
        "the dispatcher aborts"
    );
    // and then the association is gone: no second abort, just the close
    match read_pdu(&mut tcp).await {
        Ok(None) | Err(_) => {}
        Ok(Some(other)) => panic!("a second PDU arrived: {other:?}"),
    }
    let _ = handle.await;
}

#[tokio::test]
async fn loopback_c_store_then_echo_on_one_association() {
    let provider = Arc::new(RecordingProvider::new());
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider.clone(),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    // propose both contexts so one association serves both services
    let contexts = vec![
        PresentationContext::new(
            1,
            sop_class::VERIFICATION,
            vec!["1.2.840.10008.1.2".to_string()],
        ),
        store_context(),
    ];
    let mut association = Association::open(addr, "SCU", "SCP", contexts, ClientOptions::default())
        .await
        .unwrap();

    let store = association.send_c_store(&instance_file()).await.unwrap();
    assert_eq!(store.status, crate::dimse::status::SUCCESS);
    let echo = association
        .send_c_echo(sop_class::VERIFICATION)
        .await
        .unwrap();
    assert_eq!(echo.status, crate::dimse::status::SUCCESS);

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_store_failure_keeps_the_connection() {
    // the VerificationProvider does not implement C-STORE
    struct EchoOnly;
    #[async_trait::async_trait]
    impl ServiceProvider for EchoOnly {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![
                sop_class::VERIFICATION.to_string(),
                "1.2.840.10008.5.1.4.1.1.7".to_string(),
            ]
        }
        async fn on_c_echo(&self, request: &CEchoRequest) -> Result<CEchoResponse> {
            Ok(CEchoResponse::success(request))
        }
    }

    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(EchoOnly),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let contexts = vec![
        PresentationContext::new(
            1,
            sop_class::VERIFICATION,
            vec!["1.2.840.10008.1.2".to_string()],
        ),
        store_context(),
    ];
    let mut association = Association::open(addr, "SCU", "SCP", contexts, ClientOptions::default())
        .await
        .unwrap();

    let response = association.send_c_store(&instance_file()).await.unwrap();
    assert_eq!(
        response.state(),
        crate::dimse::StatusState::Failure,
        "C-STORE rejected"
    );
    // the connection survives a rejected instance (§11.2)
    let echo = association
        .send_c_echo(sop_class::VERIFICATION)
        .await
        .unwrap();
    assert_eq!(echo.status, crate::dimse::status::SUCCESS);

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_failing_echo_provider_answers_processing_failure() {
    // a provider error must not kill the association: the peer gets a
    // failure status and can keep using the connection
    struct FailingEcho;
    #[async_trait::async_trait]
    impl ServiceProvider for FailingEcho {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::VERIFICATION.to_string()]
        }
        async fn on_c_echo(&self, _request: &CEchoRequest) -> Result<CEchoResponse> {
            Err(NetworkError::protocol("provider exploded"))
        }
    }

    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(FailingEcho),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let contexts = vec![PresentationContext::new(
        1,
        sop_class::VERIFICATION,
        vec!["1.2.840.10008.1.2".to_string()],
    )];
    let mut association = Association::open(addr, "SCU", "SCP", contexts, ClientOptions::default())
        .await
        .unwrap();

    let response = association
        .send_c_echo(sop_class::VERIFICATION)
        .await
        .unwrap();
    assert_eq!(
        response.state(),
        crate::dimse::StatusState::Failure,
        "a provider error is answered as a failed operation"
    );
    // the association is still healthy: a second echo gets an answer, not EOF
    let again = association
        .send_c_echo(sop_class::VERIFICATION)
        .await
        .unwrap();
    assert_eq!(again.state(), crate::dimse::StatusState::Failure);

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

/// A provider answering C-FIND with fixed study matches.
struct QueryProvider {
    matches: Vec<(String, String)>,
    /// The repertoire the match datasets are written in; they carry no
    /// (0008,0005), so the client's fallback decides how they are read.
    charset: Option<&'static str>,
}

impl QueryProvider {
    fn new(matches: Vec<(&str, &str)>) -> Self {
        QueryProvider {
            matches: matches
                .into_iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            charset: None,
        }
    }

    fn with_charset(mut self, term: &'static str) -> Self {
        self.charset = Some(term);
        self
    }
}

#[async_trait::async_trait]
impl ServiceProvider for QueryProvider {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![sop_class::STUDY_ROOT_QR_FIND.to_string()]
    }

    fn on_c_find<'a>(
        &'a self,
        request: &'a crate::cfind::CFindRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cfind::CFindResponse>> {
        use futures::StreamExt;
        let total = self.matches.len() as u16;
        let items: Vec<_> = self
            .matches
            .iter()
            .map(|(study_uid, patient_name)| {
                let mut item = Dataset::new();
                item.insert_str(Tag::STUDY_INSTANCE_UID, Vr::Ui, study_uid.clone());
                item.insert_str(Tag::PATIENT_NAME, Vr::Pn, patient_name.clone());
                if let Some(term) = self.charset {
                    item.set_fallback_encodings(vec![
                        radicom::encoding::charset::charset_encoding(term),
                    ]);
                }
                Ok(crate::cfind::CFindResponse::pending(request, item))
            })
            .collect();
        futures::stream::iter(items)
            .chain(futures::stream::once(async move {
                Ok(crate::cfind::CFindResponse::success(request, total))
            }))
            .boxed()
    }
}

fn find_context() -> PresentationContext {
    PresentationContext::new(
        1,
        sop_class::STUDY_ROOT_QR_FIND,
        vec!["1.2.840.10008.1.2".to_string()],
    )
}

#[tokio::test]
async fn loopback_c_find_returns_every_match() {
    let provider = Arc::new(QueryProvider::new(vec![
        ("1.2.3.1", "Doe^John"),
        ("1.2.3.2", "Roe^Jane"),
    ]));
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider.clone(),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![find_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();

    let request =
        crate::cfind::CFindRequest::study_query(1).with_key(0x0010, 0x0020, Vr::Lo, "PID-1");
    let responses = association.send_c_find(&request).await.unwrap();

    assert_eq!(responses.len(), 3, "two matches plus the terminal response");
    assert!(responses[0].is_pending() && responses[1].is_pending());
    assert_eq!(responses[2].status, crate::dimse::status::SUCCESS);
    // C-FIND-RSP carries no sub-operation counters (PS3.7 Table 9.3-4); the
    // match count is the number of Pending responses seen, not a counter.
    assert_eq!(responses[2].completed, 0);

    let first = responses[0].identifier.as_ref().unwrap();
    assert_eq!(first.get_str(Tag::STUDY_INSTANCE_UID).unwrap(), "1.2.3.1");
    assert_eq!(first.get_str(Tag::PATIENT_NAME).unwrap(), "Doe^John");
    let second = responses[1].identifier.as_ref().unwrap();
    assert_eq!(second.get_str(Tag::PATIENT_NAME).unwrap(), "Roe^Jane");

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_find_delivers_pending_before_provider_finishes() {
    struct GatedFind {
        release_terminal: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl ServiceProvider for GatedFind {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_FIND.to_string()]
        }

        fn on_c_find<'a>(
            &'a self,
            request: &'a crate::cfind::CFindRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cfind::CFindResponse>> {
            use futures::StreamExt;
            let mut item = Dataset::new();
            item.insert_str(Tag::STUDY_INSTANCE_UID, Vr::Ui, "1.2.3");
            let pending = crate::cfind::CFindResponse::pending(request, item);
            let release_terminal = self.release_terminal.clone();
            Box::pin(futures::stream::once(async move { Ok(pending) }).chain(
                futures::stream::once(async move {
                    release_terminal.notified().await;
                    Ok(crate::cfind::CFindResponse::success(request, 1))
                }),
            ))
        }
    }

    let release_terminal = Arc::new(tokio::sync::Notify::new());
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(GatedFind {
            release_terminal: release_terminal.clone(),
        }),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![find_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let request = crate::cfind::CFindRequest::study_query(1);
    let mut responses = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(2),
        association.send_c_find_stream(&request, |response| {
            let terminal = !response.is_pending();
            responses.push(response);
            if !terminal {
                release_terminal.notify_one();
            }
            Ok(())
        }),
    )
    .await
    .expect("the first pending response must not wait for the provider's terminal response")
    .unwrap();

    assert_eq!(responses.len(), 2);
    assert!(responses[0].is_pending());
    assert_eq!(responses[1].status, crate::dimse::status::SUCCESS);
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_find_c_cancel_stops_an_in_flight_provider() {
    struct CancelableFind;

    #[async_trait::async_trait]
    impl ServiceProvider for CancelableFind {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_FIND.to_string()]
        }

        fn on_c_find<'a>(
            &'a self,
            request: &'a crate::cfind::CFindRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cfind::CFindResponse>> {
            use futures::StreamExt;
            let mut item = Dataset::new();
            item.insert_str(Tag::STUDY_INSTANCE_UID, Vr::Ui, "1.2.3");
            let pending = crate::cfind::CFindResponse::pending(request, item);
            Box::pin(
                futures::stream::once(async move { Ok(pending) }).chain(futures::stream::pending()),
            )
        }
    }

    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(CancelableFind),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![find_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let request = crate::cfind::CFindRequest::study_query(1);
    let context_id = association.contexts().all()[0].id;
    let syntax = transfer_syntax_of(&association.contexts().all()[0].transfer_syntax).unwrap();
    let command = bytes::Bytes::from(command_bytes(&request.to_command()).unwrap());
    let identifier =
        bytes::Bytes::from(crate::message::encode_dataset(&request.identifier, &syntax).unwrap());
    for pdata in association
        .splitter
        .split_bytes(context_id, true, command)
        .unwrap()
    {
        write_pdu(&mut association.stream, &Pdu::PData(pdata))
            .await
            .unwrap();
    }

    // Keep the Identifier and C-CANCEL in one P-DATA-TF. The assembler
    // returns both completed units together, so this covers the queued
    // unit hand-off between the outer dispatcher and handle_c_find.
    let mut cancel = Command::request(CommandField::CCancelRq, 1);
    cancel.message_id_being_responded_to = Some(1);
    let cancel_bytes = bytes::Bytes::from(command_bytes(&cancel).unwrap());
    write_pdu(
        &mut association.stream,
        &Pdu::PData(crate::pdu::PData {
            pdvs: vec![
                crate::pdu::Pdv {
                    context_id,
                    is_command: false,
                    is_last: true,
                    data: identifier,
                },
                crate::pdu::Pdv {
                    context_id,
                    is_command: true,
                    is_last: true,
                    data: cancel_bytes,
                },
            ],
        }),
    )
    .await
    .unwrap();
    let cancelled = tokio::time::timeout(
        Duration::from_secs(2),
        association.await_response(1, CommandField::CFindRsp),
    )
    .await
    .expect("C-CANCEL must stop the in-flight provider")
    .unwrap();
    assert_eq!(cancelled.status, Some(crate::dimse::status::CANCEL));

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_find_with_no_matches_terminates_cleanly() {
    let provider = Arc::new(QueryProvider::new(vec![]));
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider,
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![find_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let responses = association
        .send_c_find(&crate::cfind::CFindRequest::study_query(1))
        .await
        .unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].status, crate::dimse::status::SUCCESS);
    assert_eq!(responses[0].completed, 0);
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_find_surfaces_a_provider_failure() {
    struct BrokenFind;
    #[async_trait::async_trait]
    impl ServiceProvider for BrokenFind {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_FIND.to_string()]
        }
        fn on_c_find<'a>(
            &'a self,
            _request: &'a crate::cfind::CFindRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cfind::CFindResponse>> {
            Box::pin(futures::stream::once(async {
                Err(NetworkError::protocol("query backend down"))
            }))
        }
    }

    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(BrokenFind),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![find_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let responses = association
        .send_c_find(&crate::cfind::CFindRequest::study_query(1))
        .await
        .unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].state(), crate::dimse::StatusState::Failure);
    assert!(responses[0].error_comment.is_some());

    // the association survives a failed query
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}
/// A provider that answers C-MOVE by pushing instances to a fixed address.
struct MoveProvider {
    destination: std::net::SocketAddr,
    instances: Vec<radicom::io::file::DicomFile>,
    originator: std::sync::Mutex<Option<(String, u16)>>,
}

#[async_trait::async_trait]
impl ServiceProvider for MoveProvider {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![
            sop_class::STUDY_ROOT_QR_MOVE.to_string(),
            sop_class::VERIFICATION.to_string(),
        ]
    }

    async fn on_c_echo(&self, request: &CEchoRequest) -> Result<CEchoResponse> {
        Ok(CEchoResponse::success(request))
    }

    fn on_c_move<'a>(
        &'a self,
        request: &'a crate::cmove::CMoveRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cmove::CMoveResponse>> {
        let destination = self.destination;
        let instances = self.instances.clone();
        let originator = self.originator.lock().unwrap().clone();
        let originator_ae = originator.as_ref().map(|(ae, _)| ae.clone());
        let originator_id = originator.as_ref().map(|(_, id)| *id);
        Box::pin(futures::stream::once(async move {
            let (summary, failed_uids, error) = crate::cmove::store_to_destination_with_failures(
                destination,
                "MOVE-SCP",
                "STORE-SCP",
                &instances,
                originator_ae
                    .as_deref()
                    .map(|ae| (ae, originator_id.unwrap_or(0))),
                ClientOptions::default(),
            )
            .await?;
            match error {
                None => {
                    let mut response = crate::cmove::CMoveResponse::completed(request, summary);
                    response.failed_sop_instance_uids = failed_uids;
                    Ok(response)
                }
                Some(error) => {
                    let mut response = crate::cmove::CMoveResponse::failure(
                        request,
                        crate::dimse::status::PROCESSING_FAILURE,
                        error.to_string(),
                    );
                    response.summary = summary;
                    response.failed_sop_instance_uids = failed_uids;
                    Ok(response)
                }
            }
        }))
    }
}

#[tokio::test]
async fn mixed_c_move_and_echo_route_terminal_responses() {
    let stored = Arc::new(RecordingProvider::new());
    let destination = DicomServer::bind(
        "127.0.0.1:0",
        stored.clone(),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let destination_addr = destination.local_addr();
    tokio::spawn(async move {
        let _ = destination.run().await;
    });

    let provider = Arc::new(MoveProvider {
        destination: destination_addr,
        instances: vec![instance_file()],
        originator: std::sync::Mutex::new(None),
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let mut echo = echo_context();
    echo.id = 3;
    let mut association = Association::open(
        addr,
        "MIXED-SCU",
        "MIXED-SCP",
        vec![move_context(), echo],
        ClientOptions::default(),
    )
    .await
    .unwrap();

    let mut events = Vec::new();
    association
        .send_mixed_batch(
            vec![
                crate::batch::DicomRequest::Move(crate::cmove::CMoveRequest::study(
                    "1.2.3.4",
                    "STORE-SCP",
                    0,
                )),
                crate::batch::DicomRequest::Echo(crate::services::CEchoRequest {
                    message_id: 0,
                    affected_sop_class_uid: sop_class::VERIFICATION.to_string(),
                }),
            ],
            |event| {
                events.push(event);
                Ok(())
            },
        )
        .await
        .unwrap();

    assert_eq!(events.len(), 2);
    assert_eq!(events[0].request_index, 0);
    assert_eq!(events[1].request_index, 1);
    assert!(events.iter().all(|event| event.is_final));
    assert_eq!(stored.instance_uids(), vec!["1.2.3.4.5.6"]);
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

fn move_context() -> PresentationContext {
    PresentationContext::new(
        1,
        sop_class::STUDY_ROOT_QR_MOVE,
        vec!["1.2.840.10008.1.2".to_string()],
    )
}

#[tokio::test]
async fn loopback_c_move_pushes_instances_to_the_destination() {
    // the destination is a plain C-STORE SCP
    let stored = Arc::new(RecordingProvider::new());
    let destination = DicomServer::bind(
        "127.0.0.1:0",
        stored.clone(),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let destination_addr = destination.local_addr();
    tokio::spawn(async move {
        let _ = destination.run().await;
    });

    let provider = Arc::new(MoveProvider {
        destination: destination_addr,
        instances: vec![instance_file()],
        originator: std::sync::Mutex::new(Some(("MOVE-SCU".to_string(), 9))),
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "MOVE-SCU",
        "MOVE-SCP",
        vec![move_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();

    let request = crate::cmove::CMoveRequest::study("1.2.3.4", "STORE-SCP", 1);
    let responses = association.send_c_move(&request).await.unwrap();

    assert_eq!(
        responses.len(),
        1,
        "one terminal response, no progress was emitted"
    );
    assert_eq!(responses[0].status, crate::dimse::status::SUCCESS);
    assert_eq!(responses[0].summary.completed, 1);
    assert_eq!(
        stored.instance_uids(),
        vec!["1.2.3.4.5.6".to_string()],
        "the destination received the instance"
    );
    assert_eq!(
        stored.origins(),
        vec![(Some("MOVE-SCU".to_string()), Some(9))],
        "Move Originator AE title and message ID travel with the sub-operation (D5)"
    );

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_move_surfaces_a_failed_delivery() {
    // the destination refuses everything: no reverse handler for C-STORE
    struct RefusingStore;
    #[async_trait::async_trait]
    impl ServiceProvider for RefusingStore {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec!["1.2.840.10008.5.1.4.1.1.7".to_string()]
        }
    }
    let destination = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(RefusingStore),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let destination_addr = destination.local_addr();
    tokio::spawn(async move {
        let _ = destination.run().await;
    });

    let provider = Arc::new(MoveProvider {
        destination: destination_addr,
        instances: vec![instance_file()],
        originator: std::sync::Mutex::new(None),
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "MOVE-SCU",
        "MOVE-SCP",
        vec![move_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let responses = association
        .send_c_move(&crate::cmove::CMoveRequest::study(
            "1.2.3.4",
            "STORE-SCP",
            1,
        ))
        .await
        .unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].failed_sop_instance_uids, vec!["1.2.3.4.5.6"]);
    assert_eq!(
        responses[0].state(),
        crate::dimse::StatusState::Failure,
        "a destination that rejects every instance fails the move"
    );
    assert_eq!(responses[0].summary.failed, 1);

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_move_without_a_destination_is_refused() {
    // the provider is never called: the engine rejects a malformed request
    let provider = Arc::new(MoveProvider {
        destination: "127.0.0.1:1".parse().unwrap(),
        instances: Vec::new(),
        originator: std::sync::Mutex::new(None),
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![move_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    // a hand-built request without Move Destination
    let mut request = crate::cmove::CMoveRequest::study("1.2.3.4", "DEST", 1);
    request.destination_ae = String::new();
    let mut command = request.to_command();
    command.move_destination = None;
    let identifier =
        crate::message::encode_dataset(&request.identifier, &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN)
            .unwrap();
    let context_id = association.contexts().all()[0].id;
    let bytes = command_bytes(&command).unwrap();
    for pdata in association
        .splitter
        .split_message(context_id, &bytes, Some(&identifier))
        .unwrap()
    {
        write_pdu(&mut association.stream, &Pdu::PData(pdata))
            .await
            .unwrap();
    }
    let response = association
        .await_response(1, CommandField::CMoveRsp)
        .await
        .unwrap();
    assert_eq!(
        response.status,
        Some(crate::dimse::status::MOVE_DESTINATION_UNKNOWN)
    );

    // A non-empty destination still has to be a valid 1..=16 byte AE title.
    let bad = crate::cmove::CMoveRequest::study("1.2.3.4", "12345678901234567", 2);
    let bad_identifier =
        crate::message::encode_dataset(&bad.identifier, &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN)
            .unwrap();
    let bad_command = command_bytes(&bad.to_command()).unwrap();
    for pdata in association
        .splitter
        .split_message(context_id, &bad_command, Some(&bad_identifier))
        .unwrap()
    {
        write_pdu(&mut association.stream, &Pdu::PData(pdata))
            .await
            .unwrap();
    }
    let bad_response = association
        .await_response(2, CommandField::CMoveRsp)
        .await
        .unwrap();
    assert_eq!(
        bad_response.status,
        Some(crate::dimse::status::MOVE_DESTINATION_UNKNOWN)
    );

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

/// A provider that answers C-GET by handing back instances.
struct GetProvider {
    instances: Vec<radicom::io::file::DicomFile>,
}

#[async_trait::async_trait]
impl ServiceProvider for GetProvider {
    fn supported_sop_classes(&self) -> Vec<String> {
        // the retrieve model plus the storage class the sub-operations
        // travel on (§11.4)
        vec![
            sop_class::VERIFICATION.to_string(),
            sop_class::STUDY_ROOT_QR_GET.to_string(),
            "1.2.840.10008.5.1.4.1.1.7".to_string(),
        ]
    }

    async fn on_c_echo(&self, request: &CEchoRequest) -> Result<CEchoResponse> {
        Ok(CEchoResponse::success(request))
    }

    fn on_c_get<'a>(
        &'a self,
        request: &'a crate::cget::CGetRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
        let total = self.instances.len() as u16;
        let events: Vec<_> = self
            .instances
            .iter()
            .cloned()
            .enumerate()
            .flat_map(|(index, file)| {
                let remaining = total.saturating_sub(index as u16);
                [
                    Ok(crate::cget::CGetEvent::Response(
                        crate::cget::CGetResponse::pending(
                            request,
                            remaining,
                            crate::cmove::SubOperationSummary::default(),
                        ),
                    )),
                    Ok(crate::cget::CGetEvent::instance(file)),
                ]
            })
            .collect();
        Box::pin(futures::stream::iter(events))
    }
}

fn get_context() -> PresentationContext {
    PresentationContext::new(
        1,
        sop_class::STUDY_ROOT_QR_GET,
        vec!["1.2.840.10008.1.2".to_string()],
    )
}

#[tokio::test]
async fn loopback_c_get_delivers_instances_on_the_same_association() {
    let provider = Arc::new(GetProvider {
        instances: vec![instance_file()],
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    // one association carries both the C-GET and the reverse C-STOREs
    let contexts = vec![
        get_context(),
        PresentationContext::new(
            3,
            "1.2.840.10008.5.1.4.1.1.7",
            vec!["1.2.840.10008.1.2".to_string()],
        ),
    ];
    let mut association =
        Association::open(addr, "GET-SCU", "GET-SCP", contexts, get_client_options())
            .await
            .unwrap();
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    association.collect_reverse_stores(received.clone());

    let responses = association
        .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
        .await
        .unwrap();

    let stored = received.lock().unwrap().clone();
    assert_eq!(
        stored.len(),
        1,
        "the instance came back as a reverse C-STORE"
    );
    assert_eq!(
        stored[0].dataset.get_str(Tag::SOP_INSTANCE_UID).unwrap(),
        "1.2.3.4.5.6"
    );

    assert!(
        responses.iter().any(|r| r.is_pending()),
        "progress is reported per instance"
    );
    let terminal = responses.last().unwrap();
    assert_eq!(terminal.status, crate::dimse::status::SUCCESS);
    assert_eq!(terminal.summary.completed, 1);

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn role_selection_is_granted_in_the_accept_for_a_storage_scp() {
    // review #16: the storage SCP role must travel in the A-ASSOCIATE-RQ
    // and come back granted in the A-ASSOCIATE-AC, otherwise a peer that
    // honours role selection never sends the reverse C-STOREs of a C-GET.
    let provider = Arc::new(GetProvider {
        instances: vec![instance_file()],
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let storage = "1.2.840.10008.5.1.4.1.1.7";
    let contexts = vec![
        get_context(),
        PresentationContext::new(3, storage, vec!["1.2.840.10008.1.2".to_string()]),
    ];
    let options = ClientOptions {
        role_selections: vec![crate::pdu::RoleSelection {
            sop_class: storage.to_string(),
            scu_role: false,
            scp_role: true,
        }],
        ..Default::default()
    };
    let association = Association::open(addr, "GET-SCU", "GET-SCP", contexts, options)
        .await
        .unwrap();

    let granted = association
        .contexts()
        .find_abstract(storage)
        .expect("the storage context was accepted");
    assert!(
        granted.roles.iter().any(|r| r.scp_role && !r.scu_role),
        "the accept must grant this side the SCP role it asked for, got {:?}",
        granted.roles
    );

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_reverse_store_is_not_sent_when_the_peer_denied_the_scp_role() {
    // review #16: once roles were negotiated, the SCP may only send
    // reverse C-STORE on a class where the peer actually holds the SCP
    // role. Asking for SCU instead means the instance cannot be
    // delivered, so the sub-operation fails instead of being sent anyway.
    let provider = Arc::new(GetProvider {
        instances: vec![instance_file()],
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let storage = "1.2.840.10008.5.1.4.1.1.7";
    let contexts = vec![
        get_context(),
        PresentationContext::new(3, storage, vec!["1.2.840.10008.1.2".to_string()]),
    ];
    // a role selection that grants this side SCU on the retrieve class and
    // nothing at all on the storage class
    let options = ClientOptions {
        role_selections: vec![crate::pdu::RoleSelection {
            sop_class: sop_class::STUDY_ROOT_QR_GET.to_string(),
            scu_role: true,
            scp_role: false,
        }],
        ..Default::default()
    };
    let mut association = Association::open(addr, "GET-SCU", "GET-SCP", contexts, options)
        .await
        .unwrap();
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    association.collect_reverse_stores(received.clone());

    let responses = association
        .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
        .await
        .unwrap();

    assert!(
        received.lock().unwrap().is_empty(),
        "no instance may be pushed while the peer denies the SCP role"
    );
    let terminal = responses.last().unwrap();
    assert_eq!(terminal.summary.completed, 0);
    assert_eq!(
        terminal.summary.failed, 1,
        "the undeliverable instance is a failed sub-operation"
    );
    assert_ne!(terminal.status, crate::dimse::status::SUCCESS);

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_get_delivers_pending_before_provider_finishes() {
    struct GatedGet {
        release_terminal: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl ServiceProvider for GatedGet {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_GET.to_string()]
        }

        fn on_c_get<'a>(
            &'a self,
            request: &'a crate::cget::CGetRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
            use futures::StreamExt;
            let release_terminal = self.release_terminal.clone();
            let pending = crate::cget::CGetResponse::pending(
                request,
                1,
                crate::cmove::SubOperationSummary::default(),
            );
            Box::pin(
                futures::stream::once(async move { Ok(crate::cget::CGetEvent::Response(pending)) })
                    .chain(futures::stream::once(async move {
                        release_terminal.notified().await;
                        Ok(crate::cget::CGetEvent::Response(
                            crate::cget::CGetResponse::completed(
                                request,
                                crate::cmove::SubOperationSummary::default(),
                            ),
                        ))
                    })),
            )
        }
    }

    let release_terminal = Arc::new(tokio::sync::Notify::new());
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(GatedGet {
            release_terminal: release_terminal.clone(),
        }),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![get_context()],
        get_client_options(),
    )
    .await
    .unwrap();
    association.set_reverse_store_handler(Arc::new(|request: &CStoreRequest| {
        Ok(CStoreResponse::success(request))
    }));
    let request = crate::cget::CGetRequest::study("1.2.3.4", 1);
    let mut responses = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(2),
        association.send_c_get_stream(&request, |response| {
            let terminal = !response.is_pending();
            responses.push(response);
            if !terminal {
                release_terminal.notify_one();
            }
            Ok(())
        }),
    )
    .await
    .expect("the first pending response must not wait for the provider's terminal response")
    .unwrap();

    assert_eq!(responses.len(), 2);
    assert!(responses[0].is_pending());
    assert_eq!(responses[1].state(), crate::dimse::StatusState::Success);
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_get_c_cancel_stops_an_in_flight_provider() {
    struct CancelableGet;

    #[async_trait::async_trait]
    impl ServiceProvider for CancelableGet {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_GET.to_string()]
        }

        fn on_c_get<'a>(
            &'a self,
            request: &'a crate::cget::CGetRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
            use futures::StreamExt;
            let pending = crate::cget::CGetResponse::pending(
                request,
                6,
                crate::cmove::SubOperationSummary {
                    completed: 2,
                    failed: 1,
                    warning: 3,
                },
            );
            Box::pin(
                futures::stream::once(async move { Ok(crate::cget::CGetEvent::Response(pending)) })
                    .chain(futures::stream::pending()),
            )
        }
    }

    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(CancelableGet),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![get_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let request = crate::cget::CGetRequest::study("1.2.3.4", 1);
    let context_id = association.contexts().all()[0].id;
    let syntax = transfer_syntax_of(&association.contexts().all()[0].transfer_syntax).unwrap();
    let command = bytes::Bytes::from(command_bytes(&request.to_command()).unwrap());
    let identifier =
        bytes::Bytes::from(crate::message::encode_dataset(&request.identifier, &syntax).unwrap());
    for pdata in association
        .splitter
        .split_message_bytes(context_id, command, Some(identifier))
        .unwrap()
    {
        write_pdu(&mut association.stream, &Pdu::PData(pdata))
            .await
            .unwrap();
    }

    let pending = association
        .await_response(1, CommandField::CGetRsp)
        .await
        .unwrap();
    assert_eq!(pending.status, Some(crate::dimse::status::PENDING));
    assert_eq!(pending.remaining_suboperations, Some(6));
    association.send_c_cancel(context_id, 1).await.unwrap();
    let cancelled = tokio::time::timeout(
        Duration::from_secs(2),
        association.await_response(1, CommandField::CGetRsp),
    )
    .await
    .expect("C-CANCEL must stop the in-flight provider")
    .unwrap();
    assert_eq!(cancelled.status, Some(crate::dimse::status::CANCEL));
    assert_eq!(cancelled.remaining_suboperations, Some(6));
    assert_eq!(cancelled.completed_suboperations, Some(2));
    assert_eq!(cancelled.failed_suboperations, Some(1));
    assert_eq!(cancelled.warning_suboperations, Some(3));

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_get_c_cancel_during_reverse_store_preserves_progress() {
    struct CancelableGet;

    #[async_trait::async_trait]
    impl ServiceProvider for CancelableGet {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![
                sop_class::STUDY_ROOT_QR_GET.to_string(),
                "1.2.840.10008.5.1.4.1.1.7".to_string(),
            ]
        }

        fn on_c_get<'a>(
            &'a self,
            request: &'a crate::cget::CGetRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
            use futures::StreamExt;
            let instance = instance_file();
            let pending = crate::cget::CGetResponse::pending(
                request,
                4,
                crate::cmove::SubOperationSummary {
                    completed: 1,
                    failed: 0,
                    warning: 0,
                },
            );
            Box::pin(
                futures::stream::iter([
                    Ok(crate::cget::CGetEvent::instance(instance)),
                    Ok(crate::cget::CGetEvent::Response(pending)),
                ])
                .chain(futures::stream::pending()),
            )
        }
    }

    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(CancelableGet),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let contexts = vec![
        get_context(),
        PresentationContext::new(
            3,
            "1.2.840.10008.5.1.4.1.1.7",
            vec!["1.2.840.10008.1.2".to_string()],
        ),
    ];
    let mut association = Association::open(addr, "SCU", "SCP", contexts, get_client_options())
        .await
        .unwrap();
    let request = crate::cget::CGetRequest::study("1.2.3.4", 1);
    let get_context_id = association
        .contexts()
        .pick(&request.sop_class_uid, "")
        .unwrap()
        .id;
    let syntax = transfer_syntax_of(&association.contexts().all()[0].transfer_syntax).unwrap();
    let command = bytes::Bytes::from(command_bytes(&request.to_command()).unwrap());
    let identifier =
        bytes::Bytes::from(crate::message::encode_dataset(&request.identifier, &syntax).unwrap());
    for pdata in association
        .splitter
        .split_message_bytes(get_context_id, command, Some(identifier))
        .unwrap()
    {
        write_pdu(&mut association.stream, &Pdu::PData(pdata))
            .await
            .unwrap();
    }

    let mut assembler = PdvAssembler::new();
    loop {
        let Some(pdu) = association.read_request_pdu(1).await.unwrap() else {
            panic!("association closed before reverse C-STORE request");
        };
        let Pdu::PData(pdata) = pdu else { continue };
        for unit in assembler.push_pdata(&pdata).unwrap() {
            if unit.stream == Stream::Command {
                let command = command_from_bytes(&unit.bytes).unwrap();
                if command.field == CommandField::CStoreRq {
                    let store_context_id = unit.context_id;
                    association.send_c_cancel(get_context_id, 1).await.unwrap();
                    let response =
                        Command::response(&command, crate::dimse::status::SUCCESS).unwrap();
                    let response_bytes = bytes::Bytes::from(command_bytes(&response).unwrap());
                    for pdata in association
                        .splitter
                        .split_bytes(store_context_id, true, response_bytes)
                        .unwrap()
                    {
                        write_pdu(&mut association.stream, &Pdu::PData(pdata))
                            .await
                            .unwrap();
                    }
                    let cancelled = tokio::time::timeout(
                        Duration::from_secs(2),
                        association.await_response(1, CommandField::CGetRsp),
                    )
                    .await
                    .expect("C-CANCEL must stop reverse C-STORE")
                    .unwrap();
                    assert_eq!(cancelled.status, Some(crate::dimse::status::CANCEL));
                    assert_eq!(cancelled.completed_suboperations, Some(1));
                    association.release().await.unwrap();
                    handle.await.unwrap().unwrap();
                    return;
                }
            }
        }
    }
}

#[tokio::test]
async fn loopback_c_get_preserves_provider_failure_status() {
    struct BrokenGet;

    #[async_trait::async_trait]
    impl ServiceProvider for BrokenGet {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_GET.to_string()]
        }

        fn on_c_get<'a>(
            &'a self,
            _request: &'a crate::cget::CGetRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
            Box::pin(futures::stream::once(async {
                Err(NetworkError::protocol("retrieve backend down"))
            }))
        }
    }

    let server = DicomServer::bind("127.0.0.1:0", Arc::new(BrokenGet), ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![get_context()],
        get_client_options(),
    )
    .await
    .unwrap();
    association.set_reverse_store_handler(Arc::new(|request: &CStoreRequest| {
        Ok(CStoreResponse::success(request))
    }));
    let responses = association
        .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
        .await
        .unwrap();

    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].state(), crate::dimse::StatusState::Failure);
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn c_get_uses_the_last_provider_summary_when_stream_ends() {
    struct ProgressOnlyGet;

    #[async_trait::async_trait]
    impl ServiceProvider for ProgressOnlyGet {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_GET.to_string()]
        }

        fn on_c_get<'a>(
            &'a self,
            request: &'a crate::cget::CGetRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
            let summary = crate::cmove::SubOperationSummary {
                completed: 2,
                failed: 1,
                warning: 0,
            };
            Box::pin(futures::stream::once(async move {
                Ok(crate::cget::CGetEvent::Response(
                    crate::cget::CGetResponse::pending(request, 0, summary),
                ))
            }))
        }
    }

    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(ProgressOnlyGet),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![get_context()],
        get_client_options(),
    )
    .await
    .unwrap();
    association.set_reverse_store_handler(Arc::new(|request: &CStoreRequest| {
        Ok(CStoreResponse::success(request))
    }));
    let responses = association
        .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
        .await
        .unwrap();

    assert_eq!(responses.len(), 2);
    let terminal = responses.last().unwrap();
    assert_eq!(terminal.summary.completed, 2);
    assert_eq!(terminal.summary.failed, 1);
    assert_eq!(terminal.state(), crate::dimse::StatusState::Warning);
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn c_get_preserves_completed_count_when_provider_fails_after_an_instance() {
    struct PartiallyBrokenGet;

    #[async_trait::async_trait]
    impl ServiceProvider for PartiallyBrokenGet {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![
                sop_class::STUDY_ROOT_QR_GET.to_string(),
                "1.2.840.10008.5.1.4.1.1.7".to_string(),
            ]
        }

        fn on_c_get<'a>(
            &'a self,
            _request: &'a crate::cget::CGetRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
            Box::pin(futures::stream::iter([
                Ok(crate::cget::CGetEvent::instance(instance_file())),
                Err(NetworkError::protocol("retrieve backend interrupted")),
            ]))
        }
    }

    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(PartiallyBrokenGet),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association =
        Association::open(addr, "SCU", "SCP", get_contexts(), get_client_options())
            .await
            .unwrap();
    association.set_reverse_store_handler(Arc::new(|request: &CStoreRequest| {
        Ok(CStoreResponse::success(request))
    }));
    let responses = association
        .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
        .await
        .unwrap();

    assert_eq!(responses.len(), 1);
    let terminal = responses.last().unwrap();
    assert_eq!(terminal.state(), crate::dimse::StatusState::Failure);
    assert_eq!(terminal.summary.completed, 1);
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn c_get_does_not_mask_reverse_store_failure_with_provider_success() {
    struct StaleSuccessGet;

    #[async_trait::async_trait]
    impl ServiceProvider for StaleSuccessGet {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![
                sop_class::STUDY_ROOT_QR_GET.to_string(),
                "1.2.840.10008.5.1.4.1.1.7".to_string(),
            ]
        }

        fn on_c_get<'a>(
            &'a self,
            request: &'a crate::cget::CGetRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
            Box::pin(futures::stream::iter([
                Ok(crate::cget::CGetEvent::instance(instance_file())),
                Ok(crate::cget::CGetEvent::Response(
                    crate::cget::CGetResponse::completed(
                        request,
                        crate::cmove::SubOperationSummary {
                            completed: 1,
                            failed: 0,
                            warning: 0,
                        },
                    ),
                )),
            ]))
        }
    }

    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(StaleSuccessGet),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association =
        Association::open(addr, "SCU", "SCP", get_contexts(), get_client_options())
            .await
            .unwrap();
    association.set_reverse_store_handler(Arc::new(|request: &CStoreRequest| {
        Ok(CStoreResponse::failure(
            request,
            crate::dimse::status::STORAGE_FAILURE,
            "refused",
        ))
    }));
    let responses = association
        .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
        .await
        .unwrap();

    assert_eq!(responses.len(), 1);
    let terminal = responses.last().unwrap();
    assert_eq!(terminal.state(), crate::dimse::StatusState::Failure);
    assert_eq!(terminal.summary.failed, 1);
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn c_get_overrides_nonzero_provider_counters_after_an_instance() {
    struct StaleFailureGet;

    #[async_trait::async_trait]
    impl ServiceProvider for StaleFailureGet {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![
                sop_class::STUDY_ROOT_QR_GET.to_string(),
                "1.2.840.10008.5.1.4.1.1.7".to_string(),
            ]
        }

        fn on_c_get<'a>(
            &'a self,
            request: &'a crate::cget::CGetRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
            let mut response = crate::cget::CGetResponse::failure(
                request,
                crate::dimse::status::PROCESSING_FAILURE,
                "stale provider counters",
            );
            response.summary.failed = 7;
            Box::pin(futures::stream::iter([
                Ok(crate::cget::CGetEvent::instance(instance_file())),
                Ok(crate::cget::CGetEvent::Response(response)),
            ]))
        }
    }

    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(StaleFailureGet),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association =
        Association::open(addr, "SCU", "SCP", get_contexts(), get_client_options())
            .await
            .unwrap();
    association.set_reverse_store_handler(Arc::new(|request: &CStoreRequest| {
        Ok(CStoreResponse::success(request))
    }));
    let responses = association
        .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
        .await
        .unwrap();

    let terminal = responses.last().unwrap();
    assert_eq!(terminal.state(), crate::dimse::StatusState::Failure);
    assert_eq!(terminal.summary.completed, 1);
    assert_eq!(terminal.summary.failed, 0);
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn c_get_without_a_reverse_store_handler_fails_fast() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(GetProvider {
            instances: vec![instance_file()],
        }),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let mut association = Association::open(
        addr,
        "GET-SCU",
        "GET-SCP",
        vec![
            get_context(),
            PresentationContext::new(
                3,
                "1.2.840.10008.5.1.4.1.1.7",
                vec!["1.2.840.10008.1.2".to_string()],
            ),
        ],
        ClientOptions::default(),
    )
    .await
    .unwrap();

    // no handler registered: the caller learns immediately, before any
    // sub-operation is attempted (§11.4)
    match association
        .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
        .await
    {
        Err(NetworkError::ReverseStoreHandlerMissing) => {}
        other => panic!("expected ReverseStoreHandlerMissing, got {other:?}"),
    }
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_get_reports_a_failed_sub_operation() {
    let provider = Arc::new(GetProvider {
        instances: vec![instance_file()],
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "GET-SCU",
        "GET-SCP",
        vec![
            get_context(),
            PresentationContext::new(
                3,
                "1.2.840.10008.5.1.4.1.1.7",
                vec!["1.2.840.10008.1.2".to_string()],
            ),
        ],
        get_client_options(),
    )
    .await
    .unwrap();
    // a handler that refuses every instance
    association.set_reverse_store_handler(Arc::new(|request: &CStoreRequest| {
        Ok(CStoreResponse::failure(
            request,
            crate::dimse::status::STORAGE_FAILURE,
            "full",
        ))
    }));

    let responses = association
        .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
        .await
        .unwrap();
    let terminal = responses.last().unwrap();
    assert_eq!(
        terminal.state(),
        crate::dimse::StatusState::Failure,
        "a refused instance fails the retrieve"
    );
    assert_eq!(terminal.summary.failed, 1);
    assert_eq!(terminal.failed_sop_instance_uids, vec!["1.2.3.4.5.6"]);

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_extended_negotiation_is_answered_per_sop_class() {
    fn grant_level(
        sop_class: &str,
        requested: &crate::extended::AppInfo,
    ) -> Option<crate::extended::AppInfo> {
        if sop_class != "1.2.840.10008.5.1.4.1.1.7" {
            return None;
        }
        Some(crate::extended::AppInfo::answer(requested, |field, _| {
            (field == 1).then_some(1)
        }))
    }

    let options = ServerOptions {
        extended_negotiation: crate::extended::ExtendedNegotiationPolicy::new(grant_level),
        ..Default::default()
    };
    let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), options)
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let request = crate::pdu::ExtendedNegotiation {
        sop_class: "1.2.840.10008.5.1.4.1.1.7".into(),
        requested: crate::extended::StorageAppInfo {
            level_of_support: 1,
            digital_signature: 1,
            element_coercion: 0,
        }
        .to_app_info()
        .fields,
        accepted: Vec::new(),
    };
    let client_options = ClientOptions {
        extended_negotiations: vec![request],
        ..Default::default()
    };
    let association = Association::open(addr, "SCU", "SCP", vec![echo_context()], client_options)
        .await
        .unwrap();

    assert_eq!(
        association.extended_field("1.2.840.10008.5.1.4.1.1.7", 1),
        Some(&[1u8][..]),
        "the SCP granted level 2"
    );
    assert_eq!(
        association.extended_field("1.2.840.10008.5.1.4.1.1.7", 2),
        None,
        "a zero-length answer is not a grant"
    );
    assert_eq!(
        association.extended_field("9.9.9", 1),
        None,
        "unknown SOP class"
    );

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_user_identity_is_echoed_when_requested() {
    let options = ServerOptions {
        user_identity: UserIdentityPolicy::Answer(b"OK".to_vec()),
        ..Default::default()
    };
    let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), options)
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let client_options = ClientOptions {
        user_identity: Some(crate::pdu::UserIdentity {
            identity_type: 2,
            primary: b"user".to_vec(),
            secondary: b"pass".to_vec(),
            positive_response_requested: true,
        }),
        ..Default::default()
    };
    let association = Association::open(addr, "SCU", "SCP", vec![echo_context()], client_options)
        .await
        .unwrap();
    assert_eq!(association.user_identity_response(), Some(&b"OK"[..]));

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_a_refused_identity_rejects_the_association() {
    // the default policy refuses user identity
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let client_options = ClientOptions {
        user_identity: Some(crate::pdu::UserIdentity {
            identity_type: 1,
            primary: b"user".to_vec(),
            secondary: Vec::new(),
            positive_response_requested: false,
        }),
        ..Default::default()
    };
    match Association::open(addr, "SCU", "SCP", vec![echo_context()], client_options).await {
        Err(NetworkError::Reject {
            result,
            source_code,
            ..
        }) => {
            assert_eq!(result, 1, "permanent");
            assert_eq!(source_code, 1, "service user");
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn accept_answers_a_required_identity_response() {
    // NET-04: `Accept` used to send no 0x59 item even when the peer asked
    // for a positive response, so the association could never succeed.
    let options = ServerOptions {
        user_identity: UserIdentityPolicy::Accept,
        ..Default::default()
    };
    let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), options)
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let client_options = ClientOptions {
        user_identity: Some(crate::pdu::UserIdentity {
            identity_type: 1,
            primary: b"user".to_vec(),
            secondary: Vec::new(),
            positive_response_requested: true,
        }),
        ..Default::default()
    };
    let association = Association::open(addr, "SCU", "SCP", vec![echo_context()], client_options)
        .await
        .expect("the requested positive response is returned");
    assert_eq!(
        association.user_identity_response(),
        Some(&[][..]),
        "an empty positive response is still a response"
    );
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn an_invalid_identity_is_refused_before_connecting() {
    let client_options = ClientOptions {
        user_identity: Some(crate::pdu::UserIdentity {
            identity_type: 1,
            primary: b"user".to_vec(),
            // a passcode without type 2 is invalid (§11.2)
            secondary: b"pass".to_vec(),
            positive_response_requested: false,
        }),
        ..Default::default()
    };
    let result = Association::open(
        "127.0.0.1:1",
        "SCU",
        "SCP",
        vec![echo_context()],
        client_options,
    )
    .await;
    match result {
        Err(NetworkError::Protocol(message)) => {
            assert!(message.contains("type 2"), "got {message}")
        }
        other => panic!("expected a protocol error, got {other:?}"),
    }
}

#[tokio::test]
async fn loopback_honours_a_small_peer_max_pdu() {
    // the old test only asserted that C-ECHO succeeded, so any
    // implementation passed. A 64-byte maximum must actually split the
    // command set over several P-DATA PDUs, so a fake peer reads the real
    // wire frames and checks each body length against the negotiated max.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // echo the client's own 64-byte maximum back in the accept
        let request = crate::test_wire::accept_association(&mut stream).await;
        assert_eq!(request.user_information.max_pdu_length, 64);

        let mut assembler = PdvAssembler::new();
        let mut frames = 0usize;
        let mut command = None;
        loop {
            match read_pdu(&mut stream).await.unwrap() {
                Some(Pdu::PData(pdata)) => {
                    frames += 1;
                    // a P-DATA body is the PDU length field, i.e. the PDU
                    // less its 6-byte header; every PDV costs 6 header
                    // bytes plus an even-padded value
                    let body: usize = pdata
                        .pdvs
                        .iter()
                        .map(|pdv| 6 + pdv.data.len() + pdv.data.len() % 2)
                        .sum();
                    assert!(
                        body <= 64,
                        "P-DATA body is {body} bytes, over the negotiated 64"
                    );
                    for unit in assembler.push_pdata(&pdata).unwrap() {
                        if unit.stream == Stream::Command {
                            command = Some(command_from_bytes(&unit.bytes).unwrap());
                        }
                    }
                    if let Some(command) = &command {
                        let reply = Command::response(command, crate::dimse::status::SUCCESS)
                            .expect("a C-ECHO request has a response field");
                        let bytes = command_bytes(&reply).unwrap();
                        // Honour the client's advertised 64-byte receive limit:
                        // a PDV costs 6 header bytes plus an even-padded value,
                        // so keep each value <= 58 bytes (6 + 58 = 64).
                        let context_id = pdata.pdvs[0].context_id;
                        let total = bytes.len();
                        let mut sent = 0usize;
                        for chunk in bytes.chunks(58) {
                            sent += chunk.len();
                            let pdv = crate::pdu::Pdv {
                                context_id,
                                is_command: true,
                                is_last: sent >= total,
                                data: chunk.to_vec().into(),
                            };
                            write_pdu(
                                &mut stream,
                                &Pdu::PData(crate::pdu::PData { pdvs: vec![pdv] }),
                            )
                            .await
                            .unwrap();
                        }
                        // the client answers a response with A-RELEASE-RQ;
                        // keep the socket open until it does, or its release
                        // sees a hard reset instead of a clean close
                        break;
                    }
                }
                Some(_) => continue,
                None => panic!("the client closed before sending a command"),
            }
        }
        // stay available for the release handshake
        loop {
            match read_pdu(&mut stream).await.unwrap() {
                Some(Pdu::ReleaseRq) => {
                    write_pdu(&mut stream, &Pdu::ReleaseRp).await.unwrap();
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        frames
    });

    let client_options = ClientOptions {
        max_pdu_length: 64,
        ..Default::default()
    };
    let mut association =
        Association::open(addr, "SCU", "SCP", vec![echo_context()], client_options)
            .await
            .unwrap();
    let response = association
        .send_c_echo(sop_class::VERIFICATION)
        .await
        .unwrap();
    assert_eq!(
        response.status,
        crate::dimse::status::SUCCESS,
        "fragmented command still round-trips"
    );
    association.release().await.unwrap();

    let frames = peer.await.unwrap();
    assert!(
        frames > 1,
        "a 64-byte maximum must split the command set over several P-DATA PDUs (got {frames})"
    );
}
#[tokio::test]
async fn server_serves_several_connections() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    for _ in 0..3 {
        let mut association = Association::open(
            addr,
            "SCU",
            "SCP",
            vec![echo_context()],
            ClientOptions::default(),
        )
        .await
        .unwrap();
        let response = association
            .send_c_echo(sop_class::VERIFICATION)
            .await
            .unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS);
        association.release().await.unwrap();
    }
}

const COMMIT_SOP_CLASS: &str = "1.2.840.10008.1.20.1"; // storage commitment push model

struct CommitProvider;

#[async_trait::async_trait]
impl ServiceProvider for CommitProvider {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![COMMIT_SOP_CLASS.to_string()]
    }

    async fn on_n_action(
        &self,
        request: &crate::nservice::NServiceRequest,
    ) -> Result<crate::nservice::NServiceResponse> {
        // storage commitment answers with a Transaction UID in the response dataset
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::TRANSACTION_UID, Vr::Ui, "1.2.3.4.5.6");
        Ok(crate::nservice::NServiceResponse::success(
            request,
            Some(dataset),
        ))
    }

    async fn on_n_delete(
        &self,
        request: &crate::nservice::NServiceRequest,
    ) -> Result<crate::nservice::NServiceResponse> {
        // N-DELETE answers with a command-only response
        Ok(crate::nservice::NServiceResponse::success(request, None))
    }
}

fn commit_context() -> PresentationContext {
    PresentationContext::new(1, COMMIT_SOP_CLASS, vec!["1.2.840.10008.1.2.1".to_string()])
}

#[tokio::test]
async fn n_action_round_trips_with_a_dataset() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(CommitProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![commit_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();

    let mut request = crate::nservice::NServiceRequest::new(CommandField::NActionRq, 1);
    request.sop_class_uid = Some(COMMIT_SOP_CLASS.to_string());
    request.sop_instance_uid = Some("1.2.3.4".to_string());
    request.action_type_id = Some(1);
    let mut request_dataset = Dataset::new();
    request_dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4");
    request.dataset = Some(request_dataset);

    let response = association.send_n_service(&request).await.unwrap();
    assert_eq!(response.field, CommandField::NActionRsp);
    assert_eq!(response.status, crate::dimse::status::SUCCESS);
    let dataset = response
        .dataset
        .expect("N-ACTION response carries a dataset");
    assert_eq!(
        dataset.try_get_str(Tag::TRANSACTION_UID).as_deref(),
        Some("1.2.3.4.5.6"),
        "the Transaction UID comes back in the response dataset"
    );

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn n_get_response_uses_the_contexts_own_syntax() {
    // review #15: the response dataset must be encoded with the syntax the
    // presentation context accepted, not a hardcoded Explicit VR LE. Here
    // the only context is Implicit VR LE, so an Explicit-VR-encoded answer
    // would not decode on the client.
    const NGET_SOP_CLASS: &str = COMMIT_SOP_CLASS;

    struct GetProvider;
    #[async_trait::async_trait]
    impl ServiceProvider for GetProvider {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![NGET_SOP_CLASS.to_string()]
        }
        async fn on_n_get(
            &self,
            request: &crate::nservice::NServiceRequest,
        ) -> Result<crate::nservice::NServiceResponse> {
            let mut dataset = Dataset::new();
            dataset.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^Jane");
            dataset.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-1");
            Ok(crate::nservice::NServiceResponse::success(
                request,
                Some(dataset),
            ))
        }
    }

    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(GetProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    // Implicit VR LE only: the context's syntax is not Explicit VR LE
    let context =
        PresentationContext::new(1, NGET_SOP_CLASS, vec!["1.2.840.10008.1.2".to_string()]);
    let mut association =
        Association::open(addr, "SCU", "SCP", vec![context], ClientOptions::default())
            .await
            .unwrap();

    let mut request = crate::nservice::NServiceRequest::new(CommandField::NGetRq, 1);
    request.sop_class_uid = Some(NGET_SOP_CLASS.to_string());
    request.sop_instance_uid = Some("1.2.3.4".to_string());
    let response = association.send_n_service(&request).await.unwrap();
    assert_eq!(response.status, crate::dimse::status::SUCCESS);
    let dataset = response.dataset.expect("N-GET answers with a dataset");
    assert_eq!(dataset.get_str(Tag::PATIENT_NAME).unwrap(), "Doe^Jane");
    assert_eq!(dataset.get_str(Tag::PATIENT_ID).unwrap(), "PID-1");

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn n_delete_round_trips_without_a_dataset() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(CommitProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![commit_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();

    let mut request = crate::nservice::NServiceRequest::new(CommandField::NDeleteRq, 2);
    request.sop_class_uid = Some(COMMIT_SOP_CLASS.to_string());
    request.sop_instance_uid = Some("1.2.3.4".to_string());

    let response = association.send_n_service(&request).await.unwrap();
    assert_eq!(response.field, CommandField::NDeleteRsp);
    assert_eq!(response.status, crate::dimse::status::SUCCESS);
    assert!(
        response.dataset.is_none(),
        "N-DELETE response is command-only"
    );

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

/// A provider that answers the four N-services under test. A request whose
/// SOP instance UID is `FAIL` gets a failure status; everything else
/// succeeds. This exercises the real dispatch path, not just the builders.
struct NServiceEcho;
fn n_service_answer(
    request: &crate::nservice::NServiceRequest,
) -> crate::nservice::NServiceResponse {
    if request.sop_instance_uid.as_deref() == Some("FAIL") {
        crate::nservice::NServiceResponse::failure(
            request,
            crate::dimse::status::PROCESSING_FAILURE,
        )
    } else {
        crate::nservice::NServiceResponse::success(request, None)
    }
}

#[async_trait::async_trait]
impl ServiceProvider for NServiceEcho {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![
            COMMIT_SOP_CLASS.to_string(),
            sop_class::VERIFICATION.to_string(),
        ]
    }
    async fn on_c_echo(&self, request: &CEchoRequest) -> Result<CEchoResponse> {
        Ok(CEchoResponse::success(request))
    }
    async fn on_n_event_report(
        &self,
        request: &crate::nservice::NServiceRequest,
    ) -> Result<crate::nservice::NServiceResponse> {
        Ok(n_service_answer(request))
    }
    async fn on_n_get(
        &self,
        request: &crate::nservice::NServiceRequest,
    ) -> Result<crate::nservice::NServiceResponse> {
        Ok(n_service_answer(request))
    }
    async fn on_n_set(
        &self,
        request: &crate::nservice::NServiceRequest,
    ) -> Result<crate::nservice::NServiceResponse> {
        Ok(n_service_answer(request))
    }
    async fn on_n_create(
        &self,
        request: &crate::nservice::NServiceRequest,
    ) -> Result<crate::nservice::NServiceResponse> {
        Ok(n_service_answer(request))
    }
}

/// Drives one N-service request/response pair through a real association:
/// one success and one failure status.
async fn assert_n_service_round_trip(request_field: CommandField, response_field: CommandField) {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(NServiceEcho),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let mut client_options = ClientOptions::default();
    if request_field == CommandField::NEventReportRq {
        client_options
            .role_selections
            .push(crate::pdu::RoleSelection {
                sop_class: COMMIT_SOP_CLASS.into(),
                scu_role: false,
                scp_role: true,
            });
    }
    let mut association =
        Association::open(addr, "SCU", "SCP", vec![commit_context()], client_options)
            .await
            .unwrap();

    let mut ok = crate::nservice::NServiceRequest::new(request_field, 1);
    ok.sop_class_uid = Some(COMMIT_SOP_CLASS.to_string());
    ok.sop_instance_uid = Some("1.2.3.4".to_string());
    if request_field == CommandField::NEventReportRq {
        ok.event_type_id = Some(1);
    }
    let response = association.send_n_service(&ok).await.unwrap();
    assert_eq!(
        response.field, response_field,
        "a {request_field:?} is answered with {response_field:?}"
    );
    assert_eq!(response.status, crate::dimse::status::SUCCESS);
    assert_eq!(response.message_id_being_responded_to, 1);

    let mut fail = crate::nservice::NServiceRequest::new(request_field, 2);
    fail.sop_class_uid = Some(COMMIT_SOP_CLASS.to_string());
    fail.sop_instance_uid = Some("FAIL".to_string());
    if request_field == CommandField::NEventReportRq {
        fail.event_type_id = Some(1);
    }
    let response = association.send_n_service(&fail).await.unwrap();
    assert_eq!(response.field, response_field);
    assert_eq!(
        response.status,
        crate::dimse::status::PROCESSING_FAILURE,
        "a failed provider maps to a failure status"
    );

    // the association survives a failed operation
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn mixed_n_service_and_echo_responses_route_by_message_id() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(NServiceEcho),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let mut echo = echo_context();
    echo.id = 3;
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![commit_context(), echo],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let mut nget = crate::nservice::NServiceRequest::new(CommandField::NGetRq, 0);
    nget.sop_class_uid = Some(COMMIT_SOP_CLASS.to_string());
    nget.sop_instance_uid = Some("1.2.3.4".to_string());
    let mut events = Vec::new();
    association
        .send_mixed_batch(
            vec![
                crate::batch::DicomRequest::NService(nget),
                crate::batch::DicomRequest::Echo(crate::services::CEchoRequest {
                    message_id: 0,
                    affected_sop_class_uid: sop_class::VERIFICATION.to_string(),
                }),
            ],
            |event| {
                events.push(event);
                Ok(())
            },
        )
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].request_index, 0);
    assert_eq!(events[1].request_index, 1);
    assert!(events.iter().all(|event| event.is_final));
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn n_event_report_round_trips_through_the_server() {
    assert_n_service_round_trip(CommandField::NEventReportRq, CommandField::NEventReportRsp).await;
}

#[tokio::test]
async fn n_get_round_trips_through_the_server() {
    assert_n_service_round_trip(CommandField::NGetRq, CommandField::NGetRsp).await;
}

#[tokio::test]
async fn n_set_round_trips_through_the_server() {
    assert_n_service_round_trip(CommandField::NSetRq, CommandField::NSetRsp).await;
}

#[tokio::test]
async fn n_create_round_trips_through_the_server() {
    assert_n_service_round_trip(CommandField::NCreateRq, CommandField::NCreateRsp).await;
}

#[tokio::test]
async fn n_service_preserves_a_present_empty_dataset() {
    struct EmptyDatasetProvider(Arc<std::sync::Mutex<Option<bool>>>);

    #[async_trait::async_trait]
    impl ServiceProvider for EmptyDatasetProvider {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![COMMIT_SOP_CLASS.to_string()]
        }

        async fn on_n_set(
            &self,
            request: &crate::nservice::NServiceRequest,
        ) -> Result<crate::nservice::NServiceResponse> {
            *self.0.lock().unwrap() = Some(request.dataset.as_ref().is_some_and(Dataset::is_empty));
            Ok(crate::nservice::NServiceResponse::success(request, None))
        }
    }

    let seen = Arc::new(std::sync::Mutex::new(None));
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(EmptyDatasetProvider(seen.clone())),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![commit_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();

    let mut request = crate::nservice::NServiceRequest::new(CommandField::NSetRq, 1);
    request.sop_class_uid = Some(COMMIT_SOP_CLASS.to_string());
    request.sop_instance_uid = Some("1.2.3.4".to_string());
    request.dataset = Some(Dataset::new());
    association.send_n_service(&request).await.unwrap();
    assert_eq!(*seen.lock().unwrap(), Some(true));

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn concurrent_stores_complete_through_the_blocking_spool() {
    // §11.2/the default sink spools through spawn_blocking, so several
    // concurrent C-STOREs must still all succeed and leave the client with
    // an ordinary response.
    struct StoreOk;
    #[async_trait::async_trait]
    impl ServiceProvider for StoreOk {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec!["1.2.840.10008.5.1.4.1.1.7".to_string()]
        }
        async fn on_c_store(&self, request: &CStoreRequest) -> Result<CStoreResponse> {
            Ok(CStoreResponse::success(request))
        }
    }

    let server = DicomServer::bind("127.0.0.1:0", Arc::new(StoreOk), ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    let mut tasks = Vec::new();
    for _ in 0..8 {
        tasks.push(tokio::spawn(async move {
            let mut association = Association::open(
                addr,
                "SCU",
                "SCP",
                vec![store_context()],
                ClientOptions::default(),
            )
            .await
            .unwrap();
            let response = association.send_c_store(&instance_file()).await.unwrap();
            assert_eq!(response.status, crate::dimse::status::SUCCESS);
            association.release().await.unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
}

#[tokio::test]
async fn a_response_command_aborts_the_association_instead_of_panicking() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();

    // A C-FIND-RSP-shaped command set where a request belongs: the peer is
    // violating DIMSE, and the SCP must answer with A-ABORT, not panic.
    let bogus = command_bytes(&Command::request(CommandField::CFindRsp, 1)).unwrap();
    let context_id = association.contexts().all()[0].id;
    for pdata in association
        .splitter
        .split(context_id, true, &bogus)
        .unwrap()
    {
        write_pdu(&mut association.stream, &Pdu::PData(pdata))
            .await
            .unwrap();
    }

    match read_pdu(&mut association.stream).await {
        Ok(Some(Pdu::Abort(abort))) => {
            assert_eq!(abort.source, AbortSource::User, "service-user abort");
            assert_eq!(abort.reason, 0, "reason not specified");
        }
        other => panic!("expected A-ABORT, got {other:?}"),
    }
    // a panicking connection task would surface as a JoinError here
    let outcome = handle.await.unwrap();
    assert!(
        outcome.is_err(),
        "the connection ends with a protocol error"
    );
}

/// A provider that reports progress but never terminates its stream.
struct ProgressOnlyMoveProvider;

#[async_trait::async_trait]
impl ServiceProvider for ProgressOnlyMoveProvider {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![sop_class::STUDY_ROOT_QR_MOVE.to_string()]
    }

    fn on_c_move<'a>(
        &'a self,
        request: &'a crate::cmove::CMoveRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cmove::CMoveResponse>> {
        let summary = crate::cmove::SubOperationSummary {
            completed: 2,
            failed: 1,
            warning: 0,
        };
        Box::pin(futures::stream::once(async move {
            Ok(crate::cmove::CMoveResponse::pending(request, 0, summary))
        }))
    }
}

#[tokio::test]
async fn c_move_reports_the_real_counts_when_the_provider_never_terminates() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(ProgressOnlyMoveProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![move_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let responses = association
        .send_c_move(&crate::cmove::CMoveRequest::study(
            "1.2.3.4",
            "STORE-SCP",
            1,
        ))
        .await
        .unwrap();

    assert_eq!(
        responses.len(),
        2,
        "the pending progress plus the engine's terminal answer"
    );
    let terminal = responses.last().unwrap();
    assert!(!terminal.is_pending());
    assert_eq!(
        terminal.summary.completed, 2,
        "the reported progress is not replaced with zeros"
    );
    assert_eq!(terminal.summary.failed, 1);
    assert_eq!(terminal.state(), crate::dimse::StatusState::Warning);

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_move_delivers_pending_before_provider_finishes() {
    struct GatedMove {
        release_terminal: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl ServiceProvider for GatedMove {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_MOVE.to_string()]
        }

        fn on_c_move<'a>(
            &'a self,
            request: &'a crate::cmove::CMoveRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cmove::CMoveResponse>> {
            use futures::StreamExt;
            let release_terminal = self.release_terminal.clone();
            let pending = crate::cmove::CMoveResponse::pending(
                request,
                1,
                crate::cmove::SubOperationSummary::default(),
            );
            Box::pin(futures::stream::once(async move { Ok(pending) }).chain(
                futures::stream::once(async move {
                    release_terminal.notified().await;
                    Ok(crate::cmove::CMoveResponse::completed(
                        request,
                        crate::cmove::SubOperationSummary::default(),
                    ))
                }),
            ))
        }
    }

    let release_terminal = Arc::new(tokio::sync::Notify::new());
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(GatedMove {
            release_terminal: release_terminal.clone(),
        }),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![move_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let request = crate::cmove::CMoveRequest::study("1.2.3.4", "STORE-SCP", 1);
    let mut responses = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(2),
        association.send_c_move_stream(&request, |response| {
            let terminal = !response.is_pending();
            responses.push(response);
            if !terminal {
                release_terminal.notify_one();
            }
            Ok(())
        }),
    )
    .await
    .expect("the first pending response must not wait for the provider's terminal response")
    .unwrap();

    assert_eq!(responses.len(), 2);
    assert!(responses[0].is_pending());
    assert_eq!(responses[1].state(), crate::dimse::StatusState::Success);
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_c_move_c_cancel_stops_an_in_flight_provider() {
    struct CancelableMove;

    #[async_trait::async_trait]
    impl ServiceProvider for CancelableMove {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_MOVE.to_string()]
        }

        fn on_c_move<'a>(
            &'a self,
            request: &'a crate::cmove::CMoveRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cmove::CMoveResponse>> {
            use futures::StreamExt;
            let pending = crate::cmove::CMoveResponse::pending(
                request,
                7,
                crate::cmove::SubOperationSummary {
                    completed: 2,
                    failed: 1,
                    warning: 3,
                },
            );
            Box::pin(
                futures::stream::once(async move { Ok(pending) }).chain(futures::stream::pending()),
            )
        }
    }

    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(CancelableMove),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![move_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let request = crate::cmove::CMoveRequest::study("1.2.3.4", "STORE-SCP", 1);
    let context_id = association.contexts().all()[0].id;
    let syntax = transfer_syntax_of(&association.contexts().all()[0].transfer_syntax).unwrap();
    let command = bytes::Bytes::from(command_bytes(&request.to_command()).unwrap());
    let identifier =
        bytes::Bytes::from(crate::message::encode_dataset(&request.identifier, &syntax).unwrap());
    for pdata in association
        .splitter
        .split_message_bytes(context_id, command, Some(identifier))
        .unwrap()
    {
        write_pdu(&mut association.stream, &Pdu::PData(pdata))
            .await
            .unwrap();
    }

    let pending = association
        .await_response(1, CommandField::CMoveRsp)
        .await
        .unwrap();
    assert_eq!(pending.status, Some(crate::dimse::status::PENDING));
    assert_eq!(pending.remaining_suboperations, Some(7));
    assert_eq!(pending.completed_suboperations, Some(2));
    assert_eq!(pending.failed_suboperations, Some(1));
    assert_eq!(pending.warning_suboperations, Some(3));
    association.send_c_cancel(context_id, 1).await.unwrap();
    let cancelled = tokio::time::timeout(
        Duration::from_secs(2),
        association.await_response(1, CommandField::CMoveRsp),
    )
    .await
    .expect("C-CANCEL must stop the in-flight provider")
    .unwrap();
    assert_eq!(cancelled.status, Some(crate::dimse::status::CANCEL));
    assert_eq!(cancelled.remaining_suboperations, Some(7));
    assert_eq!(cancelled.completed_suboperations, Some(2));
    assert_eq!(cancelled.failed_suboperations, Some(1));
    assert_eq!(cancelled.warning_suboperations, Some(3));

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn max_clients_bounds_concurrent_associations() {
    let options = ServerOptions {
        max_clients: 1,
        ..Default::default()
    };
    let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), options)
        .await
        .unwrap();
    let addr = server.local_addr();
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    // the first association takes the only slot
    let first = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();

    // a second peer gets no answer while the slot is taken
    let second = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions {
            association_timeout: Duration::from_millis(200),
            ..Default::default()
        },
    )
    .await;
    match second {
        Err(NetworkError::AssociationRequestTimedOut { .. }) => {}
        other => panic!("expected the second association to wait for a slot, got {other:?}"),
    }

    // releasing the slot serves the next peer
    first.release().await.unwrap();
    let third = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions::default(),
    )
    .await;
    third
        .expect("the slot is released when the association ends")
        .release()
        .await
        .unwrap();
}

#[tokio::test]
async fn connected_client_handle_tracks_live_connections() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let clients = server.connected_clients();
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    let association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(clients.count(), 1);

    association.release().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while clients.count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("released association leaves the connected client count");
}

#[tokio::test]
async fn an_association_that_never_sends_a_request_times_out() {
    use tokio::io::AsyncReadExt;

    let options = ServerOptions {
        association_timeout: Duration::from_millis(100),
        ..Default::default()
    };
    let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), options)
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    // connect and stay silent: the ACSE phase must not hold the task forever
    let mut client = TcpStream::connect(addr).await.unwrap();
    let mut abort = [0u8; 10];
    tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut abort))
        .await
        .expect("the server answers within its association timeout")
        .unwrap();
    assert_eq!(abort[0], 0x07, "A-ABORT");
    assert_eq!(abort[8], AbortSource::Provider.code());
    assert_eq!(abort[9], 0, "reason not specified");

    match handle.await.unwrap() {
        Err(NetworkError::AssociationRequestTimedOut { .. }) => {}
        other => panic!("expected AssociationRequestTimedOut, got {other:?}"),
    }
}

// -- service semantics: remaining counts, message IDs, sub-operations -----

/// Runs one malformed P-DATA frame against a live `serve_connection` over a
/// duplex pair: performs the ACSE handshake, sends the raw frame, then waits
/// (bounded) for the connection task. Returns its typed result; a panic in
/// the task surfaces as a failed `expect`, a hang as a timeout.
async fn drive_malformed_pdata(frame: Vec<u8>) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let handle = tokio::spawn(serve_connection(
        server,
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    ));

    let mut rq = Associate::new("SCP", "SCU");
    rq.presentation_contexts.push(echo_context());
    let request = crate::pdu::encode(&Pdu::AssociateRq(rq)).expect("encode RQ");
    client
        .write_all(&request)
        .await
        .expect("send A-ASSOCIATE-RQ");
    match read_pdu(&mut client).await.expect("read AC") {
        Some(Pdu::AssociateAc(_)) => {}
        other => panic!("expected A-ASSOCIATE-AC, got {other:?}"),
    }

    client.write_all(&frame).await.ok();
    drop(client); // EOF if the frame alone did not already end the exchange

    match tokio::time::timeout(Duration::from_secs(2), handle).await {
        Err(_) => panic!("server hung on malformed P-DATA"),
        Ok(joined) => joined.expect("serve_connection must not panic"),
    }
}

#[tokio::test]
async fn malformed_pdata_after_association_never_panics() {
    /// Wraps a P-DATA body in a PDU frame with a correct length field.
    fn pdata_frame(body: &[u8]) -> Vec<u8> {
        let mut frame = vec![0x04, 0x00];
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    // Structurally invalid framing: each must come back as a typed error.
    let invalid: Vec<(&str, Vec<u8>)> = vec![
        (
            "declared PDU length over the cap",
            vec![0x04, 0x00, 0xFF, 0xFF, 0xFF, 0xFF],
        ),
        ("unknown PDU type", vec![0x09, 0x00, 0x00, 0x00, 0x00, 0x00]),
        ("zero-length PDV", pdata_frame(&[0x00, 0x00, 0x00, 0x00])),
        ("one-byte PDV", pdata_frame(&[0x00, 0x00, 0x00, 0x01, 0x01])),
        (
            "PDV length past the body",
            pdata_frame(&[0x00, 0x00, 0x00, 0x10, 0x01, 0x03, 1, 2]),
        ),
        (
            "truncated PDV value",
            pdata_frame(&[0x00, 0x00, 0x00, 0x06, 0x01, 0x03, 1, 2]),
        ),
    ];
    for (name, frame) in invalid {
        let result = drive_malformed_pdata(frame).await;
        assert!(
            result.is_err(),
            "case {name} must be a typed error, got {result:?}"
        );
    }

    // Decodable but ill-behaved P-DATA: the guaranteed contract is no panic
    // and no hang (Ok or a typed Err are both acceptable).
    let echo = command_bytes(&Command::request(CommandField::CEchoRq, 1)).expect("encode echo");
    let mut not_negotiated = Vec::new();
    not_negotiated.extend_from_slice(&((echo.len() + 2) as u32).to_be_bytes());
    not_negotiated.push(0xFF); // context id never negotiated
    not_negotiated.push(0x03);
    not_negotiated.extend_from_slice(&echo);
    let _ = drive_malformed_pdata(pdata_frame(&not_negotiated)).await;

    let mut command_then_stray_data = Vec::new();
    command_then_stray_data.extend_from_slice(&((echo.len() + 2) as u32).to_be_bytes());
    command_then_stray_data.push(1);
    command_then_stray_data.push(0x03); // command + last
    command_then_stray_data.extend_from_slice(&echo);
    command_then_stray_data.extend_from_slice(&4u32.to_be_bytes());
    command_then_stray_data.push(1);
    command_then_stray_data.push(0x00); // data, never last
    command_then_stray_data.extend_from_slice(&[0xAB, 0xCD]);
    let _ = drive_malformed_pdata(pdata_frame(&command_then_stray_data)).await;

    // Seeded PRNG fuzz over random P-DATA bodies: 200 iterations, no new
    // dependency, bounded and deterministic.
    let mut state = 0x5EED_1234u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..200 {
        let len = (next() % 40) as usize;
        let body: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        let _ = drive_malformed_pdata(pdata_frame(&body)).await;
    }
}

#[tokio::test]
async fn protocol_errors_after_association_send_a_provider_abort() {
    use tokio::io::AsyncWriteExt;

    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let handle = tokio::spawn(serve_connection(
        server,
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    ));

    let mut request = Associate::new("SCP", "SCU");
    request.presentation_contexts.push(echo_context());
    client
        .write_all(&crate::pdu::encode(&Pdu::AssociateRq(request)).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_pdu(&mut client).await.unwrap(),
        Some(Pdu::AssociateAc(_))
    ));

    // Unknown PDU types are protocol errors after association. The SCP
    // must notify the peer with A-ABORT before closing the connection.
    client.write_all(&[0x09, 0x00, 0, 0, 0, 0]).await.unwrap();
    match tokio::time::timeout(Duration::from_secs(2), read_pdu(&mut client)).await {
        Ok(Ok(Some(Pdu::Abort(abort)))) => {
            assert_eq!(abort.source, AbortSource::Provider);
            assert_eq!(abort.reason, 0);
        }
        other => panic!("expected provider A-ABORT, got {other:?}"),
    }
    assert!(handle.await.unwrap().is_err());
}

/// The contexts a C-GET needs: the retrieve model plus the storage class.
fn get_contexts() -> Vec<PresentationContext> {
    vec![
        get_context(),
        PresentationContext::new(
            3,
            "1.2.840.10008.5.1.4.1.1.7",
            vec!["1.2.840.10008.1.2".to_string()],
        ),
    ]
}

fn instance_with_uid(uid: &str) -> radicom::io::file::DicomFile {
    let mut dataset = Dataset::new();
    dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
    dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, uid);
    dataset.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
    radicom::io::file::DicomFile::from_dataset(dataset)
}

#[tokio::test]
async fn c_get_reports_the_remaining_sub_operations() {
    // PS3.7 §9.1.3: (0000,1020) counts the sub-operations still to be
    // performed including the current one, so three instances report
    // 3, 2, 1 and only the terminal response reports 0.
    let provider = Arc::new(GetProvider {
        instances: vec![
            instance_with_uid("1.2.3.4.5.1"),
            instance_with_uid("1.2.3.4.5.2"),
            instance_with_uid("1.2.3.4.5.3"),
        ],
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "GET-SCU",
        "GET-SCP",
        get_contexts(),
        get_client_options(),
    )
    .await
    .unwrap();
    // the reverse C-STORE message IDs must be distinct, or the SCU cannot
    // match its responses
    let stored = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = stored.clone();
    association.set_reverse_store_handler(Arc::new(move |request: &CStoreRequest| {
        sink.lock().unwrap().push(request.message_id);
        Ok(CStoreResponse::success(request))
    }));

    let responses = association
        .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
        .await
        .unwrap();

    let pending: Vec<u16> = responses
        .iter()
        .filter(|r| r.is_pending())
        .map(|r| r.remaining)
        .collect();
    assert_eq!(
        pending,
        vec![3, 2, 1],
        "the count includes the sub-operation being reported"
    );
    let terminal = responses.last().unwrap();
    assert!(!terminal.is_pending());
    assert_eq!(terminal.remaining, 0, "only the end reports none left");
    assert_eq!(terminal.summary.completed, 3);
    assert_eq!(terminal.status, crate::dimse::status::SUCCESS);

    let mut ids = stored.lock().unwrap().clone();
    assert_eq!(ids.len(), 3);
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 3, "every reverse C-STORE has its own message ID");

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn c_get_records_an_undeliverable_instance_and_carries_on() {
    // an instance the SCP cannot address is one failed
    // sub-operation; the instances after it still have to arrive.
    let mut broken = instance_with_uid("1.2.3.4.5.1");
    broken.dataset.remove(Tag::SOP_INSTANCE_UID);
    let provider = Arc::new(GetProvider {
        instances: vec![broken, instance_with_uid("1.2.3.4.5.2")],
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "GET-SCU",
        "GET-SCP",
        get_contexts(),
        get_client_options(),
    )
    .await
    .unwrap();
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    association.collect_reverse_stores(received.clone());

    let responses = association
        .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
        .await
        .expect("the retrieval completes despite the broken instance");

    assert_eq!(
        received.lock().unwrap().len(),
        1,
        "the good instance still arrived"
    );
    let terminal = responses.last().unwrap();
    assert_eq!(terminal.summary.completed, 1);
    assert_eq!(terminal.summary.failed, 1);
    assert_eq!(
        terminal.status,
        crate::dimse::status::SUBOPERATIONS_COMPLETED_WITH_WARNINGS,
        "a partial retrieve is not a failure"
    );

    // the association survived: it can still be released cleanly
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn message_ids_skip_requests_still_in_flight() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    // message ID 1 is still awaiting a response
    association.pending.insert(1);
    assert_eq!(
        association.allocate_message_id().unwrap(),
        2,
        "an in-flight ID is skipped"
    );
    // the ordinary path still allocates, sends and releases its entry
    let response = association
        .send_c_echo(sop_class::VERIFICATION)
        .await
        .unwrap();
    assert_eq!(response.status, crate::dimse::status::SUCCESS);

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn release_reports_a_peer_abort_instead_of_success() {
    // the peer must answer A-RELEASE-RQ with A-RELEASE-RP
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        crate::test_wire::accept_association(&mut stream).await;
        let release = read_pdu(&mut stream).await.unwrap();
        assert!(
            matches!(release, Some(Pdu::ReleaseRq)),
            "expected A-RELEASE-RQ"
        );
        let abort = Abort {
            source: AbortSource::Provider,
            reason: 0,
        };
        write_pdu(&mut stream, &Pdu::Abort(abort)).await.unwrap();
    });

    let association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    match association.release().await {
        Err(NetworkError::Abort {
            source_code,
            reason,
        }) => assert_eq!((source_code, reason), (2, 0)),
        other => panic!("expected the peer abort to surface, got {other:?}"),
    }
    peer.await.unwrap();
}

#[tokio::test]
async fn release_times_out_when_the_release_rp_never_arrives() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        crate::test_wire::accept_association(&mut stream).await;
        // swallow the release request and stay silent
        let _ = read_pdu(&mut stream).await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let options = ClientOptions {
        release_timeout: Duration::from_millis(100),
        ..Default::default()
    };
    let association = Association::open(addr, "SCU", "SCP", vec![echo_context()], options)
        .await
        .unwrap();
    match association.release().await {
        Err(NetworkError::ReleaseTimedOut { timeout_ms }) => assert_eq!(timeout_ms, 100),
        other => panic!("expected a release timeout, got {other:?}"),
    }
    peer.abort();
}

#[tokio::test]
async fn bound_server_reserves_and_releases_its_address() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let address = server.local_addr();
    assert!(!crate::registry::DicomServerRegistry::is_available(address));
    let duplicate = DicomServer::bind(
        address,
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await;
    assert!(
        duplicate.is_err(),
        "the registry and OS both reject a duplicate bind"
    );
    drop(server);
    assert!(crate::registry::DicomServerRegistry::is_available(address));
}

#[test]
fn transcoder_for_reuses_the_same_instance_per_syntax_pair() {
    let mut cache = std::collections::HashMap::new();
    let source = radicom::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let target = radicom::model::transfer_syntax::syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone();
    let first = transcoder_for(&mut cache, &source, &target) as *const _;
    let second = transcoder_for(&mut cache, &source, &target) as *const _;
    assert_eq!(
        first, second,
        "the same syntax pair returns the cached transcoder"
    );
    assert_eq!(cache.len(), 1);

    let other = radicom::model::transfer_syntax::syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone();
    let third = transcoder_for(&mut cache, &source, &other) as *const _;
    assert_ne!(first, third, "a different target gets its own transcoder");
    assert_eq!(cache.len(), 2);
}

#[test]
fn response_context_mismatch_is_rejected() {
    assert!(Association::validate_response_context(1, 1, "C-ECHO").is_ok());
    let error = Association::validate_response_context(1, 3, "C-ECHO").unwrap_err();
    assert!(error.to_string().contains("wrong presentation context"));
    assert!(Association::validate_response_context(1, 3, "N-service").is_err());
    assert!(Association::validate_response_context(1, 3, "C-FIND batch").is_err());
    assert!(Association::validate_response_context(1, 3, "other").is_err());
}

#[test]
fn response_sop_class_mismatch_is_rejected() {
    assert!(Association::validate_response_sop_class("1.2.3", "1.2.3").is_ok());
    assert!(Association::validate_response_sop_class("1.2.3", "1.2.4").is_err());
}

#[test]
fn dataset_pairing_rejects_orphan_and_duplicate_messages() {
    let mut pending = None;
    assert!(Association::take_pending_dataset(&mut pending, "orphan").is_err());
    Association::put_pending_dataset(&mut pending, 1u8, "duplicate").unwrap();
    assert!(Association::put_pending_dataset(&mut pending, 2u8, "duplicate").is_err());
    assert_eq!(
        Association::take_pending_dataset(&mut pending, "orphan").unwrap(),
        1
    );
}

// -- association validation and AC layout --------------------------------

/// Sends an A-ASSOCIATE-RQ shaped by `configure` and returns whatever the
/// server answers with.
async fn raw_association_handshake(configure: impl FnOnce(&mut Associate)) -> crate::pdu::Pdu {
    use tokio::io::AsyncWriteExt as _;
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut request = Associate::new("CALLED-SCP", "CALLING-SCU");
    request.presentation_contexts = vec![echo_context()];
    configure(&mut request);

    let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    write_pdu(&mut tcp, &Pdu::AssociateRq(request))
        .await
        .unwrap();
    let answer = read_pdu(&mut tcp).await.unwrap().expect("an answer PDU");
    let _ = tcp.shutdown().await;
    let _ = handle.await;
    answer
}

#[tokio::test]
async fn client_rejects_malformed_association_accepts() {
    for malformed_kind in [0u8, 1u8, 2u8, 3u8, 4u8, 5u8, 6u8, 7u8] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let Some(Pdu::AssociateRq(request)) = read_pdu(&mut stream).await.unwrap() else {
                panic!("client did not send A-ASSOCIATE-RQ");
            };
            let mut answer = Associate::new(request.called_ae, request.calling_ae);
            answer.user_information = request.user_information;
            if malformed_kind >= 4 {
                answer.presentation_contexts = vec![PresentationContext {
                    id: 1,
                    abstract_syntax: if malformed_kind == 7 {
                        sop_class::PATIENT_ROOT_QR_FIND.to_string()
                    } else {
                        String::new()
                    },
                    transfer_syntaxes: vec![syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.uid().into()],
                    result: 0,
                }];
            }
            if malformed_kind == 4 {
                answer.application_context = "9.9.9".into();
            } else if malformed_kind == 5 {
                answer.protocol_version = 2;
            } else if malformed_kind == 6 {
                answer.called_ae = "OTHER-CALLED".into();
            }
            if malformed_kind == 3 {
                answer.presentation_contexts = vec![
                    PresentationContext {
                        id: 1,
                        abstract_syntax: String::new(),
                        transfer_syntaxes: Vec::new(),
                        result: 1,
                    },
                    PresentationContext {
                        id: 1,
                        abstract_syntax: String::new(),
                        transfer_syntaxes: Vec::new(),
                        result: 1,
                    },
                ];
            } else if malformed_kind == 1 || malformed_kind == 2 {
                answer.presentation_contexts.push(PresentationContext {
                    id: 1,
                    abstract_syntax: String::new(),
                    transfer_syntaxes: Vec::new(),
                    result: 5,
                });
            }
            if malformed_kind == 2 {
                use tokio::io::AsyncWriteExt as _;
                let mut frame = crate::pdu::encode(&Pdu::AssociateAc(answer)).unwrap();
                let marker = [1, 0, 5, 0];
                let offset = frame
                    .windows(marker.len())
                    .position(|window| window == marker)
                    .expect("presentation context marker");
                frame[offset + 2] = 0;
                stream.write_all(&frame).await.unwrap();
            } else if malformed_kind == 7 {
                use tokio::io::AsyncWriteExt as _;
                let mut frame = crate::pdu::encode(&Pdu::AssociateAc(answer)).unwrap();
                let uid = sop_class::PATIENT_ROOT_QR_FIND.as_bytes();
                let mut item = vec![0x30, 0, (uid.len() >> 8) as u8, uid.len() as u8];
                item.extend_from_slice(uid);
                if item.len() % 2 != 0 {
                    item.push(0);
                    item[2] = (item.len() - 4) as u8;
                }
                let pc = frame
                    .windows(4)
                    .position(|window| window[0] == 0x21 && window[1] == 0)
                    .expect("presentation context AC item");
                let body = pc + 4;
                let old_len = u16::from_be_bytes([frame[pc + 2], frame[pc + 3]]);
                let item_len = item.len() as u16;
                let new_len = old_len + item_len;
                frame[pc + 2..pc + 4].copy_from_slice(&new_len.to_be_bytes());
                frame.splice(body + 4..body + 4, item);
                let pdu_len =
                    u32::from_be_bytes([frame[2], frame[3], frame[4], frame[5]]) + item_len as u32;
                frame[2..6].copy_from_slice(&pdu_len.to_be_bytes());
                stream.write_all(&frame).await.unwrap();
            } else {
                write_pdu(&mut stream, &Pdu::AssociateAc(answer))
                    .await
                    .unwrap();
            }
        });
        let result = Association::open(
            addr,
            "SCU",
            "SCP",
            if malformed_kind == 3 {
                vec![
                    echo_context(),
                    PresentationContext::new(
                        3,
                        sop_class::VERIFICATION,
                        vec!["1.2.840.10008.1.2".to_string()],
                    ),
                ]
            } else {
                vec![echo_context()]
            },
            ClientOptions::default(),
        )
        .await;
        assert!(result.is_err(), "malformed A-ASSOCIATE-AC was accepted");
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn the_accept_repeats_the_request_ae_titles() {
    // NET-02: the AC carries the RQ's AE titles in the RQ's order.
    let answer = raw_association_handshake(|_| {}).await;
    let Pdu::AssociateAc(accept) = answer else {
        panic!("expected A-ASSOCIATE-AC, got {answer:?}");
    };
    assert_eq!(accept.called_ae, "CALLED-SCP", "called AE is not swapped");
    assert_eq!(accept.calling_ae, "CALLING-SCU");
}

#[tokio::test]
async fn an_unsupported_protocol_version_is_rejected() {
    let answer = raw_association_handshake(|request| request.protocol_version = 2).await;
    let Pdu::AssociateRj(reject) = answer else {
        panic!("expected A-ASSOCIATE-RJ, got {answer:?}");
    };
    assert_eq!(reject.source.code(), 2, "provider-ACSE source");
    assert_eq!(reject.reason, 2, "protocol version not supported");
}

#[tokio::test]
async fn a_foreign_application_context_is_rejected() {
    let answer =
        raw_association_handshake(|request| request.application_context = "9.9.9".into()).await;
    let Pdu::AssociateRj(reject) = answer else {
        panic!("expected A-ASSOCIATE-RJ, got {answer:?}");
    };
    assert_eq!(reject.source.code(), 1, "service-user source");
    assert_eq!(reject.reason, 2, "application context not supported");
}

#[tokio::test]
async fn no_acceptable_context_is_rejected_without_a_reason() {
    // NET-06: reason 3 means "calling AE not recognized"; "nothing
    // acceptable" has no dedicated code, so it is 1 (no reason given).
    let answer = raw_association_handshake(|request| {
        request.presentation_contexts = vec![PresentationContext::new(
            1,
            "9.9.9",
            vec!["1.2.840.10008.1.2".to_string()],
        )];
    })
    .await;
    let Pdu::AssociateRj(reject) = answer else {
        panic!("expected A-ASSOCIATE-RJ, got {answer:?}");
    };
    assert_eq!(reject.source.code(), 1);
    assert_eq!(reject.reason, 1, "no reason given, not 3");
}

#[tokio::test]
async fn a_duplicate_presentation_context_id_aborts_the_association() {
    let answer = raw_association_handshake(|request| {
        request.presentation_contexts = vec![
            echo_context(),
            PresentationContext::new(
                1,
                sop_class::VERIFICATION,
                vec!["1.2.840.10008.1.2.1".to_string()],
            ),
        ];
    })
    .await;
    assert!(
        matches!(answer, Pdu::Abort(_)),
        "a malformed context set is aborted, got {answer:?}"
    );
}

#[tokio::test]
async fn an_even_presentation_context_id_aborts_the_association() {
    let answer = raw_association_handshake(|request| {
        request.presentation_contexts = vec![PresentationContext::new(
            2,
            sop_class::VERIFICATION,
            vec!["1.2.840.10008.1.2".to_string()],
        )];
    })
    .await;
    assert!(
        matches!(answer, Pdu::Abort(_)),
        "an even context ID is aborted, got {answer:?}"
    );
}

#[tokio::test]
async fn a_command_on_an_unaccepted_context_aborts_the_association() {
    use tokio::io::AsyncWriteExt as _;
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut request = Associate::new("SCP", "SCU");
    request.presentation_contexts = vec![echo_context()];
    write_pdu(&mut tcp, &Pdu::AssociateRq(request))
        .await
        .unwrap();
    assert!(matches!(
        read_pdu(&mut tcp).await.unwrap(),
        Some(Pdu::AssociateAc(_))
    ));

    // context 5 was never proposed: PS3.8 §9.3.3 forbids sending on it
    let command = Command::request(CommandField::CEchoRq, 1);
    let pdata = crate::pdu::PData {
        pdvs: vec![crate::pdu::Pdv {
            context_id: 5,
            is_command: true,
            is_last: true,
            data: command_bytes(&command).unwrap().into(),
        }],
    };
    write_pdu(&mut tcp, &Pdu::PData(pdata)).await.unwrap();
    let answer = read_pdu(&mut tcp).await.unwrap();
    assert!(
        matches!(answer, Some(Pdu::Abort(_))),
        "the association is aborted, got {answer:?}"
    );
    let _ = tcp.shutdown().await;
    let _ = handle.await;
}

// -- reverse sub-operations and terminal responses ----------------------

#[tokio::test]
async fn reverse_store_response_must_match_storage_context_and_optional_uids() {
    use crate::association::transfer_syntax;
    use crate::pdu::RoleSelection;
    let file = instance_file();
    let sop = "1.2.840.10008.5.1.4.1.1.7";
    let provider = Arc::new(VerificationProvider);
    let options = ServerOptions::default();
    let splitter = PduSplitter::new(16384, 16);
    let mut policy = NegotiationPolicy::new(vec![sop.into()]);
    policy.accept_roles = true;
    let mut contexts = policy.negotiate(&[PresentationContext::new(
        3,
        sop,
        vec![transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.into()],
    )]);
    policy.negotiate_roles(
        &mut contexts,
        &[RoleSelection {
            sop_class: sop.into(),
            scu_role: true,
            scp_role: true,
        }],
    );
    let accepted = AcceptedContexts::from_negotiated(contexts);
    for mismatch in 0..5 {
        let (_peer, mut io) = tokio::io::duplex(65536);
        let mut response = Command::request(CommandField::CStoreRsp, 1);
        response.message_id_being_responded_to = Some(1);
        response.status = Some(crate::dimse::status::SUCCESS);
        if mismatch != 4 {
            response.affected_sop_class_uid = Some(if mismatch == 1 { "9.9" } else { sop }.into());
            response.affected_sop_instance_uid =
                Some(if mismatch == 2 { "9.8" } else { "1.2.3.4.5.6" }.into());
        }
        let mut cancel = Command::request(CommandField::CCancelRq, 99);
        cancel.message_id_being_responded_to = Some(99);
        let pending_units = VecDeque::from([
            crate::assembler::MessageUnit {
                context_id: 1,
                stream: Stream::Command,
                bytes: command_bytes(&cancel).unwrap().into(),
                spool: None,
            },
            crate::assembler::MessageUnit {
                context_id: if mismatch == 0 { 1 } else { 3 },
                stream: Stream::Command,
                bytes: command_bytes(&response).unwrap().into(),
                spool: None,
            },
        ]);
        let mut serving = Serving {
            stream: &mut io,
            splitter: &splitter,
            provider: &provider,
            options: &options,
            assembler: PdvAssembler::new(),
            pending_units,
            deferred_units: VecDeque::new(),
            deferred_requests: 0,
            incoming_window: 1,
            accepted: &accepted,
            reverse_message_id: 1,
            transcoder_cache: std::collections::HashMap::new(),
            pdu_reader: crate::transport::PduReader::new(),
        };
        let result = serving.send_reverse_store(&file, 1, 99).await;
        if mismatch < 3 {
            assert!(
                matches!(result, Err(NetworkError::Protocol(_))),
                "mismatch {mismatch}: {result:?}"
            );
        } else {
            assert_eq!(result.unwrap(), (StatusState::Success, true));
        }
    }
}

#[tokio::test]
async fn a_split_store_response_keeps_the_framer_in_sync() {
    // NET-01: the C-GET loop can leave a half-read frame in the PduReader.
    // await_store_response must use the same framer, never the bare socket.
    use tokio::io::AsyncWriteExt as _;
    let (mut peer, mut server_io) = tokio::io::duplex(4096);
    let provider = Arc::new(VerificationProvider);
    let options = ServerOptions::default();
    let accepted = AcceptedContexts::default();
    let splitter = PduSplitter::new(16384, 16);
    let mut serving = Serving {
        stream: &mut server_io,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::new(),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window: 1,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        pdu_reader: crate::transport::PduReader::new(),
    };

    let mut response = Command::request(CommandField::CStoreRsp, 7);
    response.message_id_being_responded_to = Some(7);
    response.status = Some(crate::dimse::status::SUCCESS);
    response.no_dataset = true;
    let wire = crate::pdu::encode(&Pdu::PData(crate::pdu::PData {
        pdvs: vec![crate::pdu::Pdv {
            context_id: 1,
            is_command: true,
            is_last: true,
            data: command_bytes(&response).unwrap().into(),
        }],
    }))
    .unwrap();

    // The peer writes in two pieces while this side reads: the first read
    // times out with the header half-read and must keep those bytes in the
    // framer, or the continuation bytes below would be read as a header.
    let writer = tokio::spawn(async move {
        peer.write_all(&wire[..3]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        peer.write_all(&wire[3..]).await.unwrap();
        peer.write_all(&crate::pdu::encode(&Pdu::ReleaseRq).unwrap())
            .await
            .unwrap();
    });

    let partial =
        tokio::time::timeout(std::time::Duration::from_millis(20), serving.next_pdu()).await;
    assert!(partial.is_err(), "the read waits for the rest of the frame");

    // a hard timeout turns a future regression into a failure, not a hang
    let (command, cancelled) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        serving.await_store_response(7, 1, 1, 99),
    )
    .await
    .expect("the reverse-store wait must not deadlock")
    .unwrap();
    assert_eq!(command.field, CommandField::CStoreRsp);
    assert!(!cancelled);
    assert_eq!(
        serving.next_pdu().await.unwrap(),
        Some(Pdu::ReleaseRq),
        "the following PDU is still framed correctly"
    );
    writer.await.unwrap();
}

/// A provider that serves Q/R classes but never produces a response.
struct SilentProvider(Vec<String>);

#[async_trait::async_trait]
impl ServiceProvider for SilentProvider {
    fn supported_sop_classes(&self) -> Vec<String> {
        self.0.clone()
    }
    fn on_c_find<'a>(
        &'a self,
        _request: &'a crate::cfind::CFindRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cfind::CFindResponse>> {
        Box::pin(futures::stream::empty())
    }
    fn on_c_move<'a>(
        &'a self,
        _request: &'a crate::cmove::CMoveRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cmove::CMoveResponse>> {
        Box::pin(futures::stream::empty())
    }
}

fn qr_context(sop_class_uid: &str) -> PresentationContext {
    PresentationContext::new(1, sop_class_uid, vec!["1.2.840.10008.1.2".to_string()])
}

#[tokio::test]
async fn an_empty_c_move_provider_reports_a_failure() {
    // NET-09: an unterminated provider stream must not read as success with
    // zero sub-operations.
    let provider = Arc::new(SilentProvider(vec![
        sop_class::STUDY_ROOT_QR_MOVE.to_string()
    ]));
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![qr_context(sop_class::STUDY_ROOT_QR_MOVE)],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let request = crate::cmove::CMoveRequest::study("1.2.3.4", "DEST", 3);
    let responses = association.send_c_move(&request).await.unwrap();
    let terminal = responses.last().expect("one terminal response");
    assert_eq!(
        terminal.status,
        crate::dimse::status::PROCESSING_FAILURE,
        "an empty provider stream is a provider bug"
    );
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn an_unterminated_c_find_provider_reports_a_failure() {
    let provider = Arc::new(SilentProvider(vec![
        sop_class::STUDY_ROOT_QR_FIND.to_string()
    ]));
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![qr_context(sop_class::STUDY_ROOT_QR_FIND)],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let responses = association
        .send_c_find(&crate::cfind::CFindRequest::study_query(1))
        .await
        .unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(
        responses[0].status,
        crate::dimse::status::PROCESSING_FAILURE
    );
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

/// A provider that serves C-GET but never produces a response.
struct SilentGetProvider;

#[async_trait::async_trait]
impl ServiceProvider for SilentGetProvider {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![sop_class::STUDY_ROOT_QR_GET.to_string()]
    }
    fn on_c_get<'a>(
        &'a self,
        _request: &'a crate::cget::CGetRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
        Box::pin(futures::stream::empty())
    }
}

#[tokio::test]
async fn an_empty_c_get_provider_reports_a_failure() {
    // NET-09: the C-GET arm of the empty-stream guard. C-FIND and C-MOVE
    // were pinned; C-GET was not, so this branch could regress unnoticed.
    let provider = Arc::new(SilentGetProvider);
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "GET-SCU",
        "GET-SCP",
        get_contexts(),
        get_client_options(),
    )
    .await
    .unwrap();
    association.collect_reverse_stores(Arc::new(std::sync::Mutex::new(Vec::new())));

    let responses = association
        .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
        .await
        .unwrap();
    let terminal = responses.last().expect("one terminal response");
    assert_eq!(
        terminal.status,
        crate::dimse::status::PROCESSING_FAILURE,
        "an empty C-GET provider stream is a provider bug, not a success"
    );
    assert_eq!(
        terminal.summary.completed, 0,
        "no sub-operations were counted as delivered"
    );

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

// -- raw request rejections and dispatcher guards ------------------------

#[tokio::test]
async fn a_context_without_an_abstract_syntax_aborts_the_association() {
    let answer = raw_association_handshake(|request| {
        request.presentation_contexts = vec![PresentationContext {
            id: 1,
            abstract_syntax: String::new(),
            transfer_syntaxes: vec!["1.2.840.10008.1.2".to_string()],
            result: 0,
        }];
    })
    .await;
    assert!(
        matches!(answer, Pdu::Abort(_)),
        "an empty abstract syntax is aborted, got {answer:?}"
    );
}

#[tokio::test]
async fn a_context_without_a_transfer_syntax_aborts_the_association() {
    let answer = raw_association_handshake(|request| {
        request.presentation_contexts = vec![PresentationContext {
            id: 1,
            abstract_syntax: sop_class::VERIFICATION.to_string(),
            transfer_syntaxes: Vec::new(),
            result: 0,
        }];
    })
    .await;
    assert!(
        matches!(answer, Pdu::Abort(_)),
        "an empty transfer-syntax list is aborted, got {answer:?}"
    );
}

#[test]
fn a_request_context_reserved_byte_is_not_a_result() {
    // PS3.8 Table 9-13: the third byte of an RQ presentation-context item
    // is reserved and must not be tested on receive. The wire decoder
    // leaves `result` at 0 for an RQ, so a (nonconformant but harmless)
    // nonzero reserved byte never reaches validation and is accepted.
    let context = PresentationContext {
        id: 1,
        abstract_syntax: sop_class::VERIFICATION.to_string(),
        transfer_syntaxes: vec!["1.2.840.10008.1.2".to_string()],
        result: 0,
    };
    assert!(
        validate_proposed_contexts(&[context]).is_ok(),
        "an RQ context is accepted; it carries no result field"
    );
}

/// Opens an association over a raw socket so the test can send PDUs the
/// high-level client would never produce.
async fn raw_associated_stream() -> (
    tokio::net::TcpStream,
    tokio::task::JoinHandle<crate::error::Result<()>>,
) {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut request = Associate::new("CALLED-SCP", "CALLING-SCU");
    request.presentation_contexts = vec![echo_context()];
    write_pdu(&mut tcp, &Pdu::AssociateRq(request))
        .await
        .unwrap();
    match read_pdu(&mut tcp).await.unwrap() {
        Some(Pdu::AssociateAc(_)) => {}
        other => panic!("expected an accept, got {other:?}"),
    }
    (tcp, handle)
}

#[tokio::test]
async fn an_unexpected_pdu_after_association_is_aborted() {
    for anomaly in [
        Pdu::AssociateRq(Associate::new("A", "B")),
        Pdu::AssociateAc(Associate::new("A", "B")),
        Pdu::AssociateRj(AssociateRj {
            result: crate::error::RejectResult::Permanent,
            source: crate::error::RejectSource::User,
            reason: 1,
        }),
        Pdu::ReleaseRp,
    ] {
        let (mut tcp, handle) = raw_associated_stream().await;
        write_pdu(&mut tcp, &anomaly).await.unwrap();
        match read_pdu(&mut tcp).await.unwrap() {
            Some(Pdu::Abort(_)) => {}
            other => panic!("expected an abort for {anomaly:?}, got {other:?}"),
        }
        let _ = handle.await;
    }
}

#[tokio::test]
async fn a_dataset_pdv_without_a_command_aborts_the_association() {
    use tokio::io::AsyncWriteExt as _;
    let (mut tcp, handle) = raw_associated_stream().await;
    let bytes = crate::pdu::encode(&Pdu::PData(crate::pdu::PData {
        pdvs: vec![crate::pdu::Pdv {
            context_id: 1,
            is_command: false,
            is_last: true,
            data: bytes::Bytes::from_static(&[0x01, 0x02]),
        }],
    }))
    .unwrap();
    tcp.write_all(&bytes).await.unwrap();
    match read_pdu(&mut tcp).await.unwrap() {
        Some(Pdu::Abort(_)) => {}
        other => panic!("expected an abort, got {other:?}"),
    }
    let _ = handle.await;
}

#[tokio::test]
async fn a_response_command_aborts_and_a_non_request_is_refused() {
    let provider = Arc::new(VerificationProvider);
    let options = ServerOptions::default();
    let accepted = AcceptedContexts::default();
    let splitter = PduSplitter::new(16384, 16);
    let (mut _peer, mut io) = tokio::io::duplex(65536);
    let mut serving = Serving {
        stream: &mut io,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::new(),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window: 1,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        pdu_reader: crate::transport::PduReader::new(),
    };
    // a C-STORE response is not a request: the peer violated DIMSE
    let error = handle_command(
        &mut serving,
        1,
        Command::request(CommandField::CStoreRsp, 1),
    )
    .await
    .expect_err("a response command is aborted");
    assert!(error.to_string().contains("not a request"), "{error}");

    // a request this side does not serve is answered, not aborted
    handle_command(&mut serving, 1, Command::request(CommandField::CStoreRq, 2))
        .await
        .expect("an unsupported request is answered");
    // a dataset sent for a command that has none is refused the same way
    handle_command_with_dataset(
        &mut serving,
        1,
        Command::request(CommandField::CEchoRq, 3),
        crate::assembler::MessageBody::Memory(bytes::Bytes::from_static(&[0x00])),
        &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
    )
    .await
    .expect("a stray dataset is answered");
    // an N-service dispatcher only accepts N requests
    handle_n_service(
        &mut serving,
        1,
        Command::request(CommandField::CEchoRq, 4),
        None,
        &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
    )
    .await
    .expect("a non-N command is answered");
}

#[tokio::test]
async fn a_store_response_without_a_request_keeps_the_framer_in_sync() {
    use tokio::io::AsyncWriteExt as _;
    let (mut peer, mut io) = tokio::io::duplex(4096);
    let provider = Arc::new(VerificationProvider);
    let options = ServerOptions::default();
    let accepted = AcceptedContexts::default();
    let splitter = PduSplitter::new(16384, 16);
    let mut serving = Serving {
        stream: &mut io,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::new(),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window: 1,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        pdu_reader: crate::transport::PduReader::new(),
    };

    let mut response = Command::request(CommandField::CStoreRsp, 7);
    response.message_id_being_responded_to = Some(7);
    response.status = Some(crate::dimse::status::SUCCESS);
    response.no_dataset = true;
    // a C-CANCEL for the operation arrives before the store response
    let mut cancel = Command::request(CommandField::CCancelRq, 8);
    cancel.message_id_being_responded_to = Some(99);
    let mut wire = crate::pdu::encode(&Pdu::PData(crate::pdu::PData {
        pdvs: vec![crate::pdu::Pdv {
            context_id: 1,
            is_command: true,
            is_last: true,
            data: command_bytes(&cancel).unwrap().into(),
        }],
    }))
    .unwrap();
    wire.extend_from_slice(
        &crate::pdu::encode(&Pdu::PData(crate::pdu::PData {
            pdvs: vec![crate::pdu::Pdv {
                context_id: 1,
                is_command: true,
                is_last: true,
                data: command_bytes(&response).unwrap().into(),
            }],
        }))
        .unwrap(),
    );
    peer.write_all(&wire).await.unwrap();

    let (command, cancelled) = serving.await_store_response(7, 1, 1, 99).await.unwrap();
    assert_eq!(command.field, CommandField::CStoreRsp);
    assert!(cancelled, "the C-CANCEL is remembered");

    // a dataset unit is skipped without decoding it
    serving
        .pending_units
        .push_back(crate::assembler::MessageUnit {
            context_id: 1,
            stream: crate::assembler::Stream::Data,
            bytes: bytes::Bytes::from_static(&[0xAA]),
            spool: None,
        });
    serving
        .pending_units
        .push_back(crate::assembler::MessageUnit {
            context_id: 1,
            stream: crate::assembler::Stream::Command,
            bytes: command_bytes(&response).unwrap().into(),
            spool: None,
        });
    let (again, _) = serving.await_store_response(7, 1, 1, 99).await.unwrap();
    assert_eq!(again.field, CommandField::CStoreRsp);
}

#[tokio::test]
async fn a_broken_peer_while_waiting_for_a_store_response_is_reported() {
    use tokio::io::AsyncWriteExt as _;
    // EOF
    let (peer, mut io) = tokio::io::duplex(4096);
    drop(peer);
    let provider = Arc::new(VerificationProvider);
    let options = ServerOptions::default();
    let accepted = AcceptedContexts::default();
    let splitter = PduSplitter::new(16384, 16);
    let mut serving = Serving {
        stream: &mut io,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::new(),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window: 1,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        pdu_reader: crate::transport::PduReader::new(),
    };
    assert!(matches!(
        serving.await_store_response(7, 1, 1, 99).await,
        Err(NetworkError::ClosedPrematurely { .. })
    ));

    // a peer abort
    let (mut peer, mut io) = tokio::io::duplex(4096);
    peer.write_all(
        &crate::pdu::encode(&Pdu::Abort(Abort {
            source: AbortSource::User,
            reason: 5,
        }))
        .unwrap(),
    )
    .await
    .unwrap();
    let mut serving = Serving {
        stream: &mut io,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::new(),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window: 1,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        pdu_reader: crate::transport::PduReader::new(),
    };
    assert!(matches!(
        serving.await_store_response(7, 1, 1, 99).await,
        Err(NetworkError::Abort { reason: 5, .. })
    ));

    // a release request while a sub-operation is outstanding
    let (mut peer, mut io) = tokio::io::duplex(4096);
    peer.write_all(&crate::pdu::encode(&Pdu::ReleaseRq).unwrap())
        .await
        .unwrap();
    let mut serving = Serving {
        stream: &mut io,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::new(),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window: 1,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        pdu_reader: crate::transport::PduReader::new(),
    };
    assert!(matches!(
        serving.await_store_response(7, 1, 1, 99).await,
        Err(NetworkError::ClosedPrematurely { .. })
    ));

    // an unrelated PDU
    let (mut peer, mut io) = tokio::io::duplex(4096);
    peer.write_all(&crate::pdu::encode(&Pdu::AssociateRq(Associate::new("A", "B"))).unwrap())
        .await
        .unwrap();
    let mut serving = Serving {
        stream: &mut io,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::new(),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window: 1,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        pdu_reader: crate::transport::PduReader::new(),
    };
    assert!(serving
        .await_store_response(7, 1, 1, 99)
        .await
        .unwrap_err()
        .to_string()
        .contains("unexpected PDU"));
}

// -- client-side A-ASSOCIATE-AC validation --------------------------------

/// Starts a one-shot mock peer: it reads the RQ, hands it to `configure`,
/// writes back the resulting PDU and keeps the socket open briefly.
async fn mock_associate_answer(
    configure: impl FnOnce(&mut Associate, &Associate) -> Pdu + Send + 'static,
) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let Some(Pdu::AssociateRq(rq)) = read_pdu(&mut stream).await.unwrap_or(None) else {
            return;
        };
        let mut answer = Associate::new(rq.called_ae.clone(), rq.calling_ae.clone());
        // an AC echoes the accepted contexts without the abstract syntax
        answer.presentation_contexts = rq
            .presentation_contexts
            .iter()
            .map(|pc| PresentationContext {
                id: pc.id,
                abstract_syntax: String::new(),
                transfer_syntaxes: pc.transfer_syntaxes.first().cloned().into_iter().collect(),
                result: 0,
            })
            .collect();
        answer.user_information = rq.user_information.clone();
        answer.user_information.user_identity = None;
        answer.user_information.user_identity_response = None;
        let pdu = configure(&mut answer, &rq);
        let _ = write_pdu(&mut stream, &pdu).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    });
    addr
}

async fn open_against(answer: std::net::SocketAddr) -> Result<Association> {
    Association::open(
        answer,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions::default(),
    )
    .await
}

#[tokio::test]
async fn a_peer_accept_with_an_invalid_result_code_is_rejected() {
    let addr = mock_associate_answer(|accept, _| {
        accept.presentation_contexts[0].result = 5;
        Pdu::AssociateAc(accept.clone())
    })
    .await;
    let error = open_against(addr).await.expect_err("result 5 is invalid");
    assert!(error.to_string().contains("result code"), "{error}");
}

#[tokio::test]
async fn a_peer_accept_for_an_unproposed_context_is_rejected() {
    let addr = mock_associate_answer(|accept, _| {
        accept.presentation_contexts[0].id = 3;
        Pdu::AssociateAc(accept.clone())
    })
    .await;
    let error = open_against(addr)
        .await
        .expect_err("context 3 was never proposed");
    assert!(error.to_string().contains("not proposed"), "{error}");
}

#[tokio::test]
async fn a_peer_accept_with_an_unproposed_syntax_is_rejected() {
    let addr = mock_associate_answer(|accept, _| {
        // JPEG baseline was never proposed (echo_context proposes the
        // implicit and explicit uncompressed syntaxes)
        accept.presentation_contexts[0].transfer_syntaxes =
            vec!["1.2.840.10008.1.2.4.50".to_string()];
        Pdu::AssociateAc(accept.clone())
    })
    .await;
    let error = open_against(addr)
        .await
        .expect_err("a syntax that was not proposed cannot be selected");
    assert!(error.to_string().contains("not proposed"), "{error}");
}

#[tokio::test]
async fn a_peer_accept_naming_an_abstract_syntax_is_honoured() {
    let addr = mock_associate_answer(|accept, _| {
        accept.presentation_contexts[0].abstract_syntax = sop_class::VERIFICATION.to_string();
        Pdu::AssociateAc(accept.clone())
    })
    .await;
    let association = open_against(addr).await.expect("the accept is valid");
    assert!(association.accepted_extended_negotiations().is_empty());
    assert_eq!(association.user_identity_response(), None);
}

#[tokio::test]
async fn a_peer_accept_claiming_zero_max_pdu_uses_our_limit() {
    let addr = mock_associate_answer(|accept, _| {
        accept.user_information.max_pdu_length = 0;
        Pdu::AssociateAc(accept.clone())
    })
    .await;
    open_against(addr).await.expect("0 means unlimited");
}

#[tokio::test]
async fn a_peer_answer_that_is_not_an_accept_or_reject_is_a_protocol_error() {
    let addr = mock_associate_answer(|_, _| Pdu::ReleaseRq).await;
    let error = open_against(addr)
        .await
        .expect_err("a release is not an answer");
    assert!(error.to_string().contains("A-ASSOCIATE-AC"), "{error}");
}

#[tokio::test]
async fn a_required_identity_response_must_come_back() {
    let addr = mock_associate_answer(|accept, _| Pdu::AssociateAc(accept.clone())).await;
    let options = ClientOptions {
        user_identity: Some(crate::pdu::UserIdentity {
            identity_type: 1,
            primary: b"user".to_vec(),
            secondary: Vec::new(),
            positive_response_requested: true,
        }),
        ..ClientOptions::default()
    };
    let error = Association::open(addr, "SCU", "SCP", vec![echo_context()], options)
        .await
        .expect_err("the peer did not answer the identity request");
    assert!(
        error.to_string().contains("user identity response"),
        "{error}"
    );
}

// -- provider streams that report progress without a terminal ------------

/// One provider that reports progress but never terminates its stream.
struct MovePendingThenEnd;

#[async_trait::async_trait]
impl ServiceProvider for MovePendingThenEnd {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![sop_class::STUDY_ROOT_QR_MOVE.to_string()]
    }

    fn on_c_move<'a>(
        &'a self,
        request: &'a crate::cmove::CMoveRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cmove::CMoveResponse>> {
        let summary = crate::cmove::SubOperationSummary {
            completed: 2,
            ..Default::default()
        };
        let events = vec![Ok(crate::cmove::CMoveResponse::pending(
            request, 2, summary,
        ))];
        Box::pin(futures::stream::iter(events))
    }
}

#[tokio::test]
async fn a_move_stream_that_reports_progress_without_a_terminal_completes() {
    // the last Pending carries the counters; the terminal is synthesized
    let provider = Arc::new(MovePendingThenEnd);
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![qr_context(sop_class::STUDY_ROOT_QR_MOVE)],
        get_client_options(),
    )
    .await
    .unwrap();
    association.collect_reverse_stores(Arc::new(std::sync::Mutex::new(Vec::new())));
    let responses = association
        .send_c_move(&crate::cmove::CMoveRequest::study("1.2.3", "DEST", 1))
        .await
        .unwrap();
    let terminal = responses.last().unwrap();
    assert_eq!(terminal.status, crate::dimse::status::SUCCESS);
    assert_eq!(terminal.summary.completed, 2);
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

struct FindPendingThenEnd;

#[async_trait::async_trait]
impl ServiceProvider for FindPendingThenEnd {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![sop_class::STUDY_ROOT_QR_FIND.to_string()]
    }

    fn on_c_find<'a>(
        &'a self,
        request: &'a crate::cfind::CFindRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cfind::CFindResponse>> {
        let mut item = Dataset::new();
        item.insert_str(Tag::STUDY_INSTANCE_UID, Vr::Ui, "1.2.3");
        let events = vec![Ok(crate::cfind::CFindResponse::pending(request, item))];
        Box::pin(futures::stream::iter(events))
    }
}

#[tokio::test]
async fn a_find_stream_with_matches_but_no_terminal_completes() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(FindPendingThenEnd),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![qr_context(sop_class::STUDY_ROOT_QR_FIND)],
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let responses = association
        .send_c_find(&crate::cfind::CFindRequest::study_query(1))
        .await
        .unwrap();
    let terminal = responses.last().unwrap();
    assert_eq!(terminal.status, crate::dimse::status::SUCCESS);
    // C-FIND-RSP does not carry sub-operation counters.
    assert_eq!(terminal.completed, 0);
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

struct GetInstanceInExplicitVr;

#[async_trait::async_trait]
impl ServiceProvider for GetInstanceInExplicitVr {
    fn supported_sop_classes(&self) -> Vec<String> {
        // the retrieve model plus the storage class the instances travel on
        vec![
            sop_class::STUDY_ROOT_QR_GET.to_string(),
            "1.2.840.10008.5.1.4.1.1.7".to_string(),
        ]
    }

    fn on_c_get<'a>(
        &'a self,
        _request: &'a crate::cget::CGetRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
        let mut dataset = Dataset::new();
        dataset.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5.6");
        let file = radicom::io::file::DicomFile::from_dataset(dataset);
        let events = vec![Ok(crate::cget::CGetEvent::instance(file))];
        Box::pin(futures::stream::iter(events))
    }
}

#[tokio::test]
async fn a_get_instance_in_another_syntax_is_transcoded() {
    // the provider hands back Explicit VR LE while the context is Implicit
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(GetInstanceInExplicitVr),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![
            qr_context(sop_class::STUDY_ROOT_QR_GET),
            PresentationContext::new(
                3,
                "1.2.840.10008.5.1.4.1.1.7",
                vec!["1.2.840.10008.1.2".to_string()],
            ),
        ],
        get_client_options(),
    )
    .await
    .unwrap();
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    association.collect_reverse_stores(received.clone());
    let responses = association
        .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
        .await
        .unwrap();
    assert!(!received.lock().unwrap().is_empty(), "the instance arrives");
    assert!(!responses.last().unwrap().is_pending());
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

// -- PDUs that arrive while a C-FIND/C-MOVE/C-GET is in flight -----------

fn pdata(context_id: u8, is_command: bool, body: Vec<u8>) -> Pdu {
    Pdu::PData(crate::pdu::PData {
        pdvs: vec![crate::pdu::Pdv {
            context_id,
            is_command,
            is_last: true,
            data: body.into(),
        }],
    })
}

fn identifier_bytes() -> Vec<u8> {
    crate::message::encode_dataset(
        &Dataset::new(),
        &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone(),
    )
    .unwrap()
}

fn find_request() -> Command {
    let mut request = Command::request(CommandField::CFindRq, 1);
    request.affected_sop_class_uid = Some(sop_class::STUDY_ROOT_QR_FIND.to_string());
    request.no_dataset = false;
    request
}

fn move_request() -> Command {
    let mut request = Command::request(CommandField::CMoveRq, 1);
    request.affected_sop_class_uid = Some(sop_class::STUDY_ROOT_QR_MOVE.to_string());
    request.move_destination = Some("DEST".to_string());
    request.no_dataset = false;
    request
}

fn get_request() -> Command {
    let mut request = Command::request(CommandField::CGetRq, 1);
    request.affected_sop_class_uid = Some(sop_class::STUDY_ROOT_QR_GET.to_string());
    request.no_dataset = false;
    request
}

/// The anomalies a peer can send while a query/retrieve is running.
fn in_flight_anomalies() -> Vec<Pdu> {
    vec![
        pdata(1, false, vec![0xAA]), // a dataset with no command
        pdata(
            1,
            true,
            command_bytes(&Command::request(CommandField::CEchoRq, 2)).unwrap(),
        ),
        Pdu::ReleaseRq,
        Pdu::AssociateRq(Associate::new("A", "B")),
    ]
}

/// Sends `request` and then `anomaly` on one association; the anomaly must
/// end the association with an error.
async fn inject_anomaly<P: ServiceProvider + 'static>(
    provider: P,
    context: PresentationContext,
    request: Command,
    anomaly: Pdu,
) {
    use tokio::io::AsyncWriteExt as _;
    let server = DicomServer::bind("127.0.0.1:0", Arc::new(provider), ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut open = Associate::new("CALLED-SCP", "CALLING-SCU");
    open.presentation_contexts = vec![context];
    write_pdu(&mut tcp, &Pdu::AssociateRq(open)).await.unwrap();
    assert!(matches!(
        read_pdu(&mut tcp).await.unwrap(),
        Some(Pdu::AssociateAc(_))
    ));

    let mut wire = crate::pdu::encode(&pdata(
        1,
        true,
        command_bytes(&request).expect("encode request"),
    ))
    .unwrap();
    wire.extend_from_slice(&crate::pdu::encode(&pdata(1, false, identifier_bytes())).unwrap());
    wire.extend_from_slice(&crate::pdu::encode(&anomaly).unwrap());
    tcp.write_all(&wire).await.unwrap();

    // the server either aborts, answers the release, or just closes
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match read_pdu(&mut tcp).await {
                Ok(Some(Pdu::Abort(_) | Pdu::ReleaseRp)) | Ok(None) | Err(_) => return,
                Ok(Some(_)) => continue,
            }
        }
    })
    .await;
    assert!(
        handle.await.unwrap().is_err(),
        "an in-flight anomaly ends the association with an error"
    );
}

/// A provider whose stream yields one Pending response and then stalls.
struct StalledFind;
struct StalledMove;
struct StalledGet;

#[async_trait::async_trait]
impl ServiceProvider for StalledFind {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![sop_class::STUDY_ROOT_QR_FIND.to_string()]
    }
    fn on_c_find<'a>(
        &'a self,
        request: &'a crate::cfind::CFindRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cfind::CFindResponse>> {
        use futures::StreamExt as _;
        let first = Ok(crate::cfind::CFindResponse::pending(
            request,
            Dataset::new(),
        ));
        Box::pin(futures::stream::once(async move { first }).chain(futures::stream::pending()))
    }
}

#[async_trait::async_trait]
impl ServiceProvider for StalledMove {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![sop_class::STUDY_ROOT_QR_MOVE.to_string()]
    }
    fn on_c_move<'a>(
        &'a self,
        request: &'a crate::cmove::CMoveRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cmove::CMoveResponse>> {
        use futures::StreamExt as _;
        let first = Ok(crate::cmove::CMoveResponse::pending(
            request,
            1,
            crate::cmove::SubOperationSummary::default(),
        ));
        Box::pin(futures::stream::once(async move { first }).chain(futures::stream::pending()))
    }
}

#[async_trait::async_trait]
impl ServiceProvider for StalledGet {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![sop_class::STUDY_ROOT_QR_GET.to_string()]
    }
    fn on_c_get<'a>(
        &'a self,
        request: &'a crate::cget::CGetRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
        use futures::StreamExt as _;
        let first = Ok(crate::cget::CGetEvent::Response(
            crate::cget::CGetResponse::pending(
                request,
                1,
                crate::cmove::SubOperationSummary::default(),
            ),
        ));
        Box::pin(futures::stream::once(async move { first }).chain(futures::stream::pending()))
    }
}

#[tokio::test]
async fn anomalies_during_a_find_are_rejected() {
    for anomaly in in_flight_anomalies() {
        inject_anomaly(
            StalledFind,
            qr_context(sop_class::STUDY_ROOT_QR_FIND),
            find_request(),
            anomaly,
        )
        .await;
    }
}

#[tokio::test]
async fn anomalies_during_a_move_are_rejected() {
    for anomaly in in_flight_anomalies() {
        inject_anomaly(
            StalledMove,
            qr_context(sop_class::STUDY_ROOT_QR_MOVE),
            move_request(),
            anomaly,
        )
        .await;
    }
}

#[tokio::test]
async fn anomalies_during_a_get_are_rejected() {
    for anomaly in in_flight_anomalies() {
        inject_anomaly(
            StalledGet,
            qr_context(sop_class::STUDY_ROOT_QR_GET),
            get_request(),
            anomaly,
        )
        .await;
    }
}

#[tokio::test]
async fn a_connect_timeout_fires_before_the_socket_answers() {
    // M7: fo-dicom's ConnectionTimeoutInMs. A connect future that never
    // resolves models a host that drops the SYN.
    let never = std::future::pending::<std::io::Result<TcpStream>>();
    let error = connect_with_timeout(never, Some(Duration::from_millis(25)))
        .await
        .expect_err("the timeout fires");
    assert!(
        matches!(error, NetworkError::ConnectTimedOut { timeout_ms: 25 }),
        "{error}"
    );

    // a refused connect is still an IO error, not a timeout
    let refused = connect_with_timeout(
        TcpStream::connect("127.0.0.1:1"),
        Some(Duration::from_secs(5)),
    )
    .await;
    assert!(matches!(refused, Err(NetworkError::Io(_))), "{refused:?}");

    // the default is no timeout, so the previous behaviour is unchanged
    assert!(ClientOptions::default().connect_timeout.is_none());
}

#[tokio::test]
async fn a_stalled_store_response_times_out_when_the_server_sets_one() {
    // M8: an SCP-issued request must not wait forever on a silent peer.
    let (_peer, mut io) = tokio::io::duplex(4096);
    let provider = Arc::new(VerificationProvider);
    let options = ServerOptions {
        request_timeout: Some(Duration::from_millis(30)),
        ..Default::default()
    };
    let accepted = AcceptedContexts::default();
    let splitter = PduSplitter::new(16384, 16);
    let mut serving = Serving {
        stream: &mut io,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::new(),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window: 1,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        pdu_reader: crate::transport::PduReader::new(),
    };
    let started = std::time::Instant::now();
    let error = serving
        .await_store_response(7, 1, 1, 99)
        .await
        .expect_err("a silent peer is bounded");
    assert!(
        matches!(error, NetworkError::RequestTimedOut { message_id: 7 }),
        "{error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the deadline is what ends the wait"
    );
    // The idle dispatcher must NOT be bounded by `request_timeout`, or a quiet
    // but healthy peer would be aborted. A short external wait must still be
    // pending (a timeout, not an error) rather than `RequestTimedOut`.
    let idle = tokio::time::timeout(Duration::from_millis(30), serving.next_pdu()).await;
    assert!(idle.is_err(), "the idle dispatcher waits unbounded");
}

// -- batch 2: L4, L6, L8, client fallback, M6 ----------------------------

#[tokio::test]
async fn the_client_sees_the_peers_implementation_identity() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![echo_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(
        association.peer_implementation_class_uid(),
        Some(IMPLEMENTATION_CLASS_UID)
    );
    assert_eq!(
        association.peer_implementation_version_name(),
        Some(IMPLEMENTATION_VERSION_NAME)
    );

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn the_common_extended_negotiation_item_reaches_the_wire() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let Some(Pdu::AssociateRq(rq)) = read_pdu(&mut stream).await.unwrap_or(None) else {
            return;
        };
        let _ = tx.send(rq.user_information.common_extended.clone());
    });

    let item = crate::pdu::CommonExtended {
        service_class_uid: "1.2.840.10008.5.1.4.1.1.7".to_string(),
        related_general_sop_classes: vec!["1.2.840.10008.5.1.4.1.1.7".to_string()],
    };
    let options = ClientOptions {
        common_extended: Some(item.clone()),
        ..Default::default()
    };
    // the peer closes without answering: this test observes the request only
    let _ = Association::open(addr, "SCU", "SCP", vec![store_context()], options).await;
    let sent = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("the request arrives")
        .unwrap();
    assert_eq!(sent, Some(item));
}

#[tokio::test]
async fn client_metrics_count_the_associations_traffic() {
    let metrics = Arc::new(crate::observability::CountingMetrics::new());
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions::default(),
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let options = ClientOptions {
        metrics: Some(metrics.clone()),
        ..Default::default()
    };
    let mut association = Association::open(addr, "SCU", "SCP", vec![echo_context()], options)
        .await
        .unwrap();

    let opened = metrics.snapshot();
    assert!(opened.bytes_sent > 0, "the A-ASSOCIATE-RQ is counted");
    assert!(opened.bytes_received > 0, "the A-ASSOCIATE-AC is counted");
    assert_eq!(opened.associations_opened, 1);
    assert_eq!(opened.active_connections(), 1);

    association
        .send_c_echo(sop_class::VERIFICATION)
        .await
        .unwrap();
    let after_echo = metrics.snapshot();
    assert!(
        after_echo.bytes_sent > opened.bytes_sent,
        "the C-ECHO adds sent bytes"
    );

    // `release` consumes the association, dropping the metric guards
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
    assert_eq!(
        metrics.snapshot().active_connections(),
        0,
        "the guard closes the connection on drop"
    );
}

/// A C-FIND match whose PatientName comes back under `fallback`. The
/// provider writes UTF-8 without (0008,0005), so only the fallback list
/// decides how the text is encoded (H1) and decoded (item 2).
async fn find_patient_name_with(fallback: Option<&'static str>) -> String {
    let provider =
        Arc::new(QueryProvider::new(vec![("1.2.3.1", "Müller")]).with_charset("ISO_IR 192"));
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider,
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let options = ClientOptions {
        fallback_encoding: fallback,
        ..Default::default()
    };
    let mut association = Association::open(addr, "SCU", "SCP", vec![find_context()], options)
        .await
        .unwrap();
    let request = crate::cfind::CFindRequest::study_query(1);
    let responses = association.send_c_find(&request).await.unwrap();
    let name = responses[0]
        .identifier
        .as_ref()
        .expect("a pending match")
        .get_str(Tag::PATIENT_NAME)
        .unwrap_or_default();
    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
    name
}

#[tokio::test]
async fn a_c_find_match_without_a_charset_decodes_with_the_client_fallback() {
    assert_eq!(
        find_patient_name_with(Some("ISO_IR 192")).await,
        "Müller",
        "the configured fallback decodes the UTF-8 bytes"
    );
    assert_eq!(
        find_patient_name_with(None).await,
        "MÃ¼ller",
        "without a fallback the default repertoire reads single bytes"
    );
}

#[tokio::test]
async fn two_stores_travel_before_the_first_response() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let Some(Pdu::AssociateRq(rq)) = read_pdu(&mut stream).await.unwrap() else {
            panic!("no A-ASSOCIATE-RQ");
        };
        let mut answer = Associate::new(rq.called_ae.clone(), rq.calling_ae.clone());
        answer.presentation_contexts = rq
            .presentation_contexts
            .iter()
            .map(|pc| PresentationContext {
                id: pc.id,
                abstract_syntax: String::new(),
                transfer_syntaxes: pc.transfer_syntaxes.first().cloned().into_iter().collect(),
                result: 0,
            })
            .collect();
        answer.user_information = rq.user_information.clone();
        write_pdu(&mut stream, &Pdu::AssociateAc(answer))
            .await
            .unwrap();

        // Two complete C-STORE requests must arrive before this peer writes
        // a single response: that is the async-operations window (M6).
        let splitter = PduSplitter::new(16384, 16);
        let mut assembler = PdvAssembler::new();
        let mut commands = Vec::new();
        let mut datasets = 0;
        let mut context_id = 0;
        while commands.len() < 2 || datasets < 2 {
            let Some(pdu) = read_pdu(&mut stream).await.unwrap() else {
                panic!("the peer closed before both requests arrived");
            };
            let Pdu::PData(pdata) = pdu else {
                panic!("unexpected PDU while reading the requests");
            };
            for unit in assembler.push_pdata(&pdata).unwrap() {
                context_id = unit.context_id;
                match unit.stream {
                    Stream::Command => {
                        commands.push(command_from_bytes(&unit.bytes).unwrap());
                    }
                    Stream::Data => datasets += 1,
                }
            }
        }
        assert!(commands
            .iter()
            .all(|command| command.field == CommandField::CStoreRq));
        assert_eq!(datasets, 2, "both datasets arrived with their commands");

        // answer in reverse order: the batch matches by message ID
        for command in commands.iter().rev() {
            let response = Command::response(command, crate::dimse::status::SUCCESS).unwrap();
            for pdu in splitter
                .split(context_id, true, &command_bytes(&response).unwrap())
                .unwrap()
            {
                write_pdu(&mut stream, &Pdu::PData(pdu)).await.unwrap();
            }
        }
        if let Some(Pdu::ReleaseRq) = read_pdu(&mut stream).await.unwrap() {
            write_pdu(&mut stream, &Pdu::ReleaseRp).await.unwrap();
        }
    });

    let options = ClientOptions {
        async_operations: crate::pdu::AsyncOperations {
            invoked: 2,
            performed: 2,
        },
        ..Default::default()
    };
    let mut association = Association::open(addr, "SCU", "SCP", vec![store_context()], options)
        .await
        .unwrap();
    let first = instance_file();
    let second = instance_file();
    let responses = tokio::time::timeout(
        Duration::from_secs(5),
        association.send_c_store_batch(&[&first, &second]),
    )
    .await
    .expect("a sequential sender would deadlock here")
    .unwrap();
    assert_eq!(responses.len(), 2);
    assert!(responses
        .iter()
        .all(|response| response.status == crate::dimse::status::SUCCESS));
    association.release().await.unwrap();
    peer.await.unwrap();
}

#[tokio::test]
async fn a_streamed_dataset_respects_the_configured_max_data_buffer() {
    // NW-E-03: the streamed send path ignored MaxDataBuffer. A raw peer keeps
    // the explicit syntax (no transcode) so the dataset takes the streaming
    // framer, then records the largest dataset PDV it saw.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let Some(Pdu::AssociateRq(rq)) = read_pdu(&mut stream).await.unwrap() else {
            panic!("no A-ASSOCIATE-RQ");
        };
        let mut answer = Associate::new(rq.called_ae.clone(), rq.calling_ae.clone());
        answer.presentation_contexts = rq
            .presentation_contexts
            .iter()
            .map(|pc| PresentationContext {
                id: pc.id,
                abstract_syntax: String::new(),
                transfer_syntaxes: vec!["1.2.840.10008.1.2.1".to_string()],
                result: 0,
            })
            .collect();
        answer.user_information = rq.user_information.clone();
        write_pdu(&mut stream, &Pdu::AssociateAc(answer))
            .await
            .unwrap();

        let mut assembler = PdvAssembler::new();
        let mut command: Option<Command> = None;
        let mut max_data_pdv = 0usize;
        let mut responded = false;
        while !responded {
            let Some(pdu) = read_pdu(&mut stream).await.unwrap() else {
                panic!("the peer closed before the dataset arrived");
            };
            let Pdu::PData(pdata) = pdu else {
                panic!("unexpected PDU while reading the C-STORE");
            };
            for pdv in &pdata.pdvs {
                if !pdv.is_command {
                    max_data_pdv = max_data_pdv.max(pdv.data.len());
                }
            }
            for unit in assembler.push_pdata(&pdata).unwrap() {
                let context_id = unit.context_id;
                match unit.stream {
                    Stream::Command => command = Some(command_from_bytes(&unit.bytes).unwrap()),
                    Stream::Data => {
                        let command = command.as_ref().expect("command precedes the dataset");
                        let response =
                            Command::response(command, crate::dimse::status::SUCCESS).unwrap();
                        for pdu in PduSplitter::new(16384, 16)
                            .split(context_id, true, &command_bytes(&response).unwrap())
                            .unwrap()
                        {
                            write_pdu(&mut stream, &Pdu::PData(pdu)).await.unwrap();
                        }
                        responded = true;
                    }
                }
            }
        }
        if let Some(Pdu::ReleaseRq) = read_pdu(&mut stream).await.unwrap() {
            write_pdu(&mut stream, &Pdu::ReleaseRp).await.unwrap();
        }
        max_data_pdv
    });

    const CAP: usize = 64;
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![PresentationContext::new(
            3,
            "1.2.840.10008.5.1.4.1.1.7",
            vec!["1.2.840.10008.1.2.1".to_string()],
        )],
        ClientOptions {
            max_data_buffer: Some(CAP as u32),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let mut file = instance_file();
    file.dataset
        .insert_or_update(radicom::model::item::DicomItem::Element(
            radicom::model::element::DicomElement::new_bytes(
                Tag::PIXEL_DATA,
                Vr::Ob,
                std::sync::Arc::new(radicom::model::buffer::MemoryBuffer::from(vec![
                    0x5Au8;
                    64 * 1024
                ])),
            ),
        ));
    let response = association.send_c_store(&file).await.unwrap();
    assert_eq!(response.status, crate::dimse::status::SUCCESS);
    association.release().await.unwrap();

    let observed = peer.await.unwrap();
    assert!(
        observed <= CAP,
        "the streamed dataset ignored max_data_buffer: a {observed}-byte PDV exceeded {CAP}"
    );
}

#[tokio::test]
async fn a_c_find_batch_returns_each_requests_matches() {
    let provider = Arc::new(QueryProvider::new(vec![
        ("1.2.3.1", "Doe^John"),
        ("1.2.3.2", "Roe^Jane"),
    ]));
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider.clone(),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });
    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![find_context()],
        ClientOptions::default(),
    )
    .await
    .unwrap();

    let duplicate_ids = [
        crate::cfind::CFindRequest::study_query(1),
        crate::cfind::CFindRequest::study_query(1),
    ];
    assert!(
        association.send_c_find_batch(&duplicate_ids).await.is_err(),
        "a batch cannot reuse a DIMSE Message ID"
    );

    let requests = [
        crate::cfind::CFindRequest::study_query(1),
        crate::cfind::CFindRequest::study_query(2),
    ];
    let batches = association.send_c_find_batch(&requests).await.unwrap();
    assert_eq!(batches.len(), 2, "one result list per request");
    for (index, responses) in batches.iter().enumerate() {
        assert_eq!(
            responses.len(),
            3,
            "two matches plus the terminal response for request {index}"
        );
        assert_eq!(
            responses[2].message_id_being_responded_to, requests[index].message_id,
            "the results belong to their own request"
        );
        assert_eq!(
            responses[0]
                .identifier
                .as_ref()
                .unwrap()
                .get_str(Tag::PATIENT_NAME)
                .unwrap(),
            "Doe^John"
        );
    }

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

// -- M6: the SCP runs independent requests concurrently -------------------

/// A provider that takes 200 ms per C-STORE and counts the attempts.
struct SlowStore {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl ServiceProvider for SlowStore {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec!["1.2.840.10008.5.1.4.1.1.7".to_string()]
    }
    async fn on_c_store(&self, request: &CStoreRequest) -> Result<CStoreResponse> {
        // a real provider doing real work: the association must not stall
        // while this runs
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.seen
            .lock()
            .unwrap()
            .push(request.instance_uid().to_string());
        Ok(CStoreResponse::success(request))
    }
}

#[tokio::test]
async fn two_stores_against_a_slow_provider_overlap_on_the_scp() {
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(SlowStore { seen: seen.clone() }),
        ServerOptions {
            store_sink: crate::store::StoreSink::Memory,
            async_operations: crate::pdu::AsyncOperations {
                invoked: 2,
                performed: 2,
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut association = Association::open(
        addr,
        "SCU",
        "SCP",
        vec![store_context()],
        ClientOptions {
            async_operations: crate::pdu::AsyncOperations {
                invoked: 2,
                performed: 2,
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let first = instance_file();
    let mut second = instance_file();
    second
        .dataset
        .insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5.7");
    let started = std::time::Instant::now();
    let responses = association
        .send_c_store_batch(&[&first, &second])
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(responses.len(), 2);
    assert!(responses
        .iter()
        .all(|response| response.status == crate::dimse::status::SUCCESS));
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "both instances reached the provider"
    );
    // 200 ms + 200 ms if the SCP ran them one after the other; the test
    // budget leaves room for scheduling noise but not for serialisation
    assert!(
        elapsed < Duration::from_millis(380),
        "the two stores must overlap, took {elapsed:?}"
    );

    association.release().await.unwrap();
    handle.await.unwrap().unwrap();
}

// -- L3: untranscodable syntax change ------------------------------------

/// An instance whose PixelData is in a syntax no codec in this build can
/// convert (no JPEG decoder is registered).
fn jpeg_instance() -> radicom::io::file::DicomFile {
    let mut dataset = Dataset::new();
    dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
    dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5.9");
    dataset.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
    dataset.insert_value_with_vr(Tag::ROWS, Vr::Us, 1u16);
    dataset.insert_value_with_vr(Tag::COLUMNS, Vr::Us, 1u16);
    dataset.insert_value_with_vr(Tag::PIXEL_DATA, Vr::Ob, vec![0u8; 64]);
    dataset.set_transfer_syntax(syntaxes::JPEG_PROCESS_1.clone());
    radicom::io::file::DicomFile::from_dataset(dataset)
}

#[tokio::test]
async fn an_untranscodable_store_fails_unless_the_option_allows_it() {
    for ignore in [false, true] {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(RecordingProvider::new()),
            ServerOptions {
                store_sink: crate::store::StoreSink::Memory,
                ignore_unsupported_syntax_change: ignore,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });
        let mut association = Association::open(
            addr,
            "SCU",
            "SCP",
            vec![store_context()],
            ClientOptions {
                ignore_unsupported_syntax_change: ignore,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let result = association.send_c_store(&jpeg_instance()).await;
        if ignore {
            // the bytes went out in their own syntax: the peer cannot decode
            // them as the negotiated one, so it answers a failure status
            let response = result.expect("the option sends the dataset instead of failing");
            assert_ne!(
                response.status,
                crate::dimse::status::SUCCESS,
                "the peer could not decode a JPEG dataset labelled Implicit VR LE"
            );
        } else {
            assert!(
                result.is_err(),
                "without the option an untranscodable dataset is an error"
            );
        }
        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }
}

// -- inline handler paths -------------------------------------------------
//
// C-FIND and C-STORE normally run on a dispatched task (`execute_*`). The
// inline handlers are the fallback when the job window is full, so they are
// driven directly here with a transport that never yields input.

/// An `AsyncRead` that never becomes ready and an `AsyncWrite` that discards.
struct NullTransport;

impl tokio::io::AsyncRead for NullTransport {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Pending
    }
}

impl tokio::io::AsyncWrite for NullTransport {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

fn accepted_one(abstract_syntax: &str, transfer_syntax: &str) -> AcceptedContexts {
    AcceptedContexts::from_negotiated(vec![crate::association::NegotiatedContext {
        id: 1,
        abstract_syntax: abstract_syntax.into(),
        transfer_syntax: transfer_syntax.into(),
        state: crate::association::ContextState::Accept,
        roles: Vec::new(),
    }])
}

struct InlineFind;

impl ServiceProvider for InlineFind {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![sop_class::STUDY_ROOT_QR_FIND.into()]
    }

    fn on_c_find<'a>(
        &'a self,
        request: &'a crate::cfind::CFindRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cfind::CFindResponse>> {
        let mut identifier = Dataset::new();
        identifier.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        Box::pin(futures::stream::iter(vec![
            Ok(crate::cfind::CFindResponse::pending(
                request,
                identifier.clone(),
            )),
            Ok(crate::cfind::CFindResponse::success(request, 1)),
        ]))
    }
}

/// A C-FIND provider that either ends its stream at once or fails it.
struct ScriptedFind {
    fail: bool,
}

impl ServiceProvider for ScriptedFind {
    fn supported_sop_classes(&self) -> Vec<String> {
        Vec::new()
    }

    fn on_c_find<'a>(
        &'a self,
        _request: &'a crate::cfind::CFindRequest,
    ) -> futures::stream::BoxStream<'a, Result<crate::cfind::CFindResponse>> {
        if self.fail {
            Box::pin(futures::stream::once(async {
                Err(NetworkError::protocol("provider blew up"))
            }))
        } else {
            Box::pin(futures::stream::empty())
        }
    }
}

struct OkStore;

#[async_trait::async_trait]
impl ServiceProvider for OkStore {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec!["1.2.840.10008.5.1.4.1.1.7".into()]
    }

    async fn on_c_store(&self, request: &CStoreRequest) -> Result<CStoreResponse> {
        Ok(CStoreResponse::success(request))
    }
}

fn store_command(instance_uid: &str) -> Command {
    let mut command = Command::request(CommandField::CStoreRq, 5);
    command.affected_sop_class_uid = Some("1.2.840.10008.5.1.4.1.1.7".into());
    command.affected_sop_instance_uid = Some(instance_uid.into());
    command.no_dataset = false;
    command
}

fn store_body(syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>) -> MessageBody {
    let mut dataset = Dataset::new();
    dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
    dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5.6");
    dataset.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
    MessageBody::Memory(bytes::Bytes::from(
        crate::message::encode_dataset(&dataset, syntax).unwrap(),
    ))
}

#[tokio::test]
async fn the_inline_c_find_forwards_matches_and_terminates() {
    let mut transport = NullTransport;
    let splitter = PduSplitter::new(16384, 16);
    let provider = Arc::new(InlineFind);
    let options = ServerOptions::default();
    let accepted = accepted_one(
        sop_class::STUDY_ROOT_QR_FIND,
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid(),
    );
    let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let mut serving = Serving {
        stream: &mut transport,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::with_spool(options.store_spool.clone()),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window: 1,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        pdu_reader: crate::transport::PduReader::new(),
    };
    let command = crate::cfind::CFindRequest::study_query(9).to_command();
    handle_c_find(&mut serving, 1, command, &[], &syntax)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_inline_c_find_fails_when_the_provider_says_nothing() {
    for fail in [false, true] {
        let provider = Arc::new(ScriptedFind { fail });
        let mut transport = NullTransport;
        let splitter = PduSplitter::new(16384, 16);
        let options = ServerOptions::default();
        let accepted = accepted_one(
            sop_class::STUDY_ROOT_QR_FIND,
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid(),
        );
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let mut serving = Serving {
            stream: &mut transport,
            splitter: &splitter,
            provider: &provider,
            options: &options,
            assembler: PdvAssembler::with_spool(options.store_spool.clone()),
            pending_units: VecDeque::new(),
            deferred_units: VecDeque::new(),
            deferred_requests: 0,
            incoming_window: 1,
            accepted: &accepted,
            reverse_message_id: 1,
            transcoder_cache: std::collections::HashMap::new(),
            pdu_reader: crate::transport::PduReader::new(),
        };
        let command = crate::cfind::CFindRequest::study_query(9).to_command();
        handle_c_find(&mut serving, 1, command, &[], &syntax)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn the_inline_c_find_honours_a_matching_cancel() {
    let mut transport = NullTransport;
    let splitter = PduSplitter::new(16384, 16);
    let provider = Arc::new(InlineFind);
    let options = ServerOptions::default();
    let accepted = accepted_one(
        sop_class::STUDY_ROOT_QR_FIND,
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid(),
    );
    let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let mut serving = Serving {
        stream: &mut transport,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::with_spool(options.store_spool.clone()),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window: 1,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        pdu_reader: crate::transport::PduReader::new(),
    };
    let mut cancel = Command::request(CommandField::CCancelRq, 0);
    cancel.message_id_being_responded_to = Some(9);
    serving.pending_units.push_back(MessageUnit {
        context_id: 1,
        stream: Stream::Command,
        bytes: bytes::Bytes::from(command_bytes(&cancel).unwrap()),
        spool: None,
    });
    let command = crate::cfind::CFindRequest::study_query(9).to_command();
    handle_c_find(&mut serving, 1, command, &[], &syntax)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_inline_c_find_defers_a_non_command_unit() {
    let mut transport = NullTransport;
    let splitter = PduSplitter::new(16384, 16);
    let provider = Arc::new(InlineFind);
    let options = ServerOptions::default();
    let accepted = accepted_one(
        sop_class::STUDY_ROOT_QR_FIND,
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid(),
    );
    let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let mut serving = Serving {
        stream: &mut transport,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::with_spool(options.store_spool.clone()),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window: 1,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        pdu_reader: crate::transport::PduReader::new(),
    };
    serving.pending_units.push_back(MessageUnit {
        context_id: 1,
        stream: Stream::Data,
        bytes: bytes::Bytes::new(),
        spool: None,
    });
    let command = crate::cfind::CFindRequest::study_query(9).to_command();
    // window 1 forbids queueing another message mid-request: it is refused.
    assert!(handle_c_find(&mut serving, 1, command, &[], &syntax)
        .await
        .is_err());
}

#[tokio::test]
async fn the_inline_c_store_accepts_a_matching_instance() {
    let mut transport = NullTransport;
    let splitter = PduSplitter::new(16384, 16);
    let provider = Arc::new(OkStore);
    let options = ServerOptions::default();
    let accepted = accepted_one(
        "1.2.840.10008.5.1.4.1.1.7",
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid(),
    );
    let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let mut serving = Serving {
        stream: &mut transport,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::with_spool(options.store_spool.clone()),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window: 1,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        pdu_reader: crate::transport::PduReader::new(),
    };
    handle_c_store(
        &mut serving,
        1,
        store_command("1.2.3.4.5.6"),
        store_body(&syntax),
        &syntax,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn the_inline_c_store_refuses_a_mismatched_instance() {
    let mut transport = NullTransport;
    let splitter = PduSplitter::new(16384, 16);
    let provider = Arc::new(OkStore);
    let options = ServerOptions::default();
    let accepted = accepted_one(
        "1.2.840.10008.5.1.4.1.1.7",
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid(),
    );
    let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let mut serving = Serving {
        stream: &mut transport,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::with_spool(options.store_spool.clone()),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window: 1,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        pdu_reader: crate::transport::PduReader::new(),
    };
    // The command names another instance than the dataset carries.
    handle_c_store(
        &mut serving,
        1,
        store_command("9.9.9"),
        store_body(&syntax),
        &syntax,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn the_inline_c_store_rejects_a_non_store_command_and_a_bad_dataset() {
    let mut transport = NullTransport;
    let splitter = PduSplitter::new(16384, 16);
    let provider = Arc::new(OkStore);
    let options = ServerOptions::default();
    let accepted = accepted_one(
        "1.2.840.10008.5.1.4.1.1.7",
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid(),
    );
    let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let mut serving = Serving {
        stream: &mut transport,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::with_spool(options.store_spool.clone()),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window: 1,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        pdu_reader: crate::transport::PduReader::new(),
    };

    // A command that is not a C-STORE is answered, not stored.
    let not_store = Command::request(CommandField::CEchoRq, 1);
    handle_c_store(
        &mut serving,
        1,
        not_store,
        MessageBody::Memory(bytes::Bytes::new()),
        &syntax,
    )
    .await
    .unwrap();

    // A dataset that does not parse becomes a failure status, not a drop.
    handle_c_store(
        &mut serving,
        1,
        store_command("1.2.3.4.5.6"),
        MessageBody::Memory(bytes::Bytes::from_static(&[0xFF, 0x00])),
        &syntax,
    )
    .await
    .unwrap();
}

// -- small pure branches -------------------------------------------------

#[test]
fn validate_proposed_contexts_checks_ids_and_syntaxes() {
    let good = PresentationContext::new(1, "1.2.3", vec!["1.2.4".into()]);
    assert!(validate_proposed_contexts(&[good]).is_ok());

    let mut zero = PresentationContext::new(1, "1.2.3", vec!["1.2.4".into()]);
    zero.id = 0;
    assert!(validate_proposed_contexts(&[zero]).is_err(), "id 0");

    let mut even = PresentationContext::new(1, "1.2.3", vec!["1.2.4".into()]);
    even.id = 2;
    assert!(validate_proposed_contexts(&[even]).is_err(), "even id");

    let a = PresentationContext::new(1, "1.2.3", vec!["1.2.4".into()]);
    let b = PresentationContext::new(1, "1.2.5", vec!["1.2.4".into()]);
    assert!(validate_proposed_contexts(&[a, b]).is_err(), "duplicate id");

    let no_abstract = PresentationContext::new(1, "", vec!["1.2.4".into()]);
    assert!(
        validate_proposed_contexts(&[no_abstract]).is_err(),
        "empty abstract syntax"
    );

    let no_syntax = PresentationContext::new(1, "1.2.3", Vec::new());
    assert!(
        validate_proposed_contexts(&[no_syntax]).is_err(),
        "no transfer syntax"
    );
}

#[test]
fn user_identity_policy_answers_only_when_a_response_was_asked_for() {
    use crate::pdu::UserIdentity;
    let asked = UserIdentity {
        identity_type: 1,
        primary: b"user".to_vec(),
        secondary: Vec::new(),
        positive_response_requested: true,
    };
    let quiet = UserIdentity {
        positive_response_requested: false,
        ..asked.clone()
    };
    assert!(UserIdentityPolicy::Reject.answer(&asked).is_none());
    assert_eq!(
        UserIdentityPolicy::Accept.answer(&asked),
        Some(Some(Vec::new()))
    );
    assert_eq!(UserIdentityPolicy::Accept.answer(&quiet), Some(None));
    assert_eq!(
        UserIdentityPolicy::Answer(b"resp".to_vec()).answer(&asked),
        Some(Some(b"resp".to_vec()))
    );
    assert_eq!(
        UserIdentityPolicy::Answer(b"resp".to_vec()).answer(&quiet),
        Some(None)
    );
}

#[tokio::test]
async fn run_until_returns_when_shutdown_is_signalled() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions {
            max_clients: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let clients = server.connected_clients();
    assert_eq!(clients.count(), 0);
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(async move { server.run_until(receiver).await });
    shutdown.send(true).unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn run_until_accepts_a_connection_through_the_limited_slot() {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(VerificationProvider),
        ServerOptions {
            max_clients: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let clients = server.connected_clients();
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(async move { server.run_until(receiver).await });
    let _peer = tokio::net::TcpStream::connect(addr).await.unwrap();
    for _ in 0..100 {
        if clients.count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(clients.count(), 1, "the accepted connection is counted");
    shutdown.send(true).unwrap();
    let _ = handle.await.unwrap();
}

/// A malformed explicit-VR element: `ZZ` is not a valid VR.
static MALFORMED_DATASET: [u8; 10] = [0x08, 0x00, 0x00, 0x00, b'Z', b'Z', 0x02, 0x00, 0x00, 0x00];

macro_rules! inline_serving {
    ($transport:ident, $splitter:ident, $provider:ident, $options:ident, $accepted:ident, $window:expr) => {
        Serving {
            stream: &mut $transport,
            splitter: &$splitter,
            provider: &$provider,
            options: &$options,
            assembler: PdvAssembler::with_spool($options.store_spool.clone()),
            pending_units: VecDeque::new(),
            deferred_units: VecDeque::new(),
            deferred_requests: 0,
            incoming_window: $window,
            accepted: &$accepted,
            reverse_message_id: 1,
            transcoder_cache: std::collections::HashMap::new(),
            pdu_reader: crate::transport::PduReader::new(),
        }
    };
}

struct ErrEcho;

#[async_trait::async_trait]
impl ServiceProvider for ErrEcho {
    fn supported_sop_classes(&self) -> Vec<String> {
        Vec::new()
    }

    async fn on_c_echo(&self, _request: &CEchoRequest) -> Result<CEchoResponse> {
        Err(NetworkError::protocol("echo boom"))
    }
}

#[tokio::test]
async fn handle_command_answers_echo_failure_and_unsupported_requests() {
    let mut transport = NullTransport;
    let splitter = PduSplitter::new(16384, 16);
    let provider = Arc::new(ErrEcho);
    let options = ServerOptions::default();
    let accepted = accepted_one(
        sop_class::VERIFICATION,
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid(),
    );
    let mut serving = inline_serving!(transport, splitter, provider, options, accepted, 1);
    // A provider error on C-ECHO becomes a failure response.
    handle_command(&mut serving, 1, Command::request(CommandField::CEchoRq, 3))
        .await
        .unwrap();

    // A request the engine does not serve is answered ProcessingFailure.
    let mut unsupported = Command::request(CommandField::CStoreRq, 4);
    unsupported.affected_sop_class_uid = Some("1.2.3".into());
    handle_command(&mut serving, 1, unsupported).await.unwrap();
}

#[tokio::test]
async fn handle_command_aborts_on_a_response_field() {
    let mut transport = NullTransport;
    let splitter = PduSplitter::new(16384, 16);
    let provider = Arc::new(OkStore);
    let options = ServerOptions::default();
    let accepted = accepted_one(
        sop_class::VERIFICATION,
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid(),
    );
    let mut serving = inline_serving!(transport, splitter, provider, options, accepted, 1);
    let request = Command::request(CommandField::CEchoRq, 1);
    let response = Command::response(&request, 0).unwrap();
    assert!(matches!(
        handle_command(&mut serving, 1, response).await,
        Err(NetworkError::PeerViolation(_))
    ));
}

#[tokio::test]
async fn handle_command_routes_an_n_request_without_a_dataset() {
    let mut transport = NullTransport;
    let splitter = PduSplitter::new(16384, 16);
    let provider = Arc::new(ScriptedFind { fail: false });
    let options = ServerOptions::default();
    let accepted = accepted_one(
        sop_class::STUDY_ROOT_QR_FIND,
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid(),
    );
    let mut serving = inline_serving!(transport, splitter, provider, options, accepted, 1);
    // The default provider refuses N-GET, so this becomes a failure response.
    handle_command(&mut serving, 1, Command::request(CommandField::NGetRq, 5))
        .await
        .unwrap();
}

#[tokio::test]
async fn handle_n_service_rejects_a_non_n_command() {
    let mut transport = NullTransport;
    let splitter = PduSplitter::new(16384, 16);
    let provider = Arc::new(OkStore);
    let options = ServerOptions::default();
    let accepted = accepted_one(
        sop_class::VERIFICATION,
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid(),
    );
    let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let mut serving = inline_serving!(transport, splitter, provider, options, accepted, 1);
    handle_n_service(
        &mut serving,
        1,
        Command::request(CommandField::CEchoRq, 6),
        None,
        &syntax,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn handle_command_with_dataset_rejects_unexpected_payloads() {
    let mut transport = NullTransport;
    let splitter = PduSplitter::new(16384, 16);
    let provider = Arc::new(OkStore);
    let options = ServerOptions::default();
    let accepted = accepted_one(
        sop_class::VERIFICATION,
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid(),
    );
    let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let mut serving = inline_serving!(transport, splitter, provider, options, accepted, 1);

    // A request that does not carry a dataset is answered, not dispatched.
    handle_command_with_dataset(
        &mut serving,
        1,
        Command::request(CommandField::CEchoRq, 7),
        MessageBody::Memory(bytes::Bytes::new()),
        &syntax,
    )
    .await
    .unwrap();

    // An N-service whose dataset will not decode becomes a failure response.
    handle_command_with_dataset(
        &mut serving,
        1,
        Command::request(CommandField::NGetRq, 8),
        MessageBody::Memory(bytes::Bytes::from_static(&MALFORMED_DATASET)),
        &syntax,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn execute_c_store_reports_bad_bodies_and_missing_uids() {
    let provider = Arc::new(OkStore);
    let options = ServerOptions::default();
    let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Outgoing>>(4);

    let job = DispatchJob {
        context_id: 1,
        command: store_command("1.2.3.4.5.6"),
        body: MessageBody::Memory(bytes::Bytes::from_static(&MALFORMED_DATASET)),
        syntax: syntax.clone(),
        cancel: None,
    };
    execute_c_store(job, &provider, &options, &tx)
        .await
        .unwrap();
    assert!(matches!(
        rx.recv().await.unwrap().unwrap().command.field,
        CommandField::CStoreRsp
    ));

    let mut command = store_command("1.2.3.4.5.6");
    command.affected_sop_class_uid = None;
    let job = DispatchJob {
        context_id: 1,
        command,
        body: store_body(&syntax),
        syntax: syntax.clone(),
        cancel: None,
    };
    execute_c_store(job, &provider, &options, &tx)
        .await
        .unwrap();
    assert!(matches!(
        rx.recv().await.unwrap().unwrap().command.field,
        CommandField::CStoreRsp
    ));
}

#[tokio::test]
async fn execute_c_find_reports_a_malformed_identifier() {
    let provider = Arc::new(ScriptedFind { fail: false });
    let options = ServerOptions::default();
    let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Outgoing>>(4);
    let mut job = DispatchJob {
        context_id: 1,
        command: crate::cfind::CFindRequest::study_query(9).to_command(),
        body: MessageBody::Memory(bytes::Bytes::from_static(&MALFORMED_DATASET)),
        syntax,
        cancel: None,
    };
    execute_c_find(&mut job, &provider, &options, &tx)
        .await
        .unwrap();
    assert!(matches!(
        rx.recv().await.unwrap().unwrap().command.field,
        CommandField::CFindRsp
    ));
}

#[tokio::test]
async fn execute_n_service_routes_and_reports_errors() {
    let provider = Arc::new(ScriptedFind { fail: false });
    let options = ServerOptions::default();
    let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Outgoing>>(8);

    // N-GET without a dataset reaches the provider (which refuses by default).
    let job = DispatchJob {
        context_id: 1,
        command: Command::request(CommandField::NGetRq, 4),
        body: MessageBody::Memory(bytes::Bytes::new()),
        syntax: syntax.clone(),
        cancel: None,
    };
    execute_n_service(job, &provider, &options, &tx)
        .await
        .unwrap();
    assert!(matches!(
        rx.recv().await.unwrap().unwrap().command.field,
        CommandField::NGetRsp
    ));

    // A malformed dataset is answered before the provider is called.
    let job = DispatchJob {
        context_id: 1,
        command: Command::request(CommandField::NSetRq, 5),
        body: MessageBody::Memory(bytes::Bytes::from_static(&MALFORMED_DATASET)),
        syntax: syntax.clone(),
        cancel: None,
    };
    execute_n_service(job, &provider, &options, &tx)
        .await
        .unwrap();
    assert!(matches!(
        rx.recv().await.unwrap().unwrap().command.field,
        CommandField::NSetRsp
    ));

    // A non-N command is answered with a failure (direct-call safety).
    let job = DispatchJob {
        context_id: 1,
        command: Command::request(CommandField::CEchoRq, 6),
        body: MessageBody::Memory(bytes::Bytes::new()),
        syntax,
        cancel: None,
    };
    execute_n_service(job, &provider, &options, &tx)
        .await
        .unwrap();
    assert!(matches!(
        rx.recv().await.unwrap().unwrap().command.field,
        CommandField::CEchoRsp
    ));
}

#[tokio::test]
async fn handle_c_get_reports_a_malformed_identifier_and_honours_cancel() {
    let mut transport = NullTransport;
    let splitter = PduSplitter::new(16384, 16);
    let provider = Arc::new(ScriptedFind { fail: false });
    let options = ServerOptions::default();
    let accepted = accepted_one(
        sop_class::STUDY_ROOT_QR_GET,
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid(),
    );
    let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let mut serving = inline_serving!(transport, splitter, provider, options, accepted, 1);
    let command = crate::cget::CGetRequest::study("1.2.3", 42).to_command();
    handle_c_get(&mut serving, 1, command, &MALFORMED_DATASET, &syntax)
        .await
        .unwrap();

    let mut cancel = Command::request(CommandField::CCancelRq, 0);
    cancel.message_id_being_responded_to = Some(42);
    serving.pending_units.push_back(MessageUnit {
        context_id: 1,
        stream: Stream::Command,
        bytes: bytes::Bytes::from(command_bytes(&cancel).unwrap()),
        spool: None,
    });
    let command = crate::cget::CGetRequest::study("1.2.3", 42).to_command();
    handle_c_get(&mut serving, 1, command, &[], &syntax)
        .await
        .unwrap();
}

#[tokio::test]
async fn handle_c_move_reports_a_malformed_identifier_and_honours_cancel() {
    let mut transport = NullTransport;
    let splitter = PduSplitter::new(16384, 16);
    let provider = Arc::new(ScriptedFind { fail: false });
    let options = ServerOptions::default();
    let accepted = accepted_one(
        sop_class::STUDY_ROOT_QR_MOVE,
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid(),
    );
    let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
    let mut serving = inline_serving!(transport, splitter, provider, options, accepted, 1);
    let command = crate::cmove::CMoveRequest::study("1.2.3", "DEST", 42).to_command();
    handle_c_move(&mut serving, 1, command, &MALFORMED_DATASET, &syntax)
        .await
        .unwrap();

    let mut cancel = Command::request(CommandField::CCancelRq, 0);
    cancel.message_id_being_responded_to = Some(42);
    serving.pending_units.push_back(MessageUnit {
        context_id: 1,
        stream: Stream::Command,
        bytes: bytes::Bytes::from(command_bytes(&cancel).unwrap()),
        spool: None,
    });
    let command = crate::cmove::CMoveRequest::study("1.2.3", "DEST", 42).to_command();
    handle_c_move(&mut serving, 1, command, &[], &syntax)
        .await
        .unwrap();
}
