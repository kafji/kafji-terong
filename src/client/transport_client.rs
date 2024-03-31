use crate::{
    log_error,
    transport::{
        protocol::{ClientMessage, InputEvent, Ping, Pong, ServerMessage},
        Certificate, PrivateKey, SingleCertVerifier, Transport, Transporter,
    },
};
use anyhow::{Context, Error};
use macross::impl_from;
use rustls::{ClientConfig, ServerName};
use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    select,
    sync::mpsc,
    task::{self, JoinHandle},
    time::{interval_at, sleep, Instant, MissedTickBehavior},
};
use tokio_rustls::{TlsConnector, TlsStream};
use tracing::{debug, error, info, warn};

/// Time it takes before client giving up on connecting to the server.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

type ClientTransporter = Transporter<TcpStream, TlsStream<TcpStream>, ServerMessage, ClientMessage>;

#[derive(Debug)]
pub struct TransportClient {
    pub server_addr: SocketAddr,

    pub tls_certs: Vec<Certificate>,
    pub tls_key: PrivateKey,

    pub server_tls_certs: Vec<Certificate>,

    pub event_tx: mpsc::Sender<InputEvent>,
}

pub fn start(args: TransportClient) -> JoinHandle<()> {
    task::spawn(async move { run_transport_client(args).await })
}

async fn run_transport_client(args: TransportClient) {
    let TransportClient {
        server_addr,
        event_tx,
        tls_certs,
        tls_key,
        server_tls_certs,
    } = args;

    let tls_config = {
        let tls = create_client_tls_config(
            tls_certs,
            tls_key,
            server_tls_certs.into_iter().last().unwrap(),
        )
        .unwrap();
        Arc::new(tls)
    };

    loop {
        if let Err(err) = connect(&server_addr, tls_config.clone(), &event_tx).await {
            log_error!(err);

            match err {
                ConnectError::Timeout { .. } => {
                    break;
                }
                ConnectError::Other(_) => {
                    sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
}

#[derive(Debug)]
enum ConnectError {
    Timeout { msg: String },
    Other(Error),
}

impl_from!(ConnectError, {
    Self::Other => Error,
});

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::Timeout { msg } => f.write_str(msg),
            ConnectError::Other(err) => f.write_str(&err.to_string()),
        }
    }
}

impl std::error::Error for ConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConnectError::Other(err) => Some(err.as_ref()),
            _ => None,
        }
    }
}

async fn connect(
    server_addr: &SocketAddr,
    tls_config: Arc<ClientConfig>,
    event_tx: &mpsc::Sender<InputEvent>,
) -> Result<(), ConnectError> {
    info!(?server_addr, "connecting to server");

    let stream = select! { biased;
        Ok(stream) = TcpStream::connect(server_addr) => {
            stream
        }

        _ = tokio::time::sleep(CONNECT_TIMEOUT) => {
            let msg = format!("failed to connect to the server after {} secs", CONNECT_TIMEOUT.as_secs());
            return Err(ConnectError::Timeout{ msg });
        }
    };

    info!(?server_addr, "connected to server");

    let transporter: ClientTransporter = Transporter::Plain(Transport::new(stream));

    let session = Session {
        server_addr,
        tls_config,
        event_tx,
        transporter,
        state: Default::default(),
    };

    let result = run_session(session).await;

    info!(?server_addr, "disconnected from server");

    result?;

    Ok(())
}

struct Session<'a> {
    server_addr: &'a SocketAddr,
    tls_config: Arc<ClientConfig>,
    event_tx: &'a mpsc::Sender<InputEvent>,
    transporter: ClientTransporter,
    state: SessionState,
}

#[derive(Clone, Copy, Default, Debug)]
pub enum SessionState {
    #[default]
    Handshaking,
    Idle,
    EventRelayed {
        event: InputEvent,
    },
}

async fn run_session(session: Session<'_>) -> Result<(), Error> {
    let Session {
        server_addr,
        tls_config,
        event_tx,
        mut transporter,
        mut state,
    } = session;

    let mut ping_ticker = {
        let interval = Duration::from_secs(5);
        let mut ticker = interval_at(Instant::now() + interval, interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ticker
    };

    let mut local_ping_counter = 1;

    loop {
        state = match state {
            SessionState::Handshaking => {
                debug!(?server_addr, "upgrading to secure transport");

                // upgrade to tls
                transporter = {
                    let tls_config = tls_config.clone();
                    transporter
                        .upgrade(move |stream| async move {
                            upgrade_client_stream(stream, tls_config, server_addr.ip()).await
                        })
                        .await?
                };

                debug!(?server_addr, "connection upgraded");

                info!(?server_addr, "session established");

                SessionState::Idle
            }

            SessionState::Idle => {
                let transport = transporter.secure()?;

                select! { biased;

                    _ = ping_ticker.tick() => {
                        debug!("ping ticker ticks");

                        if local_ping_counter % 2 == 1 {
                            // odd, client send ping
                            let msg = Ping { counter: local_ping_counter }.into();
                            match transport.send_msg(msg).await {
                                Ok(_) => {
                                    debug!("ping sent successfully, incrementing local counter");
                                    local_ping_counter += 1;
                                    SessionState::Idle
                                },
                                Err(err) => {
                                    error!("failed to send ping, {:?}",err);
                                    break;
                                },
                            }
                        } else {
                            // even, client has sent ping a tick before
                            // but client has not receive pong from server
                            info!("terminating session, heartbeat timed out");
                            break;
                        }
                    }

                    Ok(msg) = transport.recv_msg() => {
                        debug!("received message, {:?}", msg);

                        let event = match msg {
                            ServerMessage::Event(event) => Some(event),
                            ServerMessage::Pong(Pong { counter })=> {
                                if counter == local_ping_counter {
                                    debug!("received pong, incrementing local counter, resetting ticker");
                                    ping_ticker.reset();
                                    local_ping_counter += 1;
                                    None
                                } else {
                                    // received pong from server, but counter is mismatch
                                    info!("terminating session, ping counter mismatch");
                                    break;
                                }
                            },
                            _ =>{
                                warn!("received unexpected message, {:?}", msg);
                                None
                            },
                        };

                        match event {
                            Some(event) => SessionState::EventRelayed { event },
                            None => SessionState::Idle
                        }
                    }
                }
            }

            SessionState::EventRelayed { event } => {
                // propagate event to input sink
                event_tx.send(event).await?;

                SessionState::Idle
            }
        };
    }

    Ok(())
}

async fn upgrade_client_stream<S>(
    stream: S,
    tls_config: Arc<ClientConfig>,
    server_addr: IpAddr,
) -> Result<TlsStream<S>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let tls: TlsConnector = tls_config.into();

    let stream = tls
        .connect(ServerName::IpAddress(server_addr), stream)
        .await
        .context("tls connect failed")?;

    Ok(stream.into())
}

fn create_client_tls_config(
    client_certs: Vec<Certificate>,
    client_key: PrivateKey,
    server_cert: Certificate,
) -> Result<ClientConfig, Error> {
    let cert_verifier = Arc::new(SingleCertVerifier::new(server_cert));

    let mut cfg = ClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(cert_verifier)
        .with_single_cert(
            client_certs
                .into_iter()
                .map(|x| rustls::Certificate(x.into()))
                .collect(),
            rustls::PrivateKey(client_key.into()),
        )
        .context("failed to create client config tls")?;

    cfg.enable_sni = false;

    Ok(cfg)
}
