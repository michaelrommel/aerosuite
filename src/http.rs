use crate::metrics;

use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{Empty, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use std::convert::Infallible;
use std::{net::SocketAddr, result::Result};
use tokio::net::TcpListener;

const PATH_METRICS: &str = "/metrics";

// starts an HTTP server and exports Prometheus metrics.
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
    let http_server =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();

    println!("Starting HTTP service, {}", &http_addr);
    println!("Exposing Prometheus {} exporter endpoint.", PATH_METRICS);

    loop {
        tokio::select! {
            conn = listener.accept() => {
                let (stream, peer_addr) = match conn {
                    Ok(conn) => conn,
                    Err(e) => {
                        println!("Accept error: {}", e);
                        continue;
                    }
                };
                println!("Incoming connection accepted: {}", peer_addr);

                let stream = hyper_util::rt::TokioIo::new(stream);

                let conn = http_server.serve_connection_with_upgrades(stream, service_fn(move |req: Request<Incoming>| async move {
                    let handler = HttpHandler { };
                    handler.router(req).await
                }));

                let conn = graceful.watch(conn.into_owned());

                tokio::spawn(async move {
                    if let Err(err) = conn.await {
                        println!("connection error: {}", err);
                    }
                    println!("connection dropped: {}", peer_addr);
                });
            },
            _ = shutdown.recv() => {
                drop(listener);
                println!("Shutting down HTTP server");
                break;
            }
        }
    }

    println!("HTTP shutdown OK");
    drop(done);
    Ok(())
}

struct HttpHandler {
    // pub ftp_addr: SocketAddr,
}

impl HttpHandler {
    async fn router(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<UnsyncBoxBody<Bytes, Infallible>>, http::Error> {
        let (parts, _) = req.into_parts();

        let response = match (parts.method, parts.uri.path()) {
            (Method::GET, PATH_METRICS) => Ok(Response::new(UnsyncBoxBody::new(Full::new(
                metrics::gather().into(),
            )))),
            _ => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(UnsyncBoxBody::new(Empty::<Bytes>::new())),
        };

        response
    }
}
