use crate::tls::{finish_config, tls_config, Protocol};
use anyhow::{bail, Context};
use bytes::{Buf, Bytes, BytesMut};
use h3::error::Code;
use h3::quic::SendStream;
use h3::server::{Builder, RequestResolver};
use h3_datagram::datagram_handler::{DatagramReader, DatagramSender, HandleDatagramsExt};
use h3_quinn::datagram::{RecvDatagramHandler, SendDatagramHandler};
use h3_quinn::Connection;
use http::{Method, Request, StatusCode};
use quinn::crypto::rustls::QuicServerConfig;
use quinn::Incoming;
use std::fmt::{Display, Formatter};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::task::JoinSet;

mod tls;

const BUF_CAPACITY: usize = u16::MAX as usize;

fn setup_tracing() {
    tracing_subscriber::FmtSubscriber::builder()
        .with_max_level(tracing::Level::DEBUG)
        .init()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    setup_tracing();
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .unwrap();

    let config = tls_config("certs").context("while creating base TLS configuration")?;

    tracing::info!("Starting up");

    let h3_config =
        finish_config(config, Protocol::Http3).context("while creating HTTP3 TLS configuration")?;
    h3_server(SocketAddr::from_str("0.0.0.0:443")?, h3_config).await?;

    Ok(())
}

async fn h3_server(addr: SocketAddr, tls_config: rustls::ServerConfig) -> anyhow::Result<()> {
    let tls_config =
        QuicServerConfig::try_from(tls_config).context("while creating QUIC server config")?;

    let server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_config));

    let endpoint = quinn::Endpoint::server(server_config, addr)
        .context("while creating Quinn server endpoint")?;

    let mut connection_handles = JoinSet::new();

    let mut builder = h3::server::builder();
    builder.enable_datagram(true).enable_extended_connect(true);
    let builder = Arc::new(builder);

    while let Some(connection) = endpoint.accept().await {
        connection_handles.spawn(h3_connection(builder.clone(), connection));
    }

    connection_handles.join_all().await;

    Ok(())
}

#[tracing::instrument(skip_all, fields(address), err(Debug))]
async fn h3_connection(builder: Arc<Builder>, connection: Incoming) -> anyhow::Result<()> {
    tracing::Span::current().record("address", connection.remote_address().to_string());

    // Accept QUIC connection
    let connection = connection
        .await
        .context("while accepting QUIC connection")?;

    tracing::debug!("Accepted QUIC connection");

    // Accept HTTP/3 connection
    let connection = Connection::new(connection);
    let mut connection = builder
        .build(connection)
        .await
        .context("while creating HTTP/3 connection")?;

    tracing::info!("Accepted HTTP/3 connection");

    // Set up global datagram senders/receivers
    // let dgram_sender = connection.get_datagram_sender(0);

    // Accept each HTTP/3 request
    let mut requests = JoinSet::new();
    while let Some(resolver) = connection
        .accept()
        .await
        .context("while accepting HTTP/3 request")?
    {
        let dgram_reader = connection.get_datagram_reader();
        let stream_id = resolver.frame_stream.send_id(); // TODO: this will be private soon...
        let dgram_sender = connection.get_datagram_sender(stream_id);
        requests.spawn(h3_request(resolver, dgram_reader, dgram_sender));
    }

    tracing::debug!("Remote going away, waiting for requests to finish...");
    requests.join_all().await;

    tracing::info!("Closing connection");
    Ok(())
}

#[derive(Debug)]
enum ProxyProtocol {
    Tcp,
    Udp,
}

impl Display for ProxyProtocol {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp => write!(f, "TCP"),
            Self::Udp => write!(f, "UDP"),
        }
    }
}

#[tracing::instrument(skip_all, fields(stream_id, address, protocol), err(Debug))]
async fn h3_request(
    resolver: RequestResolver<Connection, Bytes>,
    mut dgram_reader: DatagramReader<RecvDatagramHandler>,
    mut dgram_sender: DatagramSender<SendDatagramHandler, Bytes>,
) -> anyhow::Result<()> {
    let (request, mut source_stream) = resolver
        .resolve_request()
        .await
        .context("while resolving request")?;

    let stream_id = source_stream.id();
    tracing::Span::current().record("stream_id", stream_id.to_string());
    tracing::debug!("Accepted new request: {request:?}");

    let response = http::Response::builder().header("Server", "relayd");

    let (protocol, addr) = match h3_proxy(request) {
        Ok(data) => data,
        Err(err) => {
            let response = response
                .status(StatusCode::NOT_IMPLEMENTED)
                .body(())
                .context("while creating invalid parameter response")?;
            source_stream
                .send_response(response)
                .await
                .context("while sending invalid method response")?;

            source_stream
                .send_data("Method not allowed\r\n".into())
                .await
                .context("while sending invalid method response body")?;

            source_stream.stop_sending(Code::H3_NO_ERROR);
            source_stream
                .finish()
                .await
                .context("while finishing stream")?;
            bail!(err.context("while determining protocol and destination"));
        }
    };

    tracing::Span::current().record("address", addr.to_string());
    tracing::Span::current().record("protocol", protocol.to_string());
    tracing::debug!("Connecting...");

    match protocol {
        ProxyProtocol::Tcp => {
            let (mut source_write, mut source_read) = source_stream.split();

            let (mut dest_read, mut dest_write) = TcpStream::connect(addr)
                .await
                .context("while establishing TCP connection")?
                .into_split();

            let mut tasks = JoinSet::new();

            // Source -> Dest
            tasks.spawn(async move {
                while let Ok(Some(data)) = source_read.recv_data().await {
                    if let Err(err) = dest_write.write_all(data.chunk()).await {
                        tracing::error!("while copying data to dest: {err}");
                        break;
                    };
                }
                source_read.stop_sending(Code::H3_NO_ERROR);
            });

            // Dest -> Source
            tasks.spawn(async move {
                loop {
                    let mut buf = BytesMut::with_capacity(BUF_CAPACITY);
                    if let Ok(bytes) = dest_read.read_buf(&mut buf).await
                        && bytes > 0
                    {
                        if let Err(err) = source_write.send_data(buf.freeze()).await {
                            tracing::error!("while copying data to source: {err}");
                            break;
                        };
                    } else {
                        break;
                    }
                }
                if let Err(err) = source_write
                    .finish()
                    .await
                    .context("while finishing stream")
                {
                    tracing::error!("while finishing stream: {err}");
                };
            });

            tasks.join_all().await;
        }
        ProxyProtocol::Udp => {
            let dest = UdpSocket::bind("0.0.0.0:0")
                .await
                .context("while creating UDP socket")?;

            dest.connect(addr)
                .await
                .context("while establishing UDP connection")?;

            let dest_read = Arc::new(dest);
            let dest_write = dest_read.clone();

            let mut tasks = JoinSet::new();

            // Source -> Dest
            tasks.spawn(async move {
                while let Ok(datagram) = dgram_reader.read_datagram().await {
                    // TODO: is it really that efficient to be filtering out like 90% of datagrams in every task
                    if datagram.stream_id() != stream_id {
                        continue;
                    };
                    if let Err(err) = dest_write.send(datagram.payload().chunk()).await {
                        tracing::error!("while copying data to dest: {err}");
                        break;
                    };
                }
            });

            // Dest -> Source
            tasks.spawn(async move {
                loop {
                    let mut buf = BytesMut::with_capacity(BUF_CAPACITY);
                    if let Ok(bytes) = dest_read.recv_buf(&mut buf).await
                        && bytes > 0
                    {
                        if let Err(err) = dgram_sender.send_datagram(buf.freeze()) {
                            tracing::error!("while copying data to source: {err}");
                            break;
                        };
                    } else {
                        break;
                    }
                }
            });

            tasks.join_all().await;
            source_stream.stop_sending(Code::H3_NO_ERROR);
            source_stream
                .finish()
                .await
                .context("while finishing stream")?;
        }
    };

    Ok(())
}

fn h3_proxy(request: Request<()>) -> anyhow::Result<(ProxyProtocol, SocketAddr)> {
    if request.method() != Method::CONNECT {
        bail!("unknown request method: {}", request.method());
    }

    let protocol = match request.extensions().get::<h3::ext::Protocol>() {
        None => ProxyProtocol::Tcp,
        Some(&h3::ext::Protocol::CONNECT_UDP) => ProxyProtocol::Udp,
        Some(proto) => bail!("unknown protocol: {proto:?}"),
    };

    let addr = SocketAddr::from_str(request.uri().authority().unwrap().as_str())
        .context("while parsing request authority")?;

    Ok((protocol, addr))
}
