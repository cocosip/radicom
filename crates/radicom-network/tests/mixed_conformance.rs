use async_trait::async_trait;
use radicom::io::file::DicomFile;
use radicom::model::{
    buffer::MemoryBuffer, element::DicomElement, item::DicomItem, transfer_syntax::syntaxes, vr::Vr,
};
use radicom::{Dataset, Tag};
use radicom_network::{
    batch::DicomRequest,
    cget::{CGetEvent, CGetRequest},
    client::{DicomClient, DicomClientOptions},
    pdu::AsyncOperations,
    server::{ClientOptions, DicomServer, ServerOptions},
    services::{CStoreRequest, CStoreResponse, ServiceProvider},
};
use std::sync::Arc;
use std::time::Duration;
const CLASS: &str = "1.2.840.10008.5.1.4.1.1.7";
struct Provider(DicomFile);
#[async_trait]
impl ServiceProvider for Provider {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![
            CLASS.into(),
            radicom_network::association::sop_class::STUDY_ROOT_QR_GET.into(),
        ]
    }
    fn on_c_get<'a>(
        &'a self,
        _r: &'a CGetRequest,
    ) -> futures::stream::BoxStream<'a, radicom_network::Result<CGetEvent>> {
        Box::pin(futures::stream::iter(vec![Ok(CGetEvent::instance(
            self.0.clone(),
        ))]))
    }
    async fn on_c_store(&self, r: &CStoreRequest) -> radicom_network::Result<CStoreResponse> {
        Ok(CStoreResponse::success(r))
    }
}
fn file() -> DicomFile {
    let mut d = Dataset::new();
    d.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, CLASS);
    d.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4");
    d.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
        Tag::PIXEL_DATA,
        Vr::Ob,
        Arc::new(MemoryBuffer::from(vec![0x55; 32 * 1024 * 1024])),
    )));
    d.set_transfer_syntax(syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone());
    DicomFile::from_dataset(d)
}
#[tokio::test]
async fn get_and_large_store_complete_with_async_window() {
    for (window, get_first) in [(1, true), (2, true), (2, false)] {
        let file = file();
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(Provider(file.clone())),
            ServerOptions {
                async_operations: AsyncOperations {
                    invoked: 2,
                    performed: 2,
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let task = tokio::spawn(async move { server.accept_one().await });
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
            .with_options(DicomClientOptions {
                connection: ClientOptions {
                    async_operations: AsyncOperations {
                        invoked: window,
                        performed: 2,
                    },
                    request_timeout: Some(Duration::from_secs(2)),
                    ..Default::default()
                },
                ..Default::default()
            })
            .with_reverse_store_handler(Arc::new(|r| Ok(CStoreResponse::success(r))));
        let mut requests = vec![
            DicomRequest::get(CGetRequest::study("1.2.3", 1), [CLASS]),
            DicomRequest::store(Arc::new(file)),
        ];
        if !get_first {
            requests.reverse();
        }
        let outcome = tokio::time::timeout(
            Duration::from_secs(4),
            client.send_requests(requests, |_| Ok(())),
        )
        .await;
        eprintln!("window={window}: {outcome:?}");
        assert!(outcome.expect("mixed batch must not deadlock").is_ok());
        task.abort();
        drop(client);
    }
}

#[tokio::test]
async fn mixed_stores_preserve_each_source_transfer_syntax() {
    let jpeg = DicomFile::open(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests-data/encapsulated/GH538-jpeg1.dcm"
    ))
    .unwrap();
    let mut native = file();
    native.dataset.insert_str(
        Tag::SOP_CLASS_UID,
        Vr::Ui,
        jpeg.dataset.get_str(Tag::SOP_CLASS_UID).unwrap(),
    );
    native.dataset.insert_value(Tag::ROWS, 1u16);
    native.dataset.insert_value(Tag::COLUMNS, 1u16);
    native.dataset.insert_value(Tag::BITS_ALLOCATED, 8u16);
    native.dataset.insert_value(Tag::BITS_STORED, 8u16);
    native.dataset.insert_value(Tag::HIGH_BIT, 7u16);
    native.dataset.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16);
    native.dataset.insert_value(Tag::PIXEL_REPRESENTATION, 0u16);
    native
        .dataset
        .insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
    native
        .dataset
        .insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::PIXEL_DATA,
            Vr::Ob,
            Arc::new(MemoryBuffer::from(vec![0x55; 2])),
        )));
    eprintln!(
        "JPEG class={} syntax={}",
        jpeg.dataset.get_str(Tag::SOP_CLASS_UID).unwrap(),
        jpeg.dataset.transfer_syntax().uid()
    );
    let class = jpeg.dataset.get_str(Tag::SOP_CLASS_UID).unwrap().to_owned();
    struct P(String);
    #[async_trait]
    impl ServiceProvider for P {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![self.0.clone()]
        }
        async fn on_c_store(&self, r: &CStoreRequest) -> radicom_network::Result<CStoreResponse> {
            Ok(CStoreResponse::success(r))
        }
    }
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(P(class)),
        ServerOptions {
            supported_transfer_syntaxes: vec![
                jpeg.dataset.transfer_syntax().uid().into(),
                syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.uid().into(),
            ],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let task = tokio::spawn(async move { server.accept_one().await });
    let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
    let mut count = 0;
    let outcome = client
        .send_requests(
            vec![
                DicomRequest::store(Arc::new(jpeg)),
                DicomRequest::store(Arc::new(native)),
            ],
            |_| {
                count += 1;
                Ok(())
            },
        )
        .await;
    eprintln!("mixed_store_syntax_outcome={outcome:?} responses={count}");
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(count, 2);
    task.abort();
}

#[tokio::test]
async fn cancellation_routes_each_live_mixed_request() {
    use radicom_network::cfind::{CFindRequest, CFindResponse};
    use radicom_network::dimse::status;
    struct P;
    #[async_trait]
    impl ServiceProvider for P {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![
                radicom_network::association::sop_class::STUDY_ROOT_QR_FIND.into(),
                radicom_network::association::sop_class::PATIENT_ROOT_QR_FIND.into(),
            ]
        }
        fn on_c_find<'a>(
            &'a self,
            r: &'a CFindRequest,
        ) -> futures::stream::BoxStream<'a, radicom_network::Result<CFindResponse>> {
            use futures::StreamExt;
            Box::pin(
                futures::stream::iter(vec![Ok(CFindResponse::pending(r, Dataset::new()))])
                    .chain(futures::stream::pending()),
            )
        }
    }
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(P),
        ServerOptions {
            async_operations: AsyncOperations {
                invoked: 2,
                performed: 2,
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr();
    let task = tokio::spawn(async move { server.accept_one().await });
    let mut client =
        DicomClient::new("SCU", "SCP", addr.to_string()).with_options(DicomClientOptions {
            connection: ClientOptions {
                async_operations: AsyncOperations {
                    invoked: 2,
                    performed: 2,
                },
                request_timeout: Some(Duration::from_secs(2)),
                ..Default::default()
            },
            ..Default::default()
        });
    let cancel = client.cancellation_handle();
    let mut pending = std::collections::HashSet::new();
    let mut completed = std::collections::HashSet::new();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        client.send_requests(
            vec![
                CFindRequest::study_query(42),
                CFindRequest::patient_query(0),
            ],
            |event| {
                if event.is_final {
                    let radicom_network::batch::DicomResponse::CFind(response) = event.response
                    else {
                        panic!("expected FIND")
                    };
                    assert_eq!(response.status, status::CANCEL);
                    completed.insert(event.message_id);
                } else {
                    pending.insert(event.message_id);
                    if pending.len() == 2 {
                        for id in &pending {
                            assert!(cancel.cancel(*id));
                        }
                    }
                }
                Ok(())
            },
        ),
    )
    .await
    .expect("cancelled requests must finish");
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(completed.len(), 2);
    for id in completed {
        assert!(!cancel.cancel(id));
    }
    task.abort();
}

#[tokio::test]
async fn cancellation_works_when_a_later_mixed_request_reuses_the_id() {
    use futures::StreamExt;
    use radicom_network::cfind::{CFindRequest, CFindResponse};
    use radicom_network::dimse::status;

    struct P;
    #[async_trait]
    impl ServiceProvider for P {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![radicom_network::association::sop_class::STUDY_ROOT_QR_FIND.into()]
        }
        fn on_c_find<'a>(
            &'a self,
            request: &'a CFindRequest,
        ) -> futures::stream::BoxStream<'a, radicom_network::Result<CFindResponse>> {
            Box::pin(
                futures::stream::once(async move {
                    Ok(CFindResponse::pending(request, Dataset::new()))
                })
                .chain(futures::stream::pending()),
            )
        }
    }

    let server = DicomServer::bind("127.0.0.1:0", Arc::new(P), ServerOptions::default())
        .await
        .unwrap();
    let addr = server.local_addr();
    let task = tokio::spawn(async move { server.accept_one().await });
    let mut client =
        DicomClient::new("SCU", "SCP", addr.to_string()).with_options(DicomClientOptions {
            connection: ClientOptions {
                request_timeout: Some(Duration::from_secs(2)),
                ..Default::default()
            },
            ..Default::default()
        });
    let cancel = client.cancellation_handle();
    let mut pending_ids = Vec::new();
    let mut completed = Vec::new();
    let outcome = client
        .send_requests(
            vec![CFindRequest::study_query(1), CFindRequest::study_query(0)],
            |event| {
                if event.is_final {
                    let radicom_network::batch::DicomResponse::CFind(response) = event.response
                    else {
                        panic!("expected FIND")
                    };
                    assert_eq!(response.status, status::CANCEL);
                    completed.push(event.request_index);
                } else {
                    pending_ids.push(event.message_id);
                    assert!(cancel.cancel(event.message_id));
                }
                Ok(())
            },
        )
        .await;
    task.abort();
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(pending_ids, [1, 1]);
    assert_eq!(completed, [0, 1]);
    assert!(!cancel.cancel(1));
}

#[tokio::test]
async fn association_accept_ignores_reserved_ae_bytes_and_unknown_version_bits_and_rejected_syntax()
{
    use radicom_network::{
        association::sop_class,
        pdu::{Associate, Pdu, PresentationContext},
        server::Association,
        transport::{read_pdu, write_pdu},
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let Some(Pdu::AssociateRq(request)) = read_pdu(&mut stream).await.unwrap() else {
            panic!("expected request");
        };
        let mut answer = Associate::new("RESERVED-CALLED", "RESERVED-CALLING");
        answer.protocol_version = 3;
        answer.user_information = request.user_information;
        answer.presentation_contexts = request
            .presentation_contexts
            .into_iter()
            .enumerate()
            .map(|(index, context)| PresentationContext {
                id: context.id,
                abstract_syntax: String::new(),
                result: if index == 0 { 0 } else { 3 },
                transfer_syntaxes: vec![syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.uid().into()],
            })
            .collect();
        write_pdu(&mut stream, &Pdu::AssociateAc(answer))
            .await
            .unwrap();
        assert!(matches!(
            read_pdu(&mut stream).await.unwrap(),
            Some(Pdu::ReleaseRq)
        ));
        write_pdu(&mut stream, &Pdu::ReleaseRp).await.unwrap();
    });
    let contexts = vec![
        PresentationContext::new(
            1,
            sop_class::VERIFICATION,
            vec![syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.uid().into()],
        ),
        PresentationContext::new(
            3,
            sop_class::STUDY_ROOT_QR_FIND,
            vec![syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid().into()],
        ),
    ];
    let association = Association::open(addr, "SCU", "SCP", contexts, ClientOptions::default())
        .await
        .expect("valid partial acceptance");
    assert_eq!(association.contexts().all().len(), 1);
    association.release().await.unwrap();
    peer.await.unwrap();
}
