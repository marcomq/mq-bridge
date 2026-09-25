mod config {
    use crate::models::GrpcConfig;

    #[test]
    fn legacy_grpc_config_keys_still_deserialize() {
        let config: GrpcConfig = serde_json::from_value(serde_json::json!({
            "url": "http://127.0.0.1:50051",
            "timeout_ms": 250,
            "server_streaming": true
        }))
        .unwrap();

        assert_eq!(config.timeout_ms, Some(250));
        assert!(config.server_streaming);
    }
}

mod consumer {
    use super::super::consumer::*;
    use super::super::publisher::GrpcPublisher;
    use super::super::{proto, BridgeMessage, CanonicalMessage};
    use crate::models::{Endpoint, EndpointType, GrpcConfig, Route};
    use crate::traits::{MessageConsumer, MessageDisposition, MessagePublisher, SentBatch};
    use proto::bridge_client::BridgeClient;
    use proto::bridge_server::{Bridge, BridgeServer};
    use proto::{PublishResponse, SubscribeRequest};
    use std::time::Duration;
    use tokio::sync::{broadcast, mpsc};
    use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
    use tonic::transport::Server as TonicServer;
    use tonic::{Request, Response, Status};

    struct MockBridge {
        tx: broadcast::Sender<BridgeMessage>,
    }

    #[tonic::async_trait]
    impl Bridge for MockBridge {
        async fn publish(
            &self,
            request: Request<BridgeMessage>,
        ) -> Result<Response<PublishResponse>, Status> {
            // The receiver can be dropped if no subscriber is active. We can ignore the error.
            let msg = request.into_inner();
            let msg_id = msg.id.clone();
            let _ = self.tx.send(msg);
            Ok(Response::new(PublishResponse {
                result: Some(proto::publish_response::Result::Ack(proto::Ack {
                    id: msg_id,
                    status: 0,
                    reason: String::new(),
                    metadata: Default::default(),
                })),
            }))
        }

        async fn acknowledge(
            &self,
            request: Request<proto::Ack>,
        ) -> Result<Response<proto::AckResponse>, Status> {
            let _ = request.into_inner();
            Ok(Response::new(proto::AckResponse {
                success: true,
                error: String::new(),
            }))
        }

        type PublishBatchStream = ReceiverStream<Result<PublishResponse, Status>>;

        async fn publish_batch(
            &self,
            request: Request<tonic::Streaming<BridgeMessage>>,
        ) -> Result<Response<Self::PublishBatchStream>, Status> {
            let mut stream = request.into_inner();
            let (tx, rx) = mpsc::channel(32);
            let sender = self.tx.clone();

            tokio::spawn(async move {
                while let Ok(Some(msg_result)) = stream.message().await {
                    let msg_id = msg_result.id.clone();
                    let _ = sender.send(msg_result);
                    let resp = PublishResponse {
                        result: Some(proto::publish_response::Result::Ack(proto::Ack {
                            id: msg_id,
                            status: 0,
                            reason: String::new(),
                            metadata: Default::default(),
                        })),
                    };
                    if tx.send(Ok(resp)).await.is_err() {
                        break;
                    }
                }
            });

            Ok(Response::new(ReceiverStream::new(rx)))
        }

        type SubscribeStream = ReceiverStream<Result<BridgeMessage, Status>>;

        async fn subscribe(
            &self,
            _request: Request<SubscribeRequest>,
        ) -> Result<Response<Self::SubscribeStream>, Status> {
            let mut rx = self.tx.subscribe();
            let (tx_stream, rx_stream) = mpsc::channel(10);

            // Spawn a task to bridge broadcast::Receiver to mpsc::Sender for the tonic stream
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(msg) => {
                            if tx_stream.send(Ok(msg)).await.is_err() {
                                // Downstream consumer has disconnected, so we stop.
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // This means the consumer is slow, and we skipped some messages.
                            // In a real-world scenario, you might want to log this.
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            // The sender is gone, no more messages will come.
                            break;
                        }
                    }
                }
            });

            Ok(Response::new(ReceiverStream::new(rx_stream)))
        }
    }

    #[tokio::test]
    async fn test_grpc_publisher_and_consumer() {
        // Bind an ephemeral port and start the server using that listener so tests
        // don't rely on a hardcoded port.
        let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let local = listener.local_addr().unwrap();
        let (tx, _) = broadcast::channel(16);
        let mut rx_for_pub_test = tx.subscribe();
        let bridge = MockBridge { tx: tx.clone() };

        let incoming: TcpListenerStream = TcpListenerStream::new(listener);
        let server_handle = tokio::spawn(async move {
            TonicServer::builder()
                .serve_with_incoming(BridgeServer::new(bridge), incoming)
                .await
                .unwrap();
        });

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let config = GrpcConfig {
            url: format!("http://{}", local),
            timeout_ms: None,
            topic: Some("test_topic".to_string()),
            ..Default::default()
        };

        let publisher_ep = Endpoint {
            endpoint_type: EndpointType::Grpc(config.clone()),
            middlewares: vec![],
            handler: None,
        };
        let publisher = Route::new(Endpoint::new_memory("in", 10), publisher_ep)
            .create_publisher()
            .await
            .expect("Failed to create publisher");

        let sent_payload = "hello_grpc";
        publisher
            .send(sent_payload.into())
            .await
            .expect("Failed to send");

        // Verify the mock server received the message from the publisher
        let received_msg = rx_for_pub_test.recv().await.unwrap();
        assert_eq!(received_msg.payload, sent_payload.as_bytes());

        let consumer_ep = Endpoint {
            endpoint_type: EndpointType::Grpc(config),
            middlewares: vec![],
            handler: None,
        };
        // Create the consumer first. This will establish the subscription inside `new()`.
        let mut consumer = consumer_ep.create_consumer("test_route").await.unwrap();

        tx.send(BridgeMessage {
            payload: b"grpc_payload_1".to_vec(),
            id: "0190163d-8694-739b-aea5-966c26f8ad90".to_string(),
            metadata: Default::default(),
        })
        .unwrap();
        tx.send(BridgeMessage {
            payload: b"grpc_payload_2".to_vec(),
            id: "0190163d-8694-739b-aea5-966c26f8ad91".to_string(),
            metadata: Default::default(),
        })
        .unwrap();

        let batch = consumer.receive_batch(5).await.unwrap();
        assert_eq!(batch.messages.len(), 2);
        assert_eq!(batch.messages[0].get_payload_str(), "grpc_payload_1");
        assert_eq!(batch.messages[1].get_payload_str(), "grpc_payload_2");

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_grpc_route_end_to_end() {
        let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let local = listener.local_addr().unwrap();
        let (tx, _) = broadcast::channel(32);
        let bridge = MockBridge { tx };

        let incoming = TcpListenerStream::new(listener);
        let server_handle = tokio::spawn(async move {
            TonicServer::builder()
                .serve_with_incoming(BridgeServer::new(bridge), incoming)
                .await
                .unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let config = GrpcConfig {
            url: format!("http://{}", local),
            timeout_ms: None,
            topic: Some("e2e_test_topic".to_string()),
            ..Default::default()
        };

        // Source for sending messages into the system
        let mem_source_topic = format!("e2e_in_{}", fast_uuid_v7::gen_id_str());
        let mem_dest_topic = format!("e2e_out_{}", fast_uuid_v7::gen_id_str());
        let mem_source_ep = Endpoint::new_memory(&mem_source_topic, 10);
        let mem_source_publisher = mem_source_ep.create_publisher("mem_source").await.unwrap();

        // The gRPC endpoint that will publish messages to our mock server
        let grpc_publisher_ep = Endpoint {
            endpoint_type: EndpointType::Grpc(config.clone()),
            middlewares: vec![],
            handler: None,
        };

        // The gRPC endpoint that will consume messages from our mock server
        let grpc_consumer_ep = Endpoint {
            endpoint_type: EndpointType::Grpc(config),
            middlewares: vec![],
            handler: None,
        };

        // The final destination for messages
        let mem_dest_ep = Endpoint::new_memory(&mem_dest_topic, 10);
        let mut mem_dest_consumer = mem_dest_ep.create_consumer("test_route").await.unwrap();

        // Setup and run routes using deploy()
        // Route 1: Memory -> gRPC (tests GrpcPublisher::send_batch)
        let route_to_grpc = Route::new(mem_source_ep, grpc_publisher_ep);
        route_to_grpc.deploy("route_to_grpc").await.unwrap();

        // Route 2: gRPC -> Memory (tests GrpcConsumer::receive_batch)
        let route_from_grpc = Route::new(grpc_consumer_ep, mem_dest_ep);
        route_from_grpc.deploy("route_from_grpc").await.unwrap();

        // Execute test: Send a batch of messages into the first route
        let messages_to_send = vec![
            CanonicalMessage::new("e2e_payload_1".into(), None),
            CanonicalMessage::new("e2e_payload_2".into(), None),
        ];
        mem_source_publisher
            .send_batch(messages_to_send.clone())
            .await
            .unwrap();

        // Verify: Receive the batch from the second route's destination
        let mut received_messages = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while received_messages.len() < messages_to_send.len() {
                let batch = mem_dest_consumer.receive_batch(5).await.unwrap();
                received_messages.extend(batch.messages);
            }
        })
        .await
        .expect("gRPC route did not deliver the expected messages");

        assert_eq!(received_messages.len(), messages_to_send.len());
        assert_eq!(
            received_messages[0].get_payload_str(),
            messages_to_send[0].get_payload_str()
        );
        assert_eq!(
            received_messages[1].get_payload_str(),
            messages_to_send[1].get_payload_str()
        );

        server_handle.abort();
    }
    #[tokio::test]
    async fn acknowledge_and_batch_streaming_round_trip() {
        let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let local = listener.local_addr().unwrap();
        let (tx, _) = broadcast::channel(16);
        let bridge = MockBridge { tx: tx.clone() };

        let incoming = TcpListenerStream::new(listener);
        let server_handle = tokio::spawn(async move {
            TonicServer::builder()
                .serve_with_incoming(BridgeServer::new(bridge), incoming)
                .await
                .unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let config = GrpcConfig {
            url: format!("http://{}", local),
            timeout_ms: None,
            topic: Some("batch_test_topic".to_string()),
            ..Default::default()
        };

        let mut consumer = ClientModeConsumer::new(&config, &config.url)
            .await
            .expect("Failed to create ClientModeConsumer");
        let publisher = GrpcPublisher::new(&config)
            .await
            .expect("Failed to create GrpcPublisher");

        let msgs = vec![
            CanonicalMessage::new("batch_1".into(), None),
            CanonicalMessage::new("batch_2".into(), None),
        ];

        // The mock answers with an Ack variant, which maps to SentBatch::Ack.
        let sent_result = publisher.send_batch(msgs).await;
        assert!(matches!(sent_result, Ok(SentBatch::Ack)));

        let received = tokio::time::timeout(Duration::from_secs(1), consumer.receive_batch(2))
            .await
            .expect("subscription timed out")
            .expect("subscription failed");
        assert_eq!(received.messages.len(), 2);
        (received.commit)(vec![MessageDisposition::Ack; 2])
            .await
            .expect("acknowledge failed");

        // Explicit Acknowledge, outside the route commit path.
        let mut client = BridgeClient::new(
            tonic::transport::Endpoint::from_shared(config.url.clone())
                .unwrap()
                .connect()
                .await
                .unwrap(),
        );
        let ack_req = tonic::Request::new(proto::Ack {
            id: fast_uuid_v7::gen_id_str().to_string(),
            status: 0,
            reason: String::new(),
            metadata: Default::default(),
        });

        let ack_resp = client.acknowledge(ack_req).await;
        assert!(ack_resp.is_ok());
        assert!(ack_resp.unwrap().into_inner().success);

        server_handle.abort();
    }
}

mod dynamic {
    use super::super::consumer::ClientModeConsumer;
    use super::super::dynamic::*;
    use super::super::publisher::GrpcPublisher;
    use super::super::server::{PrefixRouter, ServerModeConsumer, REFLECTION_V1_PREFIX};
    use super::super::GrpcStatusError;
    use crate::models::{GrpcConfig, SecretExtractor, TlsConfig};
    use crate::traits::{
        ConsumerError, MessageConsumer, MessagePublisher, PublisherError, SentBatch,
    };
    use crate::CanonicalMessage;
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::metadata::MetadataMap;
    use tonic::transport::{Identity, Server as TonicServer, ServerTlsConfig};
    use tonic::{Request, Response, Status};

    mod dynamic_fixture {
        tonic::include_proto!("mqbridge.test.v1");
        pub const FILE_DESCRIPTOR_SET: &[u8] =
            tonic::include_file_descriptor_set!("grpc_dynamic_test_descriptor");
    }

    #[derive(Default)]
    struct DynamicFixtureService;

    #[tonic::async_trait]
    impl dynamic_fixture::dynamic_fixture_server::DynamicFixture for DynamicFixtureService {
        async fn unary(
            &self,
            request: Request<dynamic_fixture::DynamicRequest>,
        ) -> Result<Response<dynamic_fixture::DynamicResponse>, Status> {
            if request.get_ref().sequence == 98 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if request.get_ref().sequence == -1 {
                let mut metadata = tonic::metadata::MetadataMap::new();
                metadata.insert("error-detail", "secret-trailer".parse().unwrap());
                return Err(Status::with_metadata(
                    tonic::Code::InvalidArgument,
                    "invalid fixture request",
                    metadata,
                ));
            }
            let auth = request
                .metadata()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            Ok(Response::new(dynamic_fixture::DynamicResponse {
                data: request.get_ref().data.clone(),
                sequence: request.get_ref().sequence,
                auth,
            }))
        }

        type StreamStream = std::pin::Pin<
            Box<
                dyn futures::Stream<Item = Result<dynamic_fixture::DynamicResponse, Status>> + Send,
            >,
        >;

        async fn stream(
            &self,
            request: Request<dynamic_fixture::DynamicRequest>,
        ) -> Result<Response<Self::StreamStream>, Status> {
            let input = request.into_inner();
            if input.sequence == 99 {
                let response = dynamic_fixture::DynamicResponse {
                    data: input.data,
                    sequence: input.sequence,
                    auth: String::new(),
                };
                return Ok(Response::new(Box::pin(futures::stream::once(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok(response)
                }))));
            }
            let responses = (0..2).map(move |offset| {
                Ok(dynamic_fixture::DynamicResponse {
                    data: input.data.clone(),
                    sequence: input.sequence + offset,
                    auth: String::new(),
                })
            });
            Ok(Response::new(Box::pin(tokio_stream::iter(responses))))
        }

        /// Sums the sequences it received and echoes the count via `data`, so a test can
        /// prove every streamed request arrived in one RPC.
        async fn client_stream(
            &self,
            request: Request<tonic::Streaming<dynamic_fixture::DynamicRequest>>,
        ) -> Result<Response<dynamic_fixture::DynamicResponse>, Status> {
            let auth = request
                .metadata()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let mut stream = request.into_inner();
            let mut total = 0_i64;
            let mut count = 0_u8;
            while let Some(message) = stream.message().await? {
                if message.sequence == -1 {
                    return Err(Status::invalid_argument("fixture rejects sequence -1"));
                }
                total += message.sequence;
                count += 1;
            }
            Ok(Response::new(dynamic_fixture::DynamicResponse {
                data: vec![count],
                sequence: total,
                auth,
            }))
        }

        type BidiStream = std::pin::Pin<
            Box<
                dyn futures::Stream<Item = Result<dynamic_fixture::DynamicResponse, Status>> + Send,
            >,
        >;

        async fn bidi(
            &self,
            _request: Request<tonic::Streaming<dynamic_fixture::DynamicRequest>>,
        ) -> Result<Response<Self::BidiStream>, Status> {
            Err(Status::unimplemented("fixture"))
        }
    }

    /// The gRPC certs are generated in-process: these are unit tests, so they must not
    /// depend on `gen_certs.sh` (the Windows runner has no bash/openssl).
    fn grpc_cert_dir() -> &'static std::path::Path {
        static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        DIR.get_or_init(|| {
            use rcgen::{
                BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer,
                KeyPair, KeyUsagePurpose,
            };
            let hosts = vec!["localhost".to_string(), "127.0.0.1".to_string()];

            let mut ca_params = CertificateParams::new(hosts.clone()).unwrap();
            ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
            let ca_key = KeyPair::generate().unwrap();
            let ca = ca_params.self_signed(&ca_key).unwrap();
            let issuer = Issuer::new(ca_params, ca_key);

            let mut server_params = CertificateParams::new(hosts).unwrap();
            server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            let server_key = KeyPair::generate().unwrap();
            let server = server_params.signed_by(&server_key, &issuer).unwrap();

            let dir =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/grpc-test-certs");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("ca.pem"), ca.pem()).unwrap();
            std::fs::write(dir.join("server.crt"), server.pem()).unwrap();
            std::fs::write(dir.join("server.key"), server_key.serialize_pem()).unwrap();
            dir
        })
    }

    async fn dynamic_fixture_server_with_tls(
        tls: bool,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        if tls {
            #[cfg(feature = "rustls-aws-lc")]
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            #[cfg(not(feature = "rustls-aws-lc"))]
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let incoming = TcpListenerStream::new(listener);
        let reflection = tonic_reflection::server::Builder::configure()
            .register_encoded_file_descriptor_set(dynamic_fixture::FILE_DESCRIPTOR_SET)
            .build_v1()
            .unwrap();
        let mut builder = TonicServer::builder();
        if tls {
            let certs = grpc_cert_dir();
            let identity = Identity::from_pem(
                std::fs::read(certs.join("server.crt")).unwrap(),
                std::fs::read(certs.join("server.key")).unwrap(),
            );
            builder = builder
                .tls_config(ServerTlsConfig::new().identity(identity))
                .unwrap();
        }
        let handle = tokio::spawn(async move {
            builder
                .serve_with_incoming(
                    PrefixRouter {
                        fallback:
                            dynamic_fixture::dynamic_fixture_server::DynamicFixtureServer::new(
                                DynamicFixtureService,
                            ),
                        prefix: REFLECTION_V1_PREFIX,
                        matched: reflection,
                    },
                    incoming,
                )
                .await
                .unwrap();
        });
        (address, handle)
    }

    async fn dynamic_fixture_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        dynamic_fixture_server_with_tls(false).await
    }

    async fn dynamic_tls_fixture_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        dynamic_fixture_server_with_tls(true).await
    }

    fn dynamic_config(address: std::net::SocketAddr, method: &str) -> GrpcConfig {
        GrpcConfig::new(format!("http://{address}"))
            .with_descriptor_set_bytes(dynamic_fixture::FILE_DESCRIPTOR_SET.to_vec())
            .with_service_name("mqbridge.test.v1.DynamicFixture")
            .with_method_name(method)
            .with_request(serde_json::json!({
                "data": "aGVsbG8=",
                "sequence": "7"
            }))
    }

    fn dynamic_tls_config(address: std::net::SocketAddr, method: &str) -> GrpcConfig {
        let mut config = dynamic_config(address, method);
        config.url = format!("https://{address}");
        config.tls =
            TlsConfig::new().with_ca_file(grpc_cert_dir().join("ca.pem").to_string_lossy());
        config
    }

    #[tokio::test]
    async fn dynamic_unary_uses_canonical_json_metadata_and_stable_ids() {
        let (address, handle) = dynamic_tls_fixture_server().await;
        let mut config = dynamic_tls_config(address, "Unary")
            .with_bearer_token("test-token")
            .with_metadata(HashMap::from([("x-static".into(), "value".into())]));
        config.server_streaming = true; // Deprecated hint must not override the descriptor.

        let mut first = DynamicConsumer::new(&config, &config.url).await.unwrap();
        let first_batch = first.receive_batch(1).await.unwrap();
        let first_message = &first_batch.messages[0];
        let json: serde_json::Value = serde_json::from_slice(&first_message.payload).unwrap();
        assert_eq!(json["data"], "aGVsbG8=");
        assert_eq!(json["sequence"], "7");
        assert_eq!(json["auth"], "Bearer test-token");
        assert_eq!(first_message.metadata["grpc.ack_guarantee"], "none");
        assert_eq!(first_message.metadata["grpc.response_index"], "0");

        let mut second = DynamicConsumer::new(&config, &config.url).await.unwrap();
        let second_batch = second.receive_batch(1).await.unwrap();
        assert_eq!(
            first_message.message_id, second_batch.messages[0].message_id,
            "the same RPC response must have a deterministic id"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn dynamic_server_streaming_is_descriptor_derived() {
        let (address, handle) = dynamic_fixture_server().await;
        let config = dynamic_config(address, "Stream");
        let mut consumer = DynamicConsumer::new(&config, &config.url).await.unwrap();
        let batch = consumer.receive_batch(8).await.unwrap();
        assert_eq!(batch.messages.len(), 2);
        let first: serde_json::Value = serde_json::from_slice(&batch.messages[0].payload).unwrap();
        let second: serde_json::Value = serde_json::from_slice(&batch.messages[1].payload).unwrap();
        assert_eq!(first["sequence"], "7");
        assert_eq!(second["sequence"], "8");
        handle.abort();
    }

    #[tokio::test]
    async fn dynamic_unary_output_calls_once_per_message_and_returns_replies() {
        let (address, handle) = dynamic_tls_fixture_server().await;
        let config = dynamic_tls_config(address, "Unary")
            .with_bearer_token("sink-token")
            .with_request(serde_json::Value::Null);
        let publisher = DynamicPublisher::new(&config, &config.url).await.unwrap();

        let sent = publisher
            .send_batch(vec![
                CanonicalMessage::new(br#"{"data":"aGk=","sequence":"1"}"#.to_vec(), Some(11)),
                CanonicalMessage::new(br#"{"data":"aGk=","sequence":"2"}"#.to_vec(), Some(22)),
            ])
            .await
            .unwrap();

        let SentBatch::Partial { responses, failed } = sent else {
            panic!("a unary sink replies per message");
        };
        assert!(failed.is_empty(), "{failed:?}");
        let responses = responses.unwrap();
        assert_eq!(responses.len(), 2);
        // Replies carry the originating id so the route can correlate them.
        let mut ids: Vec<_> = responses.iter().map(|reply| reply.message_id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![11, 22]);
        for reply in &responses {
            let json: serde_json::Value = serde_json::from_slice(&reply.payload).unwrap();
            assert_eq!(json["auth"], "Bearer sink-token");
        }
        handle.abort();
    }

    #[tokio::test]
    async fn dynamic_client_streaming_output_sends_one_batch_per_rpc() {
        let (address, handle) = dynamic_fixture_server().await;
        let config = dynamic_config(address, "ClientStream").with_request(serde_json::Value::Null);
        let publisher = DynamicPublisher::new(&config, &config.url).await.unwrap();

        let sent = publisher
            .send_batch(vec![
                CanonicalMessage::new(br#"{"data":"","sequence":"3"}"#.to_vec(), Some(7)),
                CanonicalMessage::new(br#"{"data":"","sequence":"4"}"#.to_vec(), Some(8)),
                CanonicalMessage::new(br#"{"data":"","sequence":"5"}"#.to_vec(), Some(9)),
            ])
            .await
            .unwrap();

        let SentBatch::Partial { responses, failed } = sent else {
            panic!("a client-streaming sink replies once per batch");
        };
        assert!(failed.is_empty(), "{failed:?}");
        let responses = responses.unwrap();
        assert_eq!(responses.len(), 1, "one reply covers the whole batch");
        let json: serde_json::Value = serde_json::from_slice(&responses[0].payload).unwrap();
        // The fixture sums sequences and reports how many requests one RPC carried.
        assert_eq!(json["sequence"], "12");
        assert_eq!(json["data"], "Aw==", "one RPC carried all three requests");
        handle.abort();
    }

    #[tokio::test]
    async fn dynamic_output_failures_are_classified_and_scoped() {
        let (address, handle) = dynamic_fixture_server().await;

        // A payload that does not match the descriptor fails permanently, on its own,
        // without stopping the messages around it.
        let unary = dynamic_config(address, "Unary").with_request(serde_json::Value::Null);
        let publisher = DynamicPublisher::new(&unary, &unary.url).await.unwrap();
        let sent = publisher
            .send_batch(vec![
                CanonicalMessage::new(br#"{"nope":1}"#.to_vec(), Some(1)),
                CanonicalMessage::new(br#"{"data":"","sequence":"5"}"#.to_vec(), Some(2)),
            ])
            .await
            .unwrap();
        let SentBatch::Partial { responses, failed } = sent else {
            panic!("expected per-message outcomes");
        };
        assert_eq!(responses.unwrap().len(), 1, "the good message still went");
        assert_eq!(failed.len(), 1);
        assert!(matches!(failed[0].1, PublisherError::NonRetryable(_)));

        // INVALID_ARGUMENT is permanent, and on a client-streaming RPC the one reply
        // covers the batch, so every streamed message fails with it.
        let streaming =
            dynamic_config(address, "ClientStream").with_request(serde_json::Value::Null);
        let publisher = DynamicPublisher::new(&streaming, &streaming.url)
            .await
            .unwrap();
        let sent = publisher
            .send_batch(vec![
                CanonicalMessage::new(br#"{"data":"","sequence":"1"}"#.to_vec(), Some(3)),
                CanonicalMessage::new(br#"{"data":"","sequence":"-1"}"#.to_vec(), Some(4)),
            ])
            .await
            .unwrap();
        let SentBatch::Partial { failed, .. } = sent else {
            panic!("expected a failed batch");
        };
        assert_eq!(failed.len(), 2, "batch granularity fails the whole stream");
        assert!(failed
            .iter()
            .all(|(_, error)| matches!(error, PublisherError::NonRetryable(_))));
        handle.abort();
    }

    #[tokio::test]
    async fn dynamic_output_rejects_source_shaped_methods_and_request() {
        let (address, handle) = dynamic_fixture_server().await;

        for method in ["Stream", "Bidi"] {
            let config = dynamic_config(address, method).with_request(serde_json::Value::Null);
            let error = DynamicPublisher::new(&config, &config.url)
                .await
                .err()
                .unwrap();
            assert!(
                error.to_string().contains("use it as the route's input"),
                "{error:#}"
            );
        }

        // `request` belongs to a source; on a sink the messages are the requests.
        let config = dynamic_config(address, "Unary");
        let error = DynamicPublisher::new(&config, &config.url)
            .await
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("does not use `request`"),
            "{error:#}"
        );

        // The mirror image: a client-streaming method used as an input.
        let config = dynamic_config(address, "ClientStream");
        let error = DynamicConsumer::new(&config, &config.url)
            .await
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("use it as the route's output"),
            "{error:#}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn dynamic_reflection_and_capability_errors_are_explicit() {
        let (address, handle) = dynamic_fixture_server().await;
        let mut reflected = dynamic_config(address, "Unary");
        reflected.descriptor_set_bytes = None;
        reflected.reflection = true;
        DynamicConsumer::new(&reflected, &reflected.url)
            .await
            .expect("reflection should discover the fixture");

        for (method, shape) in [
            ("ClientStream", "client-streaming"),
            ("Bidi", "bidirectional-streaming"),
        ] {
            let config = dynamic_config(address, method);
            let error = DynamicConsumer::new(&config, &config.url)
                .await
                .err()
                .unwrap();
            assert!(error.to_string().contains(shape), "{error:#}");
            assert!(error
                .to_string()
                .contains("supports unary and server-streaming"));
        }
        handle.abort();
    }

    #[tokio::test]
    async fn dynamic_status_preserves_code_message_and_trailing_metadata() {
        let (address, handle) = dynamic_fixture_server().await;
        let mut config = dynamic_config(address, "Unary");
        config.request = Some(serde_json::json!({"data": "", "sequence": "-1"}));
        let error = DynamicConsumer::new(&config, &config.url)
            .await
            .err()
            .unwrap();
        let status = error
            .downcast_ref::<GrpcStatusError>()
            .expect("structured gRPC status");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(status.message(), "invalid fixture request");
        assert_eq!(
            status
                .trailing_metadata()
                .get("error-detail")
                .unwrap()
                .to_str()
                .unwrap(),
            "secret-trailer"
        );
        assert!(!format!("{status:?}").contains("secret-trailer"));
        handle.abort();
    }

    #[tokio::test]
    async fn dynamic_request_and_idle_stream_deadlines_are_separate() {
        let (address, handle) = dynamic_fixture_server().await;

        let mut request_timeout = dynamic_config(address, "Unary");
        request_timeout.request = Some(serde_json::json!({"data": "", "sequence": "98"}));
        request_timeout.request_timeout_ms = Some(10);
        let error = DynamicConsumer::new(&request_timeout, &request_timeout.url)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("call timed out"), "{error:#}");

        let mut idle_timeout = dynamic_config(address, "Stream");
        idle_timeout.request = Some(serde_json::json!({"data": "", "sequence": "99"}));
        idle_timeout.request_timeout_ms = Some(1_000);
        idle_timeout.idle_stream_timeout_ms = Some(10);
        let mut consumer = DynamicConsumer::new(&idle_timeout, &idle_timeout.url)
            .await
            .unwrap();
        let error = consumer.receive_batch(1).await.err().unwrap();
        assert!(error.to_string().contains("idle timeout"), "{error}");

        let mut legacy_timeout = dynamic_config(address, "Stream");
        legacy_timeout.request = Some(serde_json::json!({"data": "", "sequence": "99"}));
        legacy_timeout.timeout_ms = Some(10);
        legacy_timeout.request_timeout_ms = Some(1_000);
        let mut consumer = DynamicConsumer::new(&legacy_timeout, &legacy_timeout.url)
            .await
            .unwrap();
        let batch = consumer.receive_batch(1).await.unwrap();
        assert_eq!(batch.messages.len(), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn dynamic_overall_deadline_stops_the_route_instead_of_reconnecting() {
        let (address, handle) = dynamic_fixture_server().await;
        let mut config = dynamic_config(address, "Stream");
        config.request = Some(serde_json::json!({"data": "", "sequence": "99"}));
        config.overall_timeout_ms = Some(10);

        let mut consumer = DynamicConsumer::new(&config, &config.url).await.unwrap();
        let error = consumer.receive_batch(1).await.err().unwrap();
        // Connection would make the route reconnect, restarting the RPC and resetting the
        // very cap that just fired.
        assert!(
            matches!(error, ConsumerError::Permanent(_)),
            "overall deadline must be terminal, got {error:?}"
        );
        assert!(error.to_string().contains("overall deadline exceeded"));
        handle.abort();
    }

    #[tokio::test]
    async fn bridge_modes_reject_dynamic_only_credentials() {
        let base = GrpcConfig::new("http://127.0.0.1:1".to_string()).with_topic("orders");

        for (label, config) in [
            ("bearer_token", base.clone().with_bearer_token("token")),
            ("api_key", base.clone().with_api_key("key")),
            (
                "metadata",
                base.clone()
                    .with_metadata(HashMap::from([("x-tenant".into(), "acme".into())])),
            ),
            (
                "binary_metadata",
                base.clone()
                    .with_binary_metadata(HashMap::from([("x-trace-bin".into(), vec![1_u8])])),
            ),
        ] {
            let publisher = GrpcPublisher::new(&config).await.err().unwrap();
            assert!(publisher.to_string().contains(label), "{publisher:#}");

            let consumer = ClientModeConsumer::new(&config, &config.url).await.err();
            assert!(
                consumer.is_some_and(|error| error.to_string().contains(label)),
                "Bridge client must reject {label} rather than connect unauthenticated"
            );

            let mut server = config.clone();
            server.server_mode = true;
            server.url = "http://127.0.0.1:0".to_string();
            let error = ServerModeConsumer::new(&server, &server.url).await.err();
            assert!(error.is_some_and(|error| error.to_string().contains(label)));
        }
    }

    #[tokio::test]
    async fn dynamic_construction_rejects_invalid_descriptors_names_and_json() {
        let (address, handle) = dynamic_fixture_server().await;

        let mut invalid_descriptor = dynamic_config(address, "Unary");
        invalid_descriptor.descriptor_set_bytes = Some(vec![0xff]);
        assert!(
            DynamicConsumer::new(&invalid_descriptor, &invalid_descriptor.url)
                .await
                .is_err()
        );

        let mut invalid_service = dynamic_config(address, "Unary");
        invalid_service.service_name = Some("missing.Service".into());
        let error = DynamicConsumer::new(&invalid_service, &invalid_service.url)
            .await
            .err()
            .unwrap();
        assert!(error
            .to_string()
            .contains("service 'missing.Service' not found"));

        let invalid_method = dynamic_config(address, "Missing");
        let error = DynamicConsumer::new(&invalid_method, &invalid_method.url)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("method"));
        assert!(error.to_string().contains("Missing"));

        let mut invalid_json = dynamic_config(address, "Unary");
        invalid_json.request = Some(serde_json::json!({"sequence": {"not": "an integer"}}));
        assert!(DynamicConsumer::new(&invalid_json, &invalid_json.url)
            .await
            .is_err());

        handle.abort();
    }

    /// `extract_secrets` hex-encodes map keys so they survive as environment variable
    /// names, and the config crate rebuilds the map from those names. The call site has
    /// to undo that, or the header goes out under the encoded name.
    #[test]
    fn binary_metadata_keys_survive_the_secret_env_round_trip() {
        let mut config = GrpcConfig::new("https://localhost:50051");
        config
            .binary_metadata
            .insert("x-trace-bin".to_string(), vec![1, 2, 3]);

        let mut secrets = HashMap::new();
        config.extract_secrets("MQB__ROUTE__OUTPUT__GRPC", &mut secrets);
        assert!(config.binary_metadata.is_empty());
        assert_eq!(secrets.len(), 1);

        // The reload lowercases the env name and keeps only its trailing segment as key.
        let (env_key, env_value) = secrets.iter().next().unwrap();
        config.binary_metadata.insert(
            env_key.rsplit("__").next().unwrap().to_ascii_lowercase(),
            serde_json::from_str(env_value).unwrap(),
        );

        let mut metadata = MetadataMap::new();
        apply_call_metadata(&config, &mut metadata).unwrap();
        assert_eq!(
            metadata.get_bin("x-trace-bin").unwrap().to_bytes().unwrap(),
            bytes::Bytes::from_static(&[1, 2, 3])
        );
    }

    #[test]
    fn binary_metadata_rejects_a_key_that_is_neither_usable_nor_encoded() {
        let mut config = GrpcConfig::new("https://localhost:50051");
        config
            .binary_metadata
            .insert("x-trace".to_string(), vec![1]);
        let error = apply_call_metadata(&config, &mut MetadataMap::new())
            .err()
            .unwrap();
        assert!(error.to_string().contains("x-trace"), "{error:#}");
    }

    #[test]
    fn credentials_require_a_tls_endpoint() {
        let mut authorization_metadata = GrpcConfig::new("http://localhost:50051");
        authorization_metadata
            .metadata
            .insert("Authorization".to_string(), "Bearer token".to_string());
        let mut api_key_metadata = GrpcConfig::new("http://localhost:50051");
        api_key_metadata
            .metadata
            .insert("x-api-key".to_string(), "key".to_string());
        let mut authorization_bin = GrpcConfig::new("http://localhost:50051");
        authorization_bin
            .binary_metadata
            .insert("authorization-bin".to_string(), vec![1, 2, 3]);
        // An `api_key_name` that is itself a `-bin` key, carried as binary metadata.
        let mut binary_api_key_name = GrpcConfig::new("http://localhost:50051");
        binary_api_key_name.api_key_name = Some("x-tenant-bin".to_string());
        binary_api_key_name
            .binary_metadata
            .insert("x-tenant-bin".to_string(), vec![1, 2, 3]);
        for config in [
            GrpcConfig::new("http://localhost:50051").with_bearer_token("token"),
            GrpcConfig::new("http://localhost:50051").with_api_key("key"),
            authorization_metadata,
            api_key_metadata,
            authorization_bin,
            binary_api_key_name,
        ] {
            let error = apply_call_metadata(&config, &mut MetadataMap::new()).unwrap_err();
            assert!(error.to_string().contains("https://"), "{error:#}");
        }
    }
}

mod server {
    use super::super::dynamic::reflected_descriptor_pool;
    use super::super::publisher::GrpcPublisher;
    use super::super::server::*;
    use super::super::{proto, BridgeMessage};
    use crate::endpoints::grpc::GrpcConsumer;
    use crate::models::GrpcConfig;
    use crate::traits::{MessageConsumer, MessageDisposition, MessagePublisher};
    use crate::CanonicalMessage;
    use futures::StreamExt;
    use proto::bridge_server::Bridge;
    use proto::SubscribeRequest;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::{broadcast, mpsc, oneshot};
    use tonic::Request;

    /// `publish_batch` must dispatch without waiting for each message to commit. Awaiting
    /// receipts inline prevents later messages from reaching the consumer until the first
    /// has committed, so every batch holds exactly one message and the stream is serialized.
    ///
    /// Uses the real `BridgeService`, not `MockBridge` — the mock answers immediately and
    /// cannot catch this.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn publish_batch_does_not_serialize_on_commits() {
        const COUNT: usize = 200;

        let mut consumer = GrpcConsumer::new(&GrpcConfig {
            url: "http://127.0.0.1:0".into(),
            topic: Some("batching".into()),
            server_mode: true,
            ..Default::default()
        })
        .await
        .unwrap();
        let url = format!("http://{}", consumer.bound_addr.unwrap());

        let drain = tokio::spawn(async move {
            let first = consumer.receive_batch(512).await.expect("first receive");
            let first_count = first.messages.len();
            // A slow runner may deliver everything in the first batch; that proves it too.
            let mut total = first_count;
            if first_count < COUNT {
                let second =
                    tokio::time::timeout(Duration::from_secs(1), consumer.receive_batch(512))
                        .await
                        .expect("dispatch waited for the first batch to commit")
                        .expect("second receive");
                let second_count = second.messages.len();
                total += second_count;
                (first.commit)(vec![MessageDisposition::Ack; first_count])
                    .await
                    .expect("first commit");
                (second.commit)(vec![MessageDisposition::Ack; second_count])
                    .await
                    .expect("second commit");
            } else {
                (first.commit)(vec![MessageDisposition::Ack; first_count])
                    .await
                    .expect("first commit");
            }

            while total < COUNT {
                let batch = consumer.receive_batch(512).await.expect("receive");
                let n = batch.messages.len();
                if n == 0 {
                    continue;
                }
                total += n;
                (batch.commit)(vec![MessageDisposition::Ack; n])
                    .await
                    .expect("commit");
            }
            total
        });

        let publisher = GrpcPublisher::new(&GrpcConfig {
            url,
            topic: Some("batching".into()),
            ..Default::default()
        })
        .await
        .unwrap();

        let messages = (0..COUNT)
            .map(|i| CanonicalMessage::from(format!("m{i}")))
            .collect();
        publisher.send_batch(messages).await.unwrap();

        let total = tokio::time::timeout(Duration::from_secs(30), drain)
            .await
            .expect("route did not drain")
            .unwrap();

        assert_eq!(total, COUNT, "every message must arrive");
    }

    #[tokio::test(start_paused = true)]
    async fn server_mode_partial_batch_does_not_linger_before_commit() {
        let (tx, rx) = mpsc::channel(1);
        let shared_server = Arc::new(SharedGrpcServer {
            router: Arc::new(SharedGrpcRouter::new()),
            handle: tokio::spawn(std::future::pending()),
            bound_addr: "127.0.0.1:0".parse().unwrap(),
        });
        let mut consumer = ServerModeConsumer {
            route_id: GRPC_ROUTE_ID.fetch_add(1, Ordering::Relaxed),
            shared_server,
            bound_addr: "127.0.0.1:0".parse().unwrap(),
            rxs: vec![rx],
            drain_start: 0,
            exit_on_empty: false,
        };
        let (completion, receipt) = oneshot::channel();
        tx.send(InboundDelivery {
            message: bridge_msg("prompt-commit"),
            completion,
        })
        .await
        .unwrap();

        let batch = tokio::time::timeout(Duration::from_millis(1), consumer.receive_batch(128))
            .await
            .expect("server-mode receive must not linger for a partial batch")
            .expect("receive");
        assert_eq!(batch.messages.len(), 1);
        (batch.commit)(vec![MessageDisposition::Ack])
            .await
            .expect("commit");
        assert!(matches!(receipt.await, Ok(MessageDisposition::Ack)));
    }

    fn bridge_msg(id: &str) -> BridgeMessage {
        BridgeMessage {
            payload: id.as_bytes().to_vec(),
            id: id.to_string(),
            metadata: Default::default(),
        }
    }

    #[test]
    fn reply_preserves_the_request_id() {
        let response = publish_response_for_disposition(
            "request-id".to_string(),
            MessageDisposition::Reply(CanonicalMessage::from("reply")),
        );
        let Some(proto::publish_response::Result::Reply(reply)) = response.result else {
            panic!("expected a reply response");
        };
        assert_eq!(reply.id, "request-id");
    }

    #[test]
    fn pending_messages_replays_only_unacknowledged() {
        let mut pending = PendingMessages::default();
        for id in ["a", "b", "c"] {
            pending.retain(&bridge_msg(id));
        }
        // Retaining the same id twice must not duplicate it.
        pending.retain(&bridge_msg("b"));

        assert!(pending.acknowledge("b"));
        assert!(!pending.acknowledge("b"), "a second ack finds nothing");
        assert!(!pending.is_unacked("b"));

        let replayed: Vec<String> = pending.replay().into_iter().map(|msg| msg.id).collect();
        assert_eq!(replayed, vec!["a".to_string(), "c".to_string()]);
    }

    #[test]
    fn pending_messages_caps_retention_and_drops_the_oldest() {
        let mut pending = PendingMessages::default();
        for i in 0..MAX_PENDING_PER_CONSUMER + 10 {
            pending.retain(&bridge_msg(&format!("m{i}")));
        }
        assert_eq!(pending.replay().len(), MAX_PENDING_PER_CONSUMER);
        assert!(!pending.is_unacked("m0"), "the oldest is evicted");
        assert!(pending.is_unacked(&format!("m{}", MAX_PENDING_PER_CONSUMER + 9)));
    }

    fn service_with_topic(topic: &str) -> BridgeService {
        let router = Arc::new(SharedGrpcRouter::new());
        let (tx, _rx) = mpsc::channel::<InboundDelivery>(8);
        router
            .register_route(1, topic.to_string(), vec![tx])
            .unwrap();
        BridgeService {
            router,
            commit_timeout: None,
        }
    }

    /// Two live subscriptions under one id would be fanned the same broadcast messages
    /// while sharing a single retention set, so the first ack would remove the entry and
    /// the second consumer's ack would come back rejected.
    #[tokio::test]
    async fn subscribe_rejects_a_duplicate_active_consumer_id() {
        let service = service_with_topic("dup");
        let subscribe = |consumer_id: &str| {
            service.subscribe(Request::new(SubscribeRequest {
                topic: "dup".to_string(),
                consumer_id: consumer_id.to_string(),
            }))
        };

        let _first = subscribe("shared").await.expect("first subscription");
        let err = subscribe("shared")
            .await
            .expect_err("duplicate is rejected");
        assert_eq!(err.code(), tonic::Code::AlreadyExists);
        assert!(
            err.message().contains("shared"),
            "the error should name the id: {}",
            err.message()
        );

        subscribe("other").await.expect("a distinct id still works");
    }

    #[tokio::test]
    async fn subscriber_lag_closes_the_stream_and_reconnect_replays_retained_messages() {
        let router = Arc::new(SharedGrpcRouter::new());
        let (tx, _rx) = mpsc::channel::<InboundDelivery>(8);
        let (broadcast_tx, _) = broadcast::channel(2);
        router.routes.write().unwrap().insert(
            1,
            SharedGrpcRoute {
                topic: "default".to_string(),
                txs: vec![tx],
                cursor: Arc::new(AtomicUsize::new(0)),
                broadcast_tx,
                subscriber_pending: Arc::new(Mutex::new(SubscriberPending::default())),
                active_subscribers: Arc::new(Mutex::new(HashSet::new())),
            },
        );
        let service = BridgeService {
            router: router.clone(),
            commit_timeout: None,
        };
        let request = || {
            Request::new(SubscribeRequest {
                topic: "default".to_string(),
                consumer_id: "durable".to_string(),
            })
        };

        let mut first = service.subscribe(request()).await.unwrap().into_inner();
        for id in ["a", "b", "c"] {
            router.dispatch(bridge_msg(id)).await.unwrap();
        }
        assert!(tokio::time::timeout(Duration::from_secs(1), first.next())
            .await
            .expect("lagged stream should terminate")
            .is_none());

        let mut replay = service.subscribe(request()).await.unwrap().into_inner();
        for id in ["a", "b", "c"] {
            assert_eq!(replay.next().await.unwrap().unwrap().id, id);
        }
    }

    /// Without the id there is no retention set to resolve the ack against, so reporting
    /// success would claim a commit that never tracked anything.
    #[tokio::test]
    async fn acknowledge_without_a_consumer_id_reports_failure() {
        let service = service_with_topic("acks");
        let response = service
            .acknowledge(Request::new(proto::Ack {
                id: "m1".to_string(),
                status: proto::ack::Status::Ack as i32,
                reason: String::new(),
                metadata: Default::default(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!response.success);
        assert!(response.error.contains("mq_bridge.consumer_id"));
    }

    #[test]
    fn subscriber_pending_caps_the_number_of_subscribers() {
        let mut subscribers = SubscriberPending::default();
        for i in 0..MAX_PENDING_CONSUMERS + 5 {
            subscribers.entry(&format!("c{i}")).retain(&bridge_msg("x"));
        }
        assert!(subscribers.get("c0").is_none(), "the oldest is evicted");
        assert!(subscribers
            .get(&format!("c{}", MAX_PENDING_CONSUMERS + 4))
            .is_some());
    }

    /// A concrete free port, so each embedded server gets its own registry entry:
    /// `GrpcServerKey` keys on the literal address, so two `127.0.0.1:0` consumers share
    /// one server and the first of them to drop tears it down under the other.
    async fn free_server_url() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        format!("http://127.0.0.1:{port}")
    }

    async fn server_consumer_on_free_port(mut config: GrpcConfig) -> GrpcConsumer {
        let mut last_error = None;
        for _ in 0..10 {
            config.url = free_server_url().await;
            match GrpcConsumer::new(&config).await {
                Ok(consumer) => return consumer,
                Err(error)
                    if error.chain().any(|cause| {
                        cause
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::AddrInUse)
                    }) =>
                {
                    last_error = Some(error);
                }
                Err(error) => panic!("failed to start gRPC test server: {error:#}"),
            }
        }
        panic!(
            "failed to bind a reserved gRPC test port after 10 attempts: {:#}",
            last_error.unwrap()
        );
    }

    /// `PrefixRouter` replaces tonic's axum router, so every branch it dispatches has to
    /// be exercised: the Bridge service, reflection v1, and reflection v1alpha.
    #[tokio::test]
    async fn prefix_router_dispatches_bridge_and_both_reflection_versions() {
        use tonic_reflection::pb::v1alpha::server_reflection_client::ServerReflectionClient;
        use tonic_reflection::pb::v1alpha::server_reflection_request::MessageRequest;
        use tonic_reflection::pb::v1alpha::server_reflection_response::MessageResponse;
        use tonic_reflection::pb::v1alpha::ServerReflectionRequest;

        let mut consumer = server_consumer_on_free_port(GrpcConfig {
            topic: Some("router".into()),
            server_mode: true,
            ..Default::default()
        })
        .await;
        let address = consumer.bound_addr.unwrap();
        let url = format!("http://{address}");

        // v1: the descriptor-discovery path a dynamic source uses.
        let channel = tonic::transport::Endpoint::from_shared(url.clone())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let pool = reflected_descriptor_pool(
            &GrpcConfig::new(url.clone()),
            channel.clone(),
            "mqbridge.Bridge",
            Some(Duration::from_secs(5)),
        )
        .await
        .expect("v1 reflection must route to the reflection service");
        assert!(pool.get_service_by_name("mqbridge.Bridge").is_some());

        // v1alpha: same descriptors over the older path older tooling still uses.
        let mut v1alpha = ServerReflectionClient::new(channel);
        let response = v1alpha
            .server_reflection_info(tokio_stream::iter([ServerReflectionRequest {
                host: String::new(),
                message_request: Some(MessageRequest::FileContainingSymbol(
                    "mqbridge.Bridge".to_owned(),
                )),
            }]))
            .await
            .expect("v1alpha reflection must route to the reflection service")
            .into_inner()
            .message()
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            response.message_response,
            Some(MessageResponse::FileDescriptorResponse(_))
        ));

        // Fallback branch: a Bridge RPC must still reach the Bridge service.
        let publisher = GrpcPublisher::new(&GrpcConfig {
            url: url.clone(),
            topic: Some("router".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        let sent = tokio::spawn(async move {
            publisher
                .send_batch(vec![CanonicalMessage::new("routed".into(), None)])
                .await
        });
        let batch = tokio::time::timeout(Duration::from_secs(5), consumer.receive_batch(1))
            .await
            .expect("Bridge publish did not reach the fallback branch")
            .unwrap();
        assert_eq!(batch.messages[0].payload.as_ref(), b"routed");
        (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();
        sent.await.unwrap().expect("Bridge publish failed");
    }

    #[tokio::test]
    async fn generated_python_client_interoperates_with_bridge_server_mode() {
        let python = std::process::Command::new("python3")
            .args(["-c", "import grpc, grpc_tools.protoc"])
            .status();
        if !python.is_ok_and(|status| status.success()) {
            eprintln!("skipping Python gRPC compatibility test: grpcio-tools is unavailable");
            return;
        }

        let generated = tempfile::tempdir().unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let generation = std::process::Command::new("python3")
            .current_dir(root)
            .args([
                "-m",
                "grpc_tools.protoc",
                "-I",
                "src/endpoints/grpc/proto",
                "--python_out",
                generated.path().to_str().unwrap(),
                "--grpc_python_out",
                generated.path().to_str().unwrap(),
                "src/endpoints/grpc/proto/mqbridge/bridge.proto",
            ])
            .status()
            .unwrap();
        assert!(generation.success(), "Python client generation failed");

        let mut consumer = server_consumer_on_free_port(GrpcConfig {
            topic: Some("compat".into()),
            server_mode: true,
            request_timeout_ms: Some(5_000),
            ..Default::default()
        })
        .await;
        let address = consumer.bound_addr.unwrap();

        struct ChildGuard(std::process::Child);

        impl Drop for ChildGuard {
            fn drop(&mut self) {
                if !matches!(self.0.try_wait(), Ok(Some(_))) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
        }

        let mut child = ChildGuard(
            std::process::Command::new("python3")
                .current_dir(root)
                .env("PYTHONPATH", generated.path())
                .arg("tests/compat/python/bridge_client.py")
                .arg(address.to_string())
                .spawn()
                .unwrap(),
        );

        let batch = tokio::time::timeout(Duration::from_secs(5), consumer.receive_batch(1))
            .await
            .expect("Python client did not publish")
            .expect("Bridge server did not receive Python publish");
        assert_eq!(
            batch.messages[0].payload.as_ref(),
            b"python-generated-client"
        );
        (batch.commit)(vec![MessageDisposition::Ack])
            .await
            .expect("commit Python publish");

        let status = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Python compatibility client timed out");
        assert!(status.success(), "Python compatibility client failed");
    }
}
