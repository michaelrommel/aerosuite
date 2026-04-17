//! AeroFTP - A secure FTP server with AWS credential support and HTTP metrics.
//!
//! This program implements an FTP server that:
//! - Serves files over FTP on port 21
//! - Exposes Prometheus-compatible metrics on HTTP (default: [::]:9090)
//! - Supports graceful shutdown via HUP, INT, and TERM signals
//! - Automatically restarts on HUP signal, exits on INT/TERM
//! - Uses cached AWS credentials from EC2 metadata, ECS, or EKS providers

mod aws;
mod ftp;
mod http;
mod metrics;
mod signal;

use tracing::{error, info, warn};
use tokio::task::JoinSet;
use tracing_subscriber::{reload, EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use http::FilterHandle;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Keep the handle alive for the duration of the program so the
    // /config endpoint can update the log level at runtime.
    let filter_handle = init_tracing()?;

    run(filter_handle).await?;

    Ok(())
}

/// Execute the main application loop.
///
/// Spawns the HTTP metrics server, starts the FTP server, and listens for
/// signals. Restarts on HUP signal, exits on INT/TERM.
///
/// # Arguments
/// * `filter_handle` - Tracing reload handle forwarded to the HTTP server
///
/// # Returns
/// * `Ok(())` - Application exited normally
async fn run(filter_handle: FilterHandle) -> anyhow::Result<()> {
    while main_task(filter_handle.clone()).await? == signal::ExitSignal::Hup {
        info!("Restarting on HUP");
    }
    info!("Exiting");
    Ok(())
}

/// Execute one iteration of the main task.
///
/// Starts HTTP metrics server, FTP server, and waits for signals.
///
/// # Returns
/// * `Ok(ExitSignal::Hup)` - Restart requested
/// * `Ok(ExitSignal::Int|Term)` - Exit requested
async fn main_task(filter_handle: FilterHandle) -> anyhow::Result<signal::ExitSignal> {
    const BROADCAST_CAPACITY: usize = 32;
    const METRICS_BIND_ADDRESS: &str = "[::]:9090";

    // Shutdown coordination channels
    let (shutdown_sender, http_receiver) = tokio::sync::broadcast::channel(BROADCAST_CAPACITY);
    let ftp_shutdown_handle = shutdown_sender.clone();

    // Use JoinSet for structured concurrency - tracks both server tasks
    let mut join_set = JoinSet::<()>::new();

    // Spawn HTTP metrics server
    join_set.spawn(async move {
        if let Err(e) = http::start(METRICS_BIND_ADDRESS, filter_handle, http_receiver).await {
            error!("HTTP Server error: {}", e);
        }
    });

    // Spawn FTP server with its own shutdown receiver
    join_set.spawn(async move {
        if let Err(e) = ftp::start_ftp(ftp_shutdown_handle.subscribe()).await {
            error!("FTP Server error: {}", e);
        }
    });

    match signal::listen_for_signals().await {
        Ok(signal) => {
            info!("Received signal {}, shutting down...", signal);
            drop(shutdown_sender); // Signal both servers to stop

            // Wait for all spawned tasks to complete with timeout
            while let Some(result) = join_set.join_next().await {
                if let Err(e) = result {
                    warn!("Server task cancelled or panicked: {}", e);
                }
            }

            Ok(signal)
        }
        Err(e) => {
            // Ensure servers receive shutdown signal even on error
            drop(shutdown_sender);
            Err(e)
        }
    }
}

/// Initialise tracing and return a handle for adjusting the filter at runtime.
///
/// Bridges `log::` macros so existing call sites require no changes.
/// `RUST_LOG` is honoured at startup (e.g. `RUST_LOG=aeroftp=debug`).
///
/// When the `tokio_console` feature is enabled, the Tokio Console layer is
/// added automatically on `127.0.0.1:6669`.
fn init_tracing() -> anyhow::Result<FilterHandle> {
    let filter = EnvFilter::from_default_env();
    let (filter_layer, handle) = reload::Layer::new(filter);

    let registry = tracing_subscriber::registry()
        .with(filter_layer)
        .with(tracing_subscriber::fmt::layer());

    #[cfg(not(feature = "tokio_console"))]
    registry.init();

    #[cfg(feature = "tokio_console")]
    {
        use anyhow::{bail, Context};
        use std::net::SocketAddr;

        let console_addr: SocketAddr = "127.0.0.1:6669"
            .parse()
            .context("could not parse tokio-console address")?;

        let (ip, port) = match console_addr {
            SocketAddr::V4(addr) => (addr.ip().octets(), addr.port()),
            SocketAddr::V6(_) => bail!("tokio-console only supports IPv4 addresses"),
        };

        let console_layer = console_subscriber::ConsoleLayer::builder()
            .server_addr((ip, port))
            .spawn();

        registry.with(console_layer).init();
    }

    Ok(handle)
}
