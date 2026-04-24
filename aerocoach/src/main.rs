//! aerocoach — controller and aggregator for aerosuite load tests.
//!
//! Reads configuration from environment variables, optionally loads a JSON
//! load plan, then starts:
//!
//! - A **gRPC server** (tonic) on `AEROCOACH_GRPC_PORT` (default 50051) that
//!   accepts agent `Register` and `Session` calls.
//! - An **HTTP / WebSocket server** (axum) on `AEROCOACH_HTTP_PORT` (default 8080)
//!   with control, status, plan, and result endpoints.
//! - A **delta ticker** task that broadcasts a `DashboardUpdate` JSON payload
//!   to all WebSocket clients every 3 seconds.
//!
//! ## HTTP endpoints
//! | Method | Path         | Description                                       |
//! |--------|--------------|---------------------------------------------------|
//! | GET    | `/health`    | Liveness probe                                    |
//! | GET    | `/status`    | JSON state snapshot                               |
//! | GET    | `/plan`      | Return the active load plan as JSON               |
//! | PUT    | `/plan`      | Replace the full plan (WAITING only)              |
//! | PATCH  | `/plan`      | Partial update from `effective_from_slice` onward |
//! | POST   | `/start`     | Start the slice clock (WAITING → RUNNING)         |
//! | POST   | `/stop`      | Graceful stop (RUNNING → DONE)                    |
//! | POST   | `/reset`     | Return to WAITING (DONE only)                     |
//! | POST   | `/bandwidth` | Hot-update total bandwidth                        |
//! | GET    | `/results`   | Download NDJSON record file (DONE)                |
//! | GET    | `/ws`        | WebSocket upgrade for aerotrack                   |

mod config;
mod grpc;
mod model;
mod ndjson_writer;
mod state;
mod ws;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::MissedTickBehavior;
use tracing::{error, info, warn};

use aeroproto::aeromonitor::agent_service_server::AgentServiceServer;
use aeroproto::aeromonitor::{coach_command, CoachCommand, LoadPlanUpdate, ShutdownCmd, TimeSlice, TransferRecord};

use crate::config::Config;
use crate::grpc::agent_service::AgentServiceImpl;
use crate::model::load_plan::SliceSpec;
use crate::model::{clock::SliceClock, LoadPlanFile};
use crate::ndjson_writer::NdjsonWriter;
use crate::state::delta::DeltaEngine;
use crate::state::{new_shared_state, SharedState};
use crate::ws::broadcaster::ws_handler;

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
    // Store the record directory so HTTP handlers can open the NDJSON writer.
    shared.write().await.record_dir = config.record_dir.clone();

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

    // ── Delta ticker (broadcasts DashboardUpdate every 3 s) ───────────────
    {
        let ticker_state = shared.clone();
        tokio::spawn(async move {
            let mut engine = DeltaEngine::new();
            let mut ticker = tokio::time::interval(Duration::from_secs(3));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                ticker.tick().await;

                // Collect all data under a single write lock, then release
                // before the (possibly slow) JSON serialisation step.
                let (drained, snapshot, totals_map, current_slice, total_slices, ws_tx) = {
                    let mut write = ticker_state.write().await;

                    let drained   = write.metrics.drain_completed();
                    let snapshot  = write.registry.status_snapshot();
                    let totals_map: HashMap<String, _> = snapshot
                        .iter()
                        .map(|s| (s.agent_id.clone(), write.metrics.agent_totals(&s.agent_id)))
                        .collect();
                    let current_slice = write.coach_state.current_slice().unwrap_or(0);
                    let total_slices  = write
                        .load_plan
                        .as_ref()
                        .map(|p| p.total_slices())
                        .unwrap_or(0);
                    let ws_tx = write.ws_tx.clone();

                    // Write drained records to NDJSON file.
                    let is_done = write.coach_state.is_done();
                    if let Some(ref mut writer) = write.record_writer {
                        for (agent_id, record) in &drained {
                            if let Err(e) = writer.append(agent_id, record) {
                                warn!(error = %e, "NDJSON append failed");
                            }
                        }
                        // Once the test is done, synthesize error records for any
                        // in-flight (incomplete) transfers, flush, and close the writer.
                        if is_done {
                            let now_ms = chrono::Utc::now().timestamp_millis();
                            let mut incomplete_total: u32 = 0;
                            for agent in &snapshot {
                                if agent.active_connections == 0 {
                                    continue;
                                }
                                incomplete_total += agent.active_connections;
                                for conn_idx in 0..agent.active_connections {
                                    let record = TransferRecord {
                                        filename: format!(
                                            "{}_slice{}_incomplete_{}",
                                            agent.agent_id,
                                            agent.current_slice,
                                            conn_idx
                                        ),
                                        bucket_id:         String::new(),
                                        bytes_transferred: 0,
                                        file_size_bytes:   0,
                                        bandwidth_kibps:   0,
                                        success:           false,
                                        error_reason:      Some(
                                            "transfer incomplete: test ended before completion"
                                                .to_string(),
                                        ),
                                        start_time_ms: 0,
                                        end_time_ms:   now_ms,
                                        time_slice:    agent.current_slice,
                                    };
                                    if let Err(e) = writer.append(&agent.agent_id, &record) {
                                        warn!(
                                            error    = %e,
                                            agent_id = %agent.agent_id,
                                            "NDJSON append failed for incomplete transfer"
                                        );
                                    }
                                }
                            }
                            if incomplete_total > 0 {
                                warn!(
                                    count = incomplete_total,
                                    "wrote synthetic error records for incomplete in-flight transfers"
                                );
                            }
                            if let Err(e) = writer.flush() {
                                warn!(error = %e, "NDJSON flush failed");
                            }
                        }
                    }
                    // Only log + drop the writer once — if it was already None
                    // (every subsequent tick after Done) this is a no-op.
                    if is_done && write.record_writer.take().is_some() {
                        info!("NDJSON record file closed");
                    }

                    (drained, snapshot, totals_map, current_slice, total_slices, ws_tx)
                };

                let update = engine.compute(
                    current_slice,
                    total_slices,
                    &snapshot,
                    |id| totals_map.get(id).cloned().unwrap_or_default(),
                    &drained,
                );

                match serde_json::to_string(&update) {
                    Ok(json) => {
                        let _ = ws_tx.send(json);
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to serialise DashboardUpdate");
                    }
                }
            }
        });
    }

    // ── gRPC server ───────────────────────────────────────────────────────
    let grpc_addr: SocketAddr = format!("0.0.0.0:{}", config.grpc_port).parse()?;
    let grpc_service = AgentServiceImpl::new(shared.clone());
    let grpc_server = tonic::transport::Server::builder()
        .add_service(AgentServiceServer::new(grpc_service))
        .serve(grpc_addr);

    // ── HTTP / WebSocket server ───────────────────────────────────────────
    let http_addr: SocketAddr = format!("0.0.0.0:{}", config.http_port).parse()?;
    let http_app = Router::new()
        .route("/health",    get(health_handler))
        .route("/status",    get(status_handler))
        .route("/plan",      get(plan_get_handler).put(plan_put_handler).patch(plan_patch_handler))
        .route("/start",     post(start_handler))
        .route("/stop",      post(stop_handler))
        .route("/reset",     post(reset_handler))
        .route("/bandwidth", post(bandwidth_handler))
        .route("/results",   get(results_handler))
        .route("/ws",        get(ws_handler))
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
        "state":         read.coach_state.to_string(),
        "agent_count":   read.registry.len(),
        "connected":     read.registry.connected_count(),
        "plan_id":       read.load_plan.as_ref().map(|p| p.plan_id.as_str()),
        "total_slices":  read.load_plan.as_ref().map(|p| p.total_slices()),
        "current_slice": read.coach_state.current_slice(),
        "agents":        agents,
    }))
}

// ── Plan endpoints ────────────────────────────────────────────────────────

/// `GET /plan` — return the currently loaded plan as JSON.
async fn plan_get_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let read = state.read().await;
    match read.load_plan.as_ref() {
        Some(plan) => match serde_json::to_value(plan) {
            Ok(v)  => (StatusCode::OK, Json(v)).into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response(),
        },
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no plan is loaded; supply one via PUT /plan" })),
        )
            .into_response(),
    }
}

/// `PUT /plan` — replace the full load plan.  Only accepted while WAITING.
async fn plan_put_handler(
    State(state): State<SharedState>,
    Json(body): Json<LoadPlanFile>,
) -> impl IntoResponse {
    if let Err(e) = body.validate() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    let mut write = state.write().await;
    if !write.coach_state.is_waiting() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "cannot replace plan while in state {}; plan replacement requires WAITING",
                    write.coach_state
                )
            })),
        )
            .into_response();
    }

    let plan_id = body.plan_id.clone();
    info!(plan_id = %plan_id, "plan replaced via PUT /plan");
    write.load_plan = Some(body);
    (StatusCode::OK, Json(json!({ "status": "ok", "plan_id": plan_id }))).into_response()
}

/// Request body for `PATCH /plan`.
#[derive(Debug, Deserialize)]
struct PatchPlanRequest {
    effective_from_slice: u32,
    updated_slices:       Vec<SliceSpec>,
    new_bandwidth_bps:    Option<u64>,
}

/// `PATCH /plan` — partial plan update, effective from a given slice onward.
///
/// Accepted in WAITING or RUNNING state.  If RUNNING, also broadcasts a
/// `LoadPlanUpdate` command to all connected agents.
async fn plan_patch_handler(
    State(state): State<SharedState>,
    Json(body): Json<PatchPlanRequest>,
) -> impl IntoResponse {
    let mut write = state.write().await;

    if write.coach_state.is_done() {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "cannot patch plan in DONE state" })),
        )
            .into_response();
    }

    let Some(ref mut plan) = write.load_plan else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no plan is loaded" })),
        )
            .into_response();
    };

    // Apply slice updates.
    for spec in &body.updated_slices {
        if let Some(s) = plan.slices.iter_mut().find(|s| s.slice_index == spec.slice_index) {
            s.total_connections = spec.total_connections;
        }
    }
    if let Some(bps) = body.new_bandwidth_bps {
        plan.total_bandwidth_bps = bps;
    }

    info!(
        effective_from = body.effective_from_slice,
        slices_patched = body.updated_slices.len(),
        new_bw         = ?body.new_bandwidth_bps,
        "plan patched"
    );

    // If RUNNING, forward the update to agents.
    if write.coach_state.is_running() {
        let proto_slices: Vec<TimeSlice> = body
            .updated_slices
            .iter()
            .map(|s| TimeSlice {
                slice_index:       s.slice_index,
                total_connections: s.total_connections,
            })
            .collect();
        let cmd = CoachCommand {
            payload: Some(coach_command::Payload::PlanUpdate(LoadPlanUpdate {
                effective_from_slice: body.effective_from_slice,
                updated_slices:       proto_slices,
                new_bandwidth_bps:    body.new_bandwidth_bps,
                new_file_distribution: None,
                new_total_agents:     None,
            })),
        };
        let n = write.registry.broadcast(cmd);
        info!(agents = n, "LoadPlanUpdate broadcast to agents");
    }

    (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response()
}

// ── Lifecycle endpoints ───────────────────────────────────────────────────

/// `POST /start` — transition WAITING → RUNNING and spawn the slice clock.
async fn start_handler(State(state): State<SharedState>) -> impl IntoResponse {
    // Validate preconditions.
    let (ack_notify, stop_rx, plan_id, record_dir) = {
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
            )
                .into_response();
        }
        if read.load_plan.is_none() {
            return (
                StatusCode::PRECONDITION_FAILED,
                Json(json!({ "error": "no load plan loaded; supply one via PUT /plan first" })),
            )
                .into_response();
        }
        if read.registry.connected_count() == 0 {
            warn!("POST /start called with no connected agents — clock will tick but no agents receive commands");
        }

        read.reset_stop();
        let plan_id    = read.load_plan.as_ref().map(|p| p.plan_id.clone()).unwrap_or_default();
        let record_dir = read.record_dir.clone();
        (read.ack_notify.clone(), read.subscribe_stop(), plan_id, record_dir)
    };

    // Open the NDJSON record writer (synchronous I/O — fast mkdir + open).
    let writer_result = NdjsonWriter::open(&record_dir, &plan_id);

    // Transition to RUNNING and install the writer.
    {
        let mut write = state.write().await;
        write.coach_state = crate::state::CoachState::Running { current_slice: 0 };

        match writer_result {
            Ok(writer) => {
                write.record_file_path = Some(writer.path.clone());
                write.record_writer    = Some(writer);
            }
            Err(e) => {
                warn!(error = %e, "failed to open NDJSON record file — results will not be saved");
            }
        }

        // Broadcast the real agent count to all connected agents so they
        // compute their per-agent connection and bandwidth shares correctly.
        // This MUST be sent before the first SliceTick (which fires as soon
        // as the clock task is spawned below), so we do it here under the
        // same write lock — the SliceTick cannot enter the channel until
        // the clock task is spawned after this block.
        let total_agents = write.registry.len() as u32;
        let agent_count_cmd = CoachCommand {
            payload: Some(coach_command::Payload::PlanUpdate(LoadPlanUpdate {
                // u32::MAX as the boundary keeps all existing slices intact
                // (retain keeps every slice whose index < u32::MAX = all of them).
                // We only want to push the real total_agents value; slices and
                // bandwidth are unchanged.
                effective_from_slice:  u32::MAX,
                updated_slices:        vec![],
                new_bandwidth_bps:     None,
                new_file_distribution: None,
                new_total_agents:      Some(total_agents),
            })),
        };
        let n = write.registry.broadcast(agent_count_cmd);
        info!(total_agents, agents_notified = n, "agent count broadcast");
    }

    // Spawn the slice clock as a background task.
    let clock = SliceClock::new(state.clone(), ack_notify, stop_rx);
    tokio::spawn(clock.run());

    info!("test started — slice clock spawned");

    let agents = state.read().await.registry.connected_count();
    (StatusCode::OK, Json(json!({ "status": "started", "agents": agents }))).into_response()
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
        )
            .into_response();
    }

    read.signal_stop();
    info!("stop signal sent to slice clock");
    (StatusCode::OK, Json(json!({ "status": "stopping" }))).into_response()
}

/// `POST /reset` — transition DONE → WAITING so agents can re-register.
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
        )
            .into_response();
    }

    // Before clearing state, tell any agents still connected (e.g. draining
    // gracefully after the test) to abort immediately.  This uses try_send
    // so it never blocks under the write lock; the message is queued in each
    // agent's channel buffer while the cmd_tx handles still exist.
    // write.reset() then drops the registry (and those handles) right after,
    // and the coach transitions to WAITING — so agents can re-register the
    // moment they finish processing the abort command.
    let abort_cmd = CoachCommand {
        payload: Some(coach_command::Payload::Shutdown(ShutdownCmd {
            graceful: false,
            reason:   "coach reset — abort and re-register".to_string(),
        })),
    };
    let n = write.registry.broadcast(abort_cmd);
    if n > 0 {
        info!(agents = n, "abort ShutdownCmd broadcast to draining agents on reset");
    }

    write.reset();
    info!("aerocoach reset — back to WAITING, agents may re-register");
    (StatusCode::OK, Json(json!({ "status": "waiting" }))).into_response()
}

// ── Bandwidth hot-update ──────────────────────────────────────────────────

/// Request body for `POST /bandwidth`.
#[derive(Debug, Deserialize)]
struct BandwidthRequest {
    total_bandwidth_bps: u64,
}

/// `POST /bandwidth` — update the total bandwidth ceiling at runtime.
///
/// Updates the plan in-memory and, if the test is RUNNING, broadcasts a
/// `LoadPlanUpdate` to all agents so they re-derive their per-agent limits.
async fn bandwidth_handler(
    State(state): State<SharedState>,
    Json(body): Json<BandwidthRequest>,
) -> impl IntoResponse {
    if body.total_bandwidth_bps == 0 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "total_bandwidth_bps must be greater than zero" })),
        )
            .into_response();
    }

    let mut write = state.write().await;

    if write.coach_state.is_done() {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "cannot update bandwidth in DONE state" })),
        )
            .into_response();
    }

    let Some(ref mut plan) = write.load_plan else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no plan is loaded" })),
        )
            .into_response();
    };

    plan.total_bandwidth_bps = body.total_bandwidth_bps;
    info!(bw_bps = body.total_bandwidth_bps, "bandwidth updated");

    // If RUNNING, broadcast to agents.
    if write.coach_state.is_running() {
        let current_slice = write.coach_state.current_slice().unwrap_or(0);
        let cmd = CoachCommand {
            payload: Some(coach_command::Payload::PlanUpdate(LoadPlanUpdate {
                effective_from_slice:  current_slice,
                updated_slices:        vec![],
                new_bandwidth_bps:     Some(body.total_bandwidth_bps),
                new_file_distribution: None,
                new_total_agents:      None,
            })),
        };
        let n = write.registry.broadcast(cmd);
        info!(agents = n, "bandwidth LoadPlanUpdate broadcast");
    }

    (
        StatusCode::OK,
        Json(json!({ "status": "ok", "total_bandwidth_bps": body.total_bandwidth_bps })),
    )
        .into_response()
}

// ── Results download ──────────────────────────────────────────────────────

/// `GET /results` — stream the NDJSON record file as a download.
async fn results_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let record_file_path = state.read().await.record_file_path.clone();

    let Some(path) = record_file_path else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no results available; run a test first" })),
        )
            .into_response();
    };

    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("results.ndjson")
                .to_owned();
            axum::response::Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/x-ndjson")
                .header(
                    "Content-Disposition",
                    format!("attachment; filename=\"{filename}\""),
                )
                .body(axum::body::Body::from(bytes))
                .unwrap()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
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
