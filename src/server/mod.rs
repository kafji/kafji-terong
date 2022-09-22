mod transport_server;

pub mod config;

use crate::{
    config::Config,
    logging::init_tracing,
    server::{config::ServerConfig, transport_server::TransportServer},
    transport::{generate_tls_key_pair, protocol::Sha256},
};
use cfg_if::cfg_if;
use tokio::{sync::mpsc, try_join};
use tracing::info;

/// Run the server application.
pub async fn run() {
    init_tracing();

    let config @ ServerConfig { port, addr, .. } = Config::read_config()
        .await
        .expect("failed to read config")
        .server();

    let (tls_cert, tls_key) = generate_tls_key_pair(addr).expect("failed to generate tls key pair");

    info!("starting server app");

    println!(
        "Server TLS certificate hash is {}.",
        Sha256::from_bytes(tls_cert.as_ref())
    );

    let (event_tx, event_rx) = mpsc::channel(1);

    let input_source = {
        cfg_if! {
            if #[cfg(target_os = "linux")] {
                crate::input_source::start(
                    config.linux.keyboard_device,
                    config.linux.mouse_device,
                    config.linux.touchpad_device,
                    event_tx
                )
            } else {
                crate::input_source::start(event_tx)
            }
        }
    };

    let server = {
        let args = TransportServer {
            port,
            event_rx,
            tls_cert,
            tls_key,
        };
        transport_server::start(args)
    };

    try_join!(input_source, server).unwrap();

    info!("server app stopped");
}
