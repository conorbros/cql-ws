use cassandra_protocol::compression::Compression;
use cassandra_protocol::frame::message_query::BodyReqQuery;
use cassandra_protocol::frame::message_response::ResponseBody;
use cassandra_protocol::frame::message_result::{BodyResResultRows, ResResultBody};
use cassandra_protocol::frame::Envelope;
use cassandra_protocol::frame::Flags;
use cassandra_protocol::frame::Opcode;
use cassandra_protocol::frame::Version;
use cassandra_protocol::query::query_params::QueryParams;
use cassandra_protocol::types::cassandra_type::{wrapper_fn, CassandraType};
use futures_util::{SinkExt, StreamExt};
use rustls::client::{ServerCertVerified, ServerCertVerifier, WebPkiVerifier};
use rustls::{Certificate, CertificateError, RootCertStore, ServerName};
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::error::Error;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::handshake::server::Request;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::Connector;
use tokio_tungstenite::WebSocketStream;

pub struct Session {
    in_rx: UnboundedReceiver<Message>,
    out_tx: UnboundedSender<Message>,
}

impl Session {
    fn construct_request(uri: &str, use_subprotocol_header: bool) -> Request {
        let uri = uri.parse::<http::Uri>().unwrap();

        let authority = uri.authority().unwrap().as_str();
        let host = authority
            .find('@')
            .map(|idx| authority.split_at(idx + 1).1)
            .unwrap_or_else(|| authority);

        if host.is_empty() {
            panic!("Empty host name");
        }

        let mut builder = http::Request::builder()
            .method("GET")
            .header("Host", host)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", generate_key());

        if use_subprotocol_header {
            builder = builder.header(
                "Sec-WebSocket-Protocol",
                "cql".parse::<http::HeaderValue>().unwrap(),
            );
        }
        builder.uri(uri).body(()).unwrap()
    }

    pub async fn new(address: &str, use_subprotocol_header: bool) -> Self {
        let (ws_stream, _) = tokio_tungstenite::connect_async(Self::construct_request(
            address,
            use_subprotocol_header,
        ))
        .await
        .unwrap();

        let (in_tx, in_rx) = unbounded_channel::<Message>();
        let (out_tx, out_rx) = unbounded_channel::<Message>();

        spawn_read_write_tasks(ws_stream, in_tx, out_rx);

        let mut session = Self { in_rx, out_tx };
        session.startup().await;
        session
    }

    pub async fn new_tls(address: &str, ca_path: &str, use_subprotocol_header: bool) -> Self {
        let root_cert_store = load_ca(ca_path);

        let tls_client_config = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(Arc::new(SkipVerifyHostName::new(root_cert_store)))
            .with_no_client_auth();

        let (ws_stream, _) = tokio_tungstenite::connect_async_tls_with_config(
            Self::construct_request(address, use_subprotocol_header),
            None,
            false,
            Some(Connector::Rustls(Arc::new(tls_client_config))),
        )
        .await
        .unwrap();

        let (in_tx, in_rx) = unbounded_channel::<Message>();
        let (out_tx, out_rx) = unbounded_channel::<Message>();

        spawn_read_write_tasks(ws_stream, in_tx, out_rx);

        let mut session = Self { in_rx, out_tx };
        session.startup().await;
        session
    }

    async fn startup(&mut self) {
        let envelope = Envelope::new_req_startup(None, Version::V4);
        self.out_tx.send(Self::encode(envelope)).unwrap();

        let envelope = Self::decode(self.in_rx.recv().await.unwrap());

        match envelope.opcode {
            Opcode::Ready => println!("cql-ws: received: {:?}", envelope),
            Opcode::Authenticate => {
                todo!();
            }
            _ => panic!("expected to receive a ready or authenticate message"),
        }
    }

    pub async fn query(&mut self, query: &str) -> Vec<Vec<CassandraType>> {
        let envelope = Envelope::new_query(
            BodyReqQuery {
                query: query.into(),
                query_params: QueryParams::default(),
            },
            Flags::empty(),
            Version::V4,
        );

        self.out_tx.send(Self::encode(envelope)).unwrap();

        let envelope = Self::decode(self.in_rx.recv().await.unwrap());

        if let ResponseBody::Result(ResResultBody::Rows(BodyResResultRows {
            rows_content,
            metadata,
            ..
        })) = envelope.response_body().unwrap()
        {
            let mut result_values = vec![];
            for row in &rows_content {
                let mut row_result_values = vec![];
                for (i, col_spec) in metadata.col_specs.iter().enumerate() {
                    let wrapper = wrapper_fn(&col_spec.col_type.id);
                    let value = wrapper(&row[i], &col_spec.col_type, envelope.version).unwrap();

                    row_result_values.push(value);
                }
                result_values.push(row_result_values);
            }

            result_values
        } else {
            panic!("unexpected to recieve a result envelope");
        }
    }

    fn encode(envelope: Envelope) -> Message {
        let data = envelope.encode_with(Compression::None).unwrap();
        Message::Binary(data)
    }

    fn decode(ws_message: Message) -> Envelope {
        match ws_message {
            Message::Binary(data) => {
                Envelope::from_buffer(data.as_slice(), Compression::None)
                    .unwrap()
                    .envelope
            }
            _ => panic!("expected to receive a binary message"),
        }
    }

    pub async fn send_raw_ws_message(&mut self, ws_message: Message) {
        self.out_tx.send(ws_message).unwrap();
    }

    pub async fn wait_for_raw_ws_message_resp(&mut self) -> Message {
        self.in_rx.recv().await.unwrap()
    }
}

fn spawn_read_write_tasks<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    ws_stream: WebSocketStream<S>,
    in_tx: UnboundedSender<Message>,
    mut out_rx: UnboundedReceiver<Message>,
) {
    let (mut write, mut read) = ws_stream.split();

    // read task
    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = read.next() => {
                    if let Some(message) = result {
                        match message {
                            Ok(ws_message @ Message::Binary(_)) => {
                                in_tx.send(ws_message).unwrap();
                            }
                            Ok(Message::Close(_)) => {
                                return;
                            }
                            Ok(_) => panic!("expected to recieve a binary message"),
                            Err(err) => panic!("{err}")
                        }
                    }
                }
                _ = in_tx.closed() => {
                    return;
                }
            }
        }
    });

    // write task
    tokio::spawn(async move {
        loop {
            if let Some(ws_message) = out_rx.recv().await {
                write.send(ws_message).await.unwrap();
            } else {
                match write.send(Message::Close(None)).await {
                    Ok(_) => {}
                    Err(Error::Protocol(ProtocolError::SendAfterClosing)) => {}
                    Err(err) => panic!("{err}"),
                }
                break;
            }
        }
    });
}

fn load_ca(path: &str) -> RootCertStore {
    let mut pem = BufReader::new(File::open(path).unwrap());
    let certs = rustls_pemfile::certs(&mut pem).unwrap();

    let mut root_cert_store = RootCertStore::empty();
    for cert in certs {
        root_cert_store.add(&Certificate(cert)).unwrap();
    }
    root_cert_store
}

pub struct SkipVerifyHostName {
    verifier: WebPkiVerifier,
}

impl SkipVerifyHostName {
    pub fn new(roots: RootCertStore) -> Self {
        SkipVerifyHostName {
            verifier: WebPkiVerifier::new(roots, None),
        }
    }
}

// This recreates the verify_hostname(false) functionality from openssl.
// This adds an opening for MitM attacks but we provide this functionality because there are some
// circumstances where providing a cert per instance in a cluster is difficult and this allows at least some security by sharing a single cert between all instances.
// Note that the SAN dnsname wildcards (e.g. *foo.com) wouldnt help here because we need to refer to destinations by ip address and there is no such wildcard functionality for ip addresses.
impl ServerCertVerifier for SkipVerifyHostName {
    fn verify_server_cert(
        &self,
        end_entity: &Certificate,
        intermediates: &[Certificate],
        server_name: &ServerName,
        scts: &mut dyn Iterator<Item = &[u8]>,
        ocsp_response: &[u8],
        now: std::time::SystemTime,
    ) -> std::result::Result<rustls::client::ServerCertVerified, rustls::Error> {
        match self.verifier.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            scts,
            ocsp_response,
            now,
        ) {
            Ok(result) => Ok(result),
            Err(rustls::Error::InvalidCertificate(CertificateError::NotValidForName)) => {
                Ok(ServerCertVerified::assertion())
            }
            Err(err) => Err(err),
        }
    }
}
