use crate::metrics;

use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{Empty, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::{rt::TokioExecutor, server::conn::auto::Builder};
use log::{debug, error, info};
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio::net::TcpListener;

const PATH_METRICS: &str = "/metrics";

/// Start the HTTP metrics server.
pub async fn start(
    bind_addr: &str,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
    done: tokio::sync::mpsc::Sender<()>,
) -> Result<(), String> {
    let http_addr: SocketAddr = bind_addr
        .parse()
        .map_err(|e| format!("unable to parse HTTP address {}: {}", bind_addr, e))?;

    let listener = TcpListener::bind(http_addr)
        .await
        .map_err(|e| format!("unable to parse HTTP address {}: {}", bind_addr, e))?;

    let http_server = Builder::new(TokioExecutor::new());
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();

    info!("Starting HTTP service, {}", &http_addr);
    info!("Exposing Prometheus {} exporter endpoint.", PATH_METRICS);

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

                let handler = HttpHandler {};

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
                info!("Shutting down HTTP server");
                break;
            }
        }
    }

    debug!("HTTP shutdown OK");
    drop(done);
    Ok(())
}

#[derive(Clone)]
/// HTTP request handler for metrics endpoint.
struct HttpHandler {
    // pub ftp_addr: SocketAddr,
}

impl HttpHandler {
    async fn router(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<UnsyncBoxBody<Bytes, Infallible>>, http::Error> {
        let (parts, _) = req.into_parts();

        match (parts.method, parts.uri.path()) {
            (Method::GET, PATH_METRICS) => {
                if let Ok(metrics_data) = metrics::gather() {
                    Ok(Response::new(UnsyncBoxBody::new(Full::new(
                        metrics_data.into(),
                    ))))
                } else {
                    error!("Failed to gather metrics");
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(UnsyncBoxBody::new(Empty::<Bytes>::new()))
                }
            }
            _ => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(UnsyncBoxBody::new(Empty::<Bytes>::new())),
        }
    }
}
