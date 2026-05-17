//! AeroFTP - A secure FTP server with AWS credential support and HTTP metrics.
//!
//! This program implements an FTP server that:
//! - Serves files over FTP on port 21
//! - Exposes Prometheus-compatible metrics on HTTP (default: [::]:9090)
//! - Supports graceful shutdown via HUP, INT, and TERM signals
//! - Automatically restarts on HUP signal, exits on INT/TERM
//! - Uses cached AWS credentials from EC2 metadata, ECS, or EKS providers

mod ftp;
mod http;
mod metrics;
mod signal;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{trace as sdktrace, Resource};
use rustls::crypto::aws_lc_rs as rustls_provider;
use tokio::task::JoinSet;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, reload, util::SubscriberInitExt, EnvFilter};

use http::FilterHandle;

// ---------------------------------------------------------------------------
// OTel shutdown guard
// ---------------------------------------------------------------------------

/// Holds the [`SdkTracerProvider`] for its lifetime.
///
/// On drop the provider is shut down gracefully, which flushes the
/// in-flight batch exporter queue so no spans are lost on exit.
struct OtelGuard {
    provider: sdktrace::SdkTracerProvider,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        if let Err(e) = self.provider.shutdown() {
            eprintln!("[aeroftp] OTel provider failed to shut down cleanly: {e}");
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Both aws-lc-rs (via libunftp) and ring (via redis's tls-rustls feature)
    // are compiled into the binary.  Rustls panics at runtime if it cannot
    // auto-detect a single provider, so we pin it to aws-lc-rs explicitly
    // before anything touches TLS.
    rustls_provider::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // `_otel_guard` must stay alive until main returns so the OTel batch
    // exporter has a chance to flush every span before the process exits.
    // When OTEL_SDK_DISABLED=true it is None and the Drop is a no-op.
    let (filter_handle, _otel_guard) = init_tracing()?;

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

/// Initialise tracing and return a handle for adjusting the filter at runtime,
/// plus an optional [`OtelGuard`] that must be kept alive for the process lifetime.
///
/// **What happens here:**
/// 1. A reloadable [`EnvFilter`] layer is built (honouring `RUST_LOG`).
/// 2. Unless `OTEL_SDK_DISABLED=true`, an OTLP gRPC span exporter is configured,
///    pointing at the Tempo endpoint supplied via `OTEL_EXPORTER_OTLP_ENDPOINT`
///    (default: `http://aeromon.aerosuite:4317`).
/// 3. A [`SdkTracerProvider`] with a background batch exporter is wired up.
/// 4. [`tracing_opentelemetry::layer()`] bridges every `tracing` span /
///    `#[instrument]` annotation into an OTEL span that flows to Tempo.
/// 5. All layers - filter, fmt, otel (if enabled), and optionally tokio-console
///    - are installed into the global subscriber in one shot.
///
/// # Env vars
/// * `RUST_LOG`                      - log / span filter (e.g. `aeroftp=debug`)
/// * `OTEL_SDK_DISABLED`             - set to `true` to disable all OTEL export
///                                     (no spans sent, no exporter threads created)
/// * `OTEL_EXPORTER_OTLP_ENDPOINT`   - Tempo gRPC address
///   (default: `http://aeromon.aerosuite:4317`)
/// * `OTEL_SERVICE_NAME`             - overrides the default `"aeroftp"` label
fn init_tracing() -> anyhow::Result<(FilterHandle, Option<OtelGuard>)> {
    // ── 1. Reloadable env-filter (powers POST /config) ──────────────────────
    let filter = EnvFilter::from_default_env();
    let (filter_layer, handle) = reload::Layer::new(filter);

    // ── 2. OTEL enabled? ─────────────────────────────────────────────────────
    // The standard OpenTelemetry kill-switch: OTEL_SDK_DISABLED=true strips the
    // entire export pipeline - no exporter threads, no batch queue, no tonic
    // gRPC channel.  Set it in /etc/conf.d/aeroftp to run without OTEL.
    let otel_disabled = std::env::var("OTEL_SDK_DISABLED")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    // ── 3. Optionally build the OTLP export pipeline ─────────────────────────
    let (otel_layer, guard) = if otel_disabled {
        eprintln!("[aeroftp] OTEL_SDK_DISABLED=true - OpenTelemetry export is off");
        (None, None)
    } else {
        // Tempo lives in the aeromon task; its OTLP/gRPC port is 4317.
        // On ECS (awsvpc) backends reach it by the aeromon task's private IP or
        // an internal ECS Service Connect / Cloud Map DNS name.
        let otlp_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .unwrap_or_else(|_| "http://aeromon.aerosuite:4317".to_string());

        let service_name =
            std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "aeroftp".to_string());

        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(&otlp_endpoint)
            .build()?;

        // `rt-tokio` spawns the batch worker on the existing Tokio runtime so we
        // don't pay for a separate thread.
        let provider = sdktrace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(Resource::builder().with_service_name(service_name).build())
            .build();

        opentelemetry::global::set_tracer_provider(provider.clone());
        let tracer = provider.tracer("aeroftp");
        let layer = tracing_opentelemetry::layer()
            .with_tracer(tracer)
            .with_tracked_inactivity(false); // disable busy_ns/idle_ns on every span -
                                             // those accumulate task scheduler idle time
                                             // and make span durations misleading in Tempo
        (Some(layer), Some(OtelGuard { provider }))
    };

    // ── 4. Assemble the full subscriber registry ─────────────────────────────
    // Option<L> is a valid no-op Layer when otel_layer is None, so the
    // subscriber type is the same regardless of the OTEL_SDK_DISABLED flag.
    let registry = tracing_subscriber::registry()
        .with(filter_layer)
        .with(tracing_subscriber::fmt::layer())
        .with(otel_layer);

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

    Ok((handle, guard))
}
