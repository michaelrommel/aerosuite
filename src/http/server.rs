use crate::metrics;

use anyhow::{Context, Result};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{Empty, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::{rt::TokioExecutor, server::conn::auto::Builder};
use log::{debug, error, info, warn};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

/// Type-safe enumeration of supported HTTP endpoints.
///
/// This enum provides compile-time safety for routing by ensuring only known
/// endpoints can be processed. It implements `FromStr` for parsing from request paths.
pub enum Endpoint {
    /// Prometheus metrics exporter endpoint at `/metrics`
    Metrics,
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
    /// * `Err("Unknown endpoint")` - For any other path
    ///
    /// # Examples
    /// ```
    /// use std::str::FromStr;
    /// use aeroftp::http::server::Endpoint;
    ///
    /// let result = Endpoint::from_str("/metrics");
    /// assert!(result.is_ok());
    ///
    /// let invalid = Endpoint::from_str("/health");
    /// assert!(invalid.is_err());
    /// ```
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "/metrics" => Ok(Endpoint::Metrics),
            _ => Err("Unknown endpoint"),
        }
    }
}

impl std::fmt::Display for Endpoint {
    /// Formats the endpoint as its corresponding URL path.
    ///
    /// # Examples
    /// ```
    /// use aeroftp::http::server::Endpoint;
    ///
    /// assert_eq!(format!("{}", Endpoint::Metrics), "/metrics");
    /// ```
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Endpoint::Metrics => write!(f, "/metrics"),
        }
    }
}

/// Starts an HTTP server that exposes Prometheus-compatible metrics.
///
/// This function initializes a lightweight HTTP server using `hyper` that:
/// * Listens on the specified address (default: `[::]:9090`)
/// * Serves `/metrics` endpoint with Prometheus-formatted output
/// * Supports graceful shutdown via broadcast channels
/// * Handles connections asynchronously with Tokio executor
///
/// # Arguments
/// * `bind_addr` - The socket address to bind the HTTP server to (e.g., `[::]:9090`)
/// * `shutdown` - A broadcast receiver that signals when the server should shut down gracefully
///
/// # Returns
/// * `Ok(())` - Server started and eventually shut down cleanly
/// * `Err(anyhow::Error)` - If binding to the address fails or other startup errors occur
///
/// # Examples
/// ```no_run
/// use aeroftp::http;
/// use tokio::sync::{broadcast};
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let (shutdown_sender, shutdown_receiver) = broadcast::channel(1);
///     
///     // Start HTTP metrics server
///     http::start("[::]:9090", shutdown_receiver).await?;
///     
///     Ok(())
/// }
/// ```
pub async fn start(
    bind_addr: &str,
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
    info!("Exposing Prometheus metrics exporter endpoint.");

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

                let handler = Arc::new(HttpHandler {});

                let conn = http_server.serve_connection_with_upgrades(
                    hyper_util::rt::TokioIo::new(stream),
                    service_fn(move |req: Request<Incoming>| {
                        // Arc clone is cheap - only increments reference count
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
                // Stop accepting new connections
                drop(listener);
                info!("shutting down HTTP server");
                break;
            }
        }
    }

    // Wait for all spawned connections to complete (with timeout)
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

/// Internal HTTP request handler for the metrics endpoint.
///
/// This struct routes incoming requests to appropriate handlers based on
/// method and path. Currently only supports GET /metrics.
///
/// # Thread Safety
///
/// `HttpHandler` is wrapped in an [`Arc`](std::sync::Arc) for efficient sharing
/// across multiple async tasks. The handler itself is stateless, so cloning the
/// `Arc` only increments a reference counter (no heap allocation).
#[derive(Clone)]
struct HttpHandler {}

impl HttpHandler {
    /// Routes incoming HTTP requests to the appropriate handler based on method and path.
    ///
    /// This method implements a two-phase routing strategy:
    /// 1. First validates the HTTP method (only GET is allowed)
    /// 2. Then parses the path into a typed `Endpoint` enum
    /// 3. Finally dispatches to the corresponding handler
    ///
    /// # Arguments
    /// * `req` - The incoming HTTP request
    ///
    /// # Returns
    /// * `Ok(Response)` - A valid response (200 OK, 404 Not Found, or 405 Method Not Allowed)
    /// * `Err(http::Error)` - If there's an internal error constructing the response
    #[must_use = "router result must be used to send HTTP response"]
    async fn router(
        &self,
        req: Request<Incoming>,
    ) -> http::Result<Response<UnsyncBoxBody<Bytes, Infallible>>> {
        let (parts, _) = req.into_parts();
        let method = parts.method;
        let path = parts.uri.path();

        // Match on method+path tuple for clean endpoint routing
        match (method, Endpoint::from_str(path)) {
            (Method::GET, Ok(Endpoint::Metrics)) => self.handle_metrics().await,
            (Method::GET, Err(_)) => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(UnsyncBoxBody::new(Empty::<Bytes>::new())),
            _ => Response::builder()
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .body(UnsyncBoxBody::new(Empty::<Bytes>::new())),
        }
    }

    /// Handles requests to the `/metrics` Prometheus exporter endpoint.
    ///
    /// This method gathers all registered Prometheus metrics and encodes them
    /// in the text format expected by Prometheus scrapers.
    ///
    /// # Returns
    /// * `Ok(Response<Full<Bytes>>)` - 200 OK with metrics data in text format
    /// * `Err(http::Error)` - If there's an internal error constructing the response
    async fn handle_metrics(&self) -> http::Result<Response<UnsyncBoxBody<Bytes, Infallible>>> {
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
}
