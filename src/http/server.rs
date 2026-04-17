use crate::http::FilterHandle;
use crate::metrics;

use anyhow::{Context, Result};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::{rt::TokioExecutor, server::conn::auto::Builder};
use tracing::{debug, error, info, warn};
use serde::Deserialize;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

/// Type-safe enumeration of supported HTTP endpoints.
///
/// This enum provides compile-time safety for routing by ensuring only known
/// endpoints can be processed. It implements `FromStr` for parsing from request paths.
pub enum Endpoint {
    /// Prometheus metrics exporter endpoint at `/metrics`
    Metrics,
    /// Runtime configuration endpoint at `/config`
    Config,
}

impl FromStr for Endpoint {
    /// Error type when parsing an unknown endpoint.
    type Err = &'static str;

    /// Parses a string path into the corresponding `Endpoint` variant.
    ///
    /// # Arguments
    /// * `s` - The request path (e.g., "/metrics")
    ///
    /// # Returns
    /// * `Ok(Endpoint::Metrics)` - If path is "/metrics"
    /// * `Ok(Endpoint::Config)` - If path is "/config"
    /// * `Err("Unknown endpoint")` - For any other path
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "/metrics" => Ok(Endpoint::Metrics),
            "/config" => Ok(Endpoint::Config),
            _ => Err("Unknown endpoint"),
        }
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Endpoint::Metrics => write!(f, "/metrics"),
            Endpoint::Config => write!(f, "/config"),
        }
    }
}

/// JSON body accepted by the `POST /config` endpoint.
#[derive(Deserialize)]
struct ConfigRequest {
    /// New tracing filter directive, e.g. `"debug"` or `"aeroftp=debug,libunftp=warn"`.
    log_level: String,
}

/// Starts an HTTP server that exposes Prometheus metrics and a config endpoint.
///
/// # Arguments
/// * `bind_addr` - The socket address to bind the HTTP server to (e.g., `[::]:9090`)
/// * `filter_handle` - Tracing reload handle for adjusting log levels at runtime
/// * `shutdown` - A broadcast receiver that signals when the server should shut down gracefully
pub async fn start(
    bind_addr: &str,
    filter_handle: FilterHandle,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    let http_addr: SocketAddr = bind_addr
        .parse()
        .with_context(|| format!("unable to parse HTTP address {}", bind_addr))?;

    let listener = TcpListener::bind(http_addr)
        .await
        .with_context(|| format!("unable to bind HTTP address {}", bind_addr))?;

    let http_server = Builder::new(TokioExecutor::new());
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();

    info!("Starting HTTP service, {}", &http_addr);
    info!("Exposing Prometheus metrics at /metrics and runtime config at /config.");

    loop {
        tokio::select! {
            conn = listener.accept() => {
                let (stream, _peer_addr) = match conn {
                    Ok(conn) => conn,
                    Err(e) => {
                        error!("Accept error: {}", e);
                        continue;
                    }
                };

                let handler = Arc::new(HttpHandler {
                    filter_handle: filter_handle.clone(),
                });

                let conn = http_server.serve_connection_with_upgrades(
                    hyper_util::rt::TokioIo::new(stream),
                    service_fn(move |req: Request<Incoming>| {
                        let handler = handler.clone();
                        async move { handler.router(req).await }
                    }),
                );

                let conn = graceful.watch(conn.into_owned());

                tokio::spawn(async move {
                    if let Err(err) = conn.await {
                        debug!("connection error: {}", err);
                    }
                });
            },
            _ = shutdown.recv() => {
                drop(listener);
                info!("shutting down HTTP server");
                break;
            }
        }
    }

    tokio::select! {
        _ = graceful.shutdown() => {
            debug!("all connections closed gracefully");
        },
        _ = tokio::time::sleep(Duration::from_secs(5)) => {
            warn!("shutdown timeout after 5s, forcing close of remaining connections");
        }
    }

    Ok(())
}

/// Internal HTTP request handler.
///
/// Holds shared state needed across requests. Cloning is cheap — all fields are
/// either [`Clone`] or wrapped in [`Arc`].
#[derive(Clone)]
struct HttpHandler {
    filter_handle: FilterHandle,
}

type BoxResponse = http::Result<Response<UnsyncBoxBody<Bytes, Infallible>>>;

impl HttpHandler {
    /// Routes incoming HTTP requests to the appropriate handler.
    ///
    /// | Method | Path      | Handler           |
    /// |--------|-----------|-------------------|
    /// | GET    | /metrics  | [`handle_metrics`]|
    /// | POST   | /config   | [`handle_config`] |
    /// | *      | unknown   | 404 Not Found     |
    /// | *      | known     | 405 Method Not Allowed |
    async fn router(&self, req: Request<Incoming>) -> BoxResponse {
        let method = req.method().clone();
        let path = req.uri().path().to_string();

        match (method, Endpoint::from_str(&path)) {
            (Method::GET, Ok(Endpoint::Metrics)) => self.handle_metrics().await,
            (Method::POST, Ok(Endpoint::Config)) => self.handle_config(req).await,
            (_, Err(_)) => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(UnsyncBoxBody::new(Empty::<Bytes>::new())),
            _ => Response::builder()
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .body(UnsyncBoxBody::new(Empty::<Bytes>::new())),
        }
    }

    /// Handles `GET /metrics` — returns Prometheus-formatted metrics.
    async fn handle_metrics(&self) -> BoxResponse {
        match metrics::gather() {
            Ok(metrics_data) => Ok(Response::new(UnsyncBoxBody::new(Full::new(
                metrics_data.into(),
            )))),
            Err(e) => {
                error!("failed to gather metrics: {}", e);
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(UnsyncBoxBody::new(Empty::<Bytes>::new()))
            }
        }
    }

    /// Handles `POST /config` — updates the active tracing filter at runtime.
    ///
    /// Accepts a JSON body: `{"log_level": "debug"}` or any valid
    /// [tracing `EnvFilter` directive](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html),
    /// e.g. `"aeroftp=debug,libunftp=warn"`.
    ///
    /// # Responses
    /// * `200 OK` — filter updated successfully
    /// * `400 Bad Request` — invalid JSON or unrecognised filter directive
    /// * `500 Internal Server Error` — subscriber is no longer active
    async fn handle_config(&self, req: Request<Incoming>) -> BoxResponse {
        const MAX_BODY: usize = 8 * 1024; // 8 KB is plenty for a log-level string

        let body = match req.into_body().collect().await {
            Ok(b) => b.to_bytes(),
            Err(e) => {
                warn!("failed to read /config request body: {}", e);
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(UnsyncBoxBody::new(Empty::<Bytes>::new()));
            }
        };

        if body.len() > MAX_BODY {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(UnsyncBoxBody::new(Full::new(
                    "request body too large".into(),
                )));
        }

        let config: ConfigRequest = match serde_json::from_slice(&body) {
            Ok(c) => c,
            Err(e) => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(UnsyncBoxBody::new(Full::new(
                        format!("invalid JSON: {e}").into(),
                    )));
            }
        };

        let new_filter = match EnvFilter::try_new(&config.log_level) {
            Ok(f) => f,
            Err(e) => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(UnsyncBoxBody::new(Full::new(
                        format!("invalid log level '{}': {e}", config.log_level).into(),
                    )));
            }
        };

        match self.filter_handle.modify(|f| *f = new_filter) {
            Ok(()) => {
                info!("log level updated to '{}'", config.log_level);
                Response::builder()
                    .status(StatusCode::OK)
                    .body(UnsyncBoxBody::new(Empty::<Bytes>::new()))
            }
            Err(e) => {
                error!("failed to update log filter: {}", e);
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(UnsyncBoxBody::new(Empty::<Bytes>::new()))
            }
        }
    }
}
