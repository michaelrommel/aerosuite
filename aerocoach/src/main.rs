//! aerocoach — controller and aggregator for aerosuite load tests.
//!
//! Reads configuration from environment variables, optionally loads a JSON
//! load plan, then starts:
//!
//! - A **gRPC server** (tonic) on `AEROCOACH_GRPC_PORT` (default 50051) that
//!   accepts agent `Register` and `Session` calls.
//! - An **HTTP server** (axum) on `AEROCOACH_HTTP_PORT` (default 8080) with
//!   control and status endpoints.
//!
//! ## HTTP endpoints (current)
//! | Method | Path       | Description                                |
//! |--------|------------|--------------------------------------------|
//! | GET    | `/health`  | Liveness probe                             |
//! | GET    | `/status`  | JSON state snapshot                        |
//! | POST   | `/start`   | Start the slice clock (WAITING → RUNNING)  |
//! | POST   | `/stop`    | Graceful stop (RUNNING → DONE)             |

mod config;
mod grpc;
mod model;
mod state;
mod ws;

use std::net::SocketAddr;

use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tracing::{error, info, warn};

use aeroproto::aeromonitor::agent_service_server::AgentServiceServer;

use crate::config::Config;
use crate::grpc::agent_service::AgentServiceImpl;
use crate::model::{clock::SliceClock, LoadPlanFile};
use crate::state::{new_shared_state, SharedState};

// ── Entry point ───────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // ── Tracing ───────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(std::env::var_os("NO_COLOR").is_none())
        .init();

    // ── Config ────────────────────────────────────────────────────────────
    let config = Config::from_env()?;
    info!(
        grpc_port  = config.grpc_port,
        http_port  = config.http_port,
        record_dir = %config.record_dir.display(),
        "aerocoach starting"
    );

    // ── Shared state ──────────────────────────────────────────────────────
    let shared = new_shared_state();

    // ── Load plan (optional) ──────────────────────────────────────────────
    if let Some(ref path) = config.plan_file {
        match LoadPlanFile::load(path) {
            Ok(plan) => {
                info!(
                    plan_id  = %plan.plan_id,
                    slices   = plan.total_slices(),
                    slice_ms = plan.slice_duration_ms,
                    bw_bps   = plan.total_bandwidth_bps,
                    buckets  = plan.file_distribution.buckets.len(),
                    "load plan loaded"
                );
                shared.write().await.load_plan = Some(plan);
            }
            Err(e) => {
                warn!(
                    path  = %path.display(),
                    error = %e,
                    "could not load plan file — continuing without a plan"
                );
            }
        }
    } else {
        info!("no AEROCOACH_PLAN_FILE set — supply one via PUT /plan before starting");
    }

    // ── gRPC server ───────────────────────────────────────────────────────
    let grpc_addr: SocketAddr = format!("0.0.0.0:{}", config.grpc_port).parse()?;
    let grpc_service = AgentServiceImpl::new(shared.clone());
    let grpc_server = tonic::transport::Server::builder()
        .add_service(AgentServiceServer::new(grpc_service))
        .serve(grpc_addr);

    // ── HTTP server ───────────────────────────────────────────────────────
    let http_addr: SocketAddr = format!("0.0.0.0:{}", config.http_port).parse()?;
    let http_app = Router::new()
        .route("/health", get(health_handler))
        .route("/status", get(status_handler))
        .route("/start",  post(start_handler))
        .route("/stop",   post(stop_handler))
        .route("/reset",  post(reset_handler))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind(http_addr).await?;

    info!(grpc = %grpc_addr, http = %http_addr, "servers listening");

    // ── Run until one server exits or shutdown signal ─────────────────────
    tokio::select! {
        res = grpc_server => {
            if let Err(e) = res { error!(error = %e, "gRPC server exited with error"); }
        }
        res = axum::serve(listener, http_app) => {
            if let Err(e) = res { error!(error = %e, "HTTP server exited with error"); }
        }
        _ = wait_for_shutdown() => {
            info!("shutdown signal received — exiting");
        }
    }

    Ok(())
}

// ── HTTP handlers ─────────────────────────────────────────────────────────

async fn health_handler() -> &'static str {
    "OK"
}

async fn status_handler(State(state): State<SharedState>) -> Json<Value> {
    let read = state.read().await;
    let agents: Vec<Value> = read
        .registry
        .status_snapshot()
        .into_iter()
        .map(|a| {
            json!({
                "agent_id":           a.agent_id,
                "agent_index":        a.agent_index,
                "private_ip":         a.private_ip,
                "current_slice":      a.current_slice,
                "active_connections": a.active_connections,
                "connected":          a.connected,
            })
        })
        .collect();

    Json(json!({
        "state":        read.coach_state.to_string(),
        "agent_count":  read.registry.len(),
        "connected":    read.registry.connected_count(),
        "plan_id":      read.load_plan.as_ref().map(|p| p.plan_id.as_str()),
        "total_slices": read.load_plan.as_ref().map(|p| p.total_slices()),
        "current_slice":read.coach_state.current_slice(),
        "agents":       agents,
    }))
}

/// `POST /start` — transition WAITING → RUNNING and spawn the slice clock.
async fn start_handler(
    State(state): State<SharedState>,
) -> impl IntoResponse {
    // Validate preconditions under a write lock to make the check-then-act
    // atomic.
    let (ack_notify, stop_rx) = {
        let read = state.read().await;

        if !read.coach_state.is_waiting() {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "cannot start: coach is in state {}, must be WAITING",
                        read.coach_state
                    )
                })),
            );
        }

        if read.load_plan.is_none() {
            return (
                StatusCode::PRECONDITION_FAILED,
                Json(json!({ "error": "no load plan loaded; supply one via PUT /plan first" })),
            );
        }

        if read.registry.connected_count() == 0 {
            warn!("POST /start called with no connected agents — clock will run but no agents will receive ticks");
        }

        // Reset any previous stop signal before we create the receiver.
        read.reset_stop();
        (read.ack_notify.clone(), read.subscribe_stop())
    };

    // Transition to RUNNING (slice 0 is set by the clock on the first tick).
    {
        let mut write = state.write().await;
        write.coach_state = crate::state::CoachState::Running { current_slice: 0 };
    }

    // Spawn the slice clock as a background task.
    let clock = SliceClock::new(state.clone(), ack_notify, stop_rx);
    tokio::spawn(clock.run());

    info!("test started — slice clock spawned");

    (
        StatusCode::OK,
        Json(json!({ "status": "started", "agents": state.read().await.registry.connected_count() })),
    )
}

/// `POST /stop` — request graceful early termination.
async fn stop_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let read = state.read().await;

    if !read.coach_state.is_running() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "cannot stop: coach is in state {}, must be RUNNING",
                    read.coach_state
                )
            })),
        );
    }

    read.signal_stop();
    info!("stop signal sent to slice clock");

    (StatusCode::OK, Json(json!({ "status": "stopping" })))
}

/// `POST /reset` — transition DONE → WAITING so agents can re-register and
/// the same plan (or a newly loaded one) can be executed again.
async fn reset_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let mut write = state.write().await;

    if !write.coach_state.is_done() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "cannot reset: coach is in state {}, must be DONE",
                    write.coach_state
                )
            })),
        );
    }

    write.reset();
    info!("aerocoach reset — back to WAITING, agents may re-register");

    (StatusCode::OK, Json(json!({ "status": "waiting" })))
}

// ── Shutdown signal ───────────────────────────────────────────────────────

async fn wait_for_shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigint  = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = sigint.recv()  => {}
        _ = sigterm.recv() => {}
    }
}
