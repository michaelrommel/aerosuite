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
use aeroproto::aeromonitor::{coach_command, CoachCommand, LoadPlanUpdate, ShutdownCmd, TimeSlice};

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
    // Store the record directory and plan directory so HTTP handlers can use them.
    {
        let mut w = shared.write().await;
        w.record_dir = config.record_dir.clone();
        w.plan_dir   = config.plan_dir.clone();
    }

    // ── Load plan (optional) ──────────────────────────────────────────────
    //
    // AEROCOACH_PLAN_DIR takes precedence: load the alphabetically-last .json
    // file from the directory.  Falls back to AEROCOACH_PLAN_FILE when the
    // directory variable is absent.
    let startup_plan_path: Option<std::path::PathBuf> = if let Some(ref dir) = config.plan_dir {
        match last_plan_in_dir(dir) {
            Ok(Some(path)) => {
                info!(path = %path.display(), "plan directory: loading last alphabetical plan");
                Some(path)
            }
            Ok(None) => {
                warn!(dir = %dir.display(), "plan directory is empty — no plan loaded");
                None
            }
            Err(e) => {
                warn!(dir = %dir.display(), error = %e, "cannot read plan directory");
                None
            }
        }
    } else {
        config.plan_file.clone()
    };

    if let Some(ref path) = startup_plan_path {
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
                let stem = stem_from_path(path, &plan.plan_id);
                let mut w = shared.write().await;
                w.plan_filename_stem = Some(stem);
                w.load_plan          = Some(plan);
            }
            Err(e) => {
                warn!(
                    path  = %path.display(),
                    error = %e,
                    "could not load plan file — continuing without a plan"
                );
            }
        }
    } else if config.plan_dir.is_none() {
        info!("no AEROCOACH_PLAN_FILE or AEROCOACH_PLAN_DIR set — supply a plan via PUT /plan");
    }

    // ── Delta ticker (broadcasts DashboardUpdate every 3 s) ───────────────
    {
        let ticker_state = shared.clone();
        tokio::spawn(async move {
            let mut engine = DeltaEngine::new();
            let mut ticker = tokio::time::interval(Duration::from_secs(3));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

            // Deadline after which the NDJSON writer is force-closed even if
            // some agents have not yet reported active_connections == 0.
            // Set the first time we observe `is_done`; cleared on reset.
            // The generous 120 s window covers the full 60 s graceful-drain
            // timeout on the agent side, plus network RTT and a safety margin.
            let mut writer_close_deadline: Option<std::time::Instant> = None;

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

                    // ── NDJSON writer ──────────────────────────────────────────────────
                    let is_done = write.coach_state.is_done();

                    // Maintain the writer-close deadline.
                    //   • Set it the first time we observe `is_done` so we have
                    //     a hard upper bound even if an agent crashes mid-drain.
                    //   • Clear it whenever we are not Done (coach was reset)
                    //     so the next test run starts with a fresh deadline.
                    if is_done && writer_close_deadline.is_none() {
                        writer_close_deadline = Some(
                            std::time::Instant::now()
                                + std::time::Duration::from_secs(120),
                        );
                    } else if !is_done {
                        writer_close_deadline = None;
                    }

                    // Close the writer once all agents have finished draining
                    // (every agent reports active_connections == 0 in its final
                    // MetricsUpdate) or the hard deadline has been exceeded.
                    //
                    // By waiting for agents to self-report, the NDJSON file
                    // receives the rich in-flight-at-test-end records that
                    // aerogym sends immediately on receiving ShutdownCmd
                    // (actual filename, bytes so far, file size, start time,
                    // time-slice, etc.) instead of coarse fabricated
                    // placeholders based on registry connection counters.
                    let agents_still_draining =
                        snapshot.iter().any(|a| a.active_connections > 0);
                    let deadline_exceeded = writer_close_deadline
                        .map(|d| std::time::Instant::now() >= d)
                        .unwrap_or(false);
                    let should_close_writer =
                        is_done && (!agents_still_draining || deadline_exceeded);

                    if let Some(ref mut writer) = write.record_writer {
                        for (agent_id, record) in &drained {
                            if let Err(e) = writer.append(agent_id, record) {
                                warn!(error = %e, "NDJSON append failed");
                            }
                        }
                        if should_close_writer {
                            if deadline_exceeded && agents_still_draining {
                                let still: Vec<_> = snapshot
                                    .iter()
                                    .filter(|a| a.active_connections > 0)
                                    .map(|a| a.agent_id.as_str())
                                    .collect();
                                warn!(
                                    agents = ?still,
                                    "force-closing NDJSON writer after 120 s timeout; \
                                     some agents may still be draining"
                                );
                            }
                            if let Err(e) = writer.flush() {
                                warn!(error = %e, "NDJSON flush failed");
                            }
                        }
                    }
                    // Only drop the writer once — a no-op on every subsequent tick.
                    if should_close_writer && write.record_writer.take().is_some() {
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
        .route("/plan",        get(plan_get_handler).put(plan_put_handler).patch(plan_patch_handler))
        .route("/plans",       get(plans_list_handler))
        .route("/plan/select",  post(plan_select_handler))
        .route("/plan/confirm", post(plan_confirm_handler))
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
    write.load_plan          = Some(body);
    write.plan_filename_stem = None; // no backing file — fall back to sanitised plan_id at start
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
    let (ack_notify, record_prefix, record_dir) = {
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

        let record_prefix = read
            .plan_filename_stem
            .clone()
            .unwrap_or_else(|| {
                let id = read.load_plan.as_ref().map(|p| p.plan_id.as_str()).unwrap_or("plan");
                safe_record_prefix(id)
            });
        let record_dir = read.record_dir.clone();
        (read.ack_notify.clone(), record_prefix, record_dir)
    };

    // Open the NDJSON record writer (synchronous I/O — fast mkdir + open).
    let writer_result = NdjsonWriter::open(&record_dir, &record_prefix);

    // Transition to RUNNING, reset the stop signal, install the writer, and
    // create the stop receiver — all inside the same write lock so that
    // stop_handler cannot signal a stop between the reset and the receiver
    // creation (it must acquire a read lock and see RUNNING first).
    let stop_rx = {
        let mut write = state.write().await;
        write.coach_state = crate::state::CoachState::Running { current_slice: 0 };

        // Replace the stop channel with a fresh one so the clock receiver
        // starts from a channel that has never had `true` sent on it.
        // This is immune to any stale value left in the previous channel.
        let stop_rx = write.renew_stop();

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

        stop_rx
    };

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

// ── Plan confirm ───────────────────────────────────────────────────────────────

/// `POST /plan/confirm` — push the current plan to all connected agents so
/// they re-register, pick up the new plan, and regenerate any bucket files
/// whose size is out of range for the new distribution.
///
/// Only meaningful in WAITING state (agents are idle in their session loops).
/// The coach broadcasts a non-graceful [`ShutdownCmd`] to every connected
/// agent; they abort, close their session, and immediately loop back to
/// `registration::register()` where they receive the updated plan.
/// The coach state itself does not change.
async fn plan_confirm_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let mut write = state.write().await;

    if !write.coach_state.is_waiting() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "cannot confirm plan while in state {}; must be WAITING",
                    write.coach_state
                )
            })),
        )
            .into_response();
    }

    if write.load_plan.is_none() {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({ "error": "no plan loaded; select a plan before confirming" })),
        )
            .into_response();
    }

    let cmd = CoachCommand {
        payload: Some(coach_command::Payload::Shutdown(ShutdownCmd {
            graceful: false,
            reason:   "plan confirmed — re-register to receive the updated plan".to_string(),
        })),
    };
    let n = write.registry.broadcast(cmd);
    info!(
        agents = n,
        plan_id = %write.load_plan.as_ref().map(|p| p.plan_id.as_str()).unwrap_or(""),
        "plan confirmed — ShutdownCmd broadcast to agents"
    );

    (StatusCode::OK, Json(json!({ "status": "ok", "agents_notified": n }))).into_response()
}

// ── Plan directory helpers ──────────────────────────────────────────────────

/// Entry returned by `GET /plans`.
#[derive(serde::Serialize)]
struct PlanEntry {
    filename: String,
    plan_id:  String,
}

/// Convert a plan file stem (or plan_id fallback) into a string that is safe
/// to use as a filesystem filename component on all common OS/filesystem
/// combinations.
///
/// Rules: keep ASCII letters, digits, `-`, and `_`; replace everything else
/// (spaces, colons, slashes, dots …) with `_`; collapse consecutive
/// underscores; strip leading/trailing underscores.
fn safe_record_prefix(s: &str) -> String {
    let raw: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    // Collapse runs of underscores and trim them from the edges.
    let mut out = String::with_capacity(raw.len());
    let mut prev_was_underscore = false;
    for c in raw.chars() {
        if c == '_' {
            if !prev_was_underscore && !out.is_empty() {
                out.push(c);
            }
            prev_was_underscore = true;
        } else {
            out.push(c);
            prev_was_underscore = false;
        }
    }
    // Strip a trailing underscore that might remain.
    if out.ends_with('_') { out.pop(); }
    if out.is_empty() { out.push_str("plan"); }
    out
}

/// Extract the filesystem stem from a plan file path and sanitise it for use
/// as a record-file prefix.  Falls back to sanitising `plan_id` if the path
/// has no usable stem.
fn stem_from_path(path: &std::path::Path, plan_id: &str) -> String {
    let raw = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(plan_id);
    safe_record_prefix(raw)
}

/// Return the path of the alphabetically-last `.json` file in `dir`,
/// or `None` if the directory contains no `.json` files.
fn last_plan_in_dir(dir: &std::path::Path) -> anyhow::Result<Option<std::path::PathBuf>> {
    use anyhow::Context as _;
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read plan directory {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    entries.sort();
    Ok(entries.into_iter().last())
}

/// `GET /plans` — list all `.json` files in the plan directory with their
/// human-readable `plan_id`, sorted alphabetically by filename.
async fn plans_list_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let plan_dir = state.read().await.plan_dir.clone();

    let Some(dir) = plan_dir else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no plan directory configured (AEROCOACH_PLAN_DIR not set)" })),
        )
            .into_response();
    };

    let mut entries: Vec<std::path::PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
            .collect(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    entries.sort();

    let plans: Vec<PlanEntry> = entries
        .into_iter()
        .filter_map(|path| {
            let filename = path.file_name()?.to_string_lossy().into_owned();
            // Best-effort: read plan_id from the file; skip unparseable files.
            let plan_id = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .and_then(|v| v["plan_id"].as_str().map(str::to_owned))
                .unwrap_or_else(|| filename.clone());
            Some(PlanEntry { filename, plan_id })
        })
        .collect();

    (StatusCode::OK, Json(json!(plans))).into_response()
}

/// Request body for `POST /plan/select`.
#[derive(Deserialize)]
struct SelectPlanRequest {
    filename: String,
}

/// `POST /plan/select` — load a plan by filename from the plan directory.
///
/// Only accepted in WAITING state.  The filename must name a `.json` file
/// inside the configured plan directory; path traversal is rejected.
async fn plan_select_handler(
    State(state): State<SharedState>,
    Json(body): Json<SelectPlanRequest>,
) -> impl IntoResponse {
    let plan_dir = state.read().await.plan_dir.clone();

    let Some(dir) = plan_dir else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no plan directory configured (AEROCOACH_PLAN_DIR not set)" })),
        )
            .into_response();
    };

    // Guard against path traversal.
    if body.filename.contains('/') || body.filename.contains('\\') || body.filename.contains("..") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "filename must not contain path separators" })),
        )
            .into_response();
    }
    if !body.filename.ends_with(".json") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "filename must end with .json" })),
        )
            .into_response();
    }

    let path = dir.join(&body.filename);

    let mut write = state.write().await;
    if !write.coach_state.is_waiting() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "cannot select plan while in state {}; must be WAITING",
                    write.coach_state
                )
            })),
        )
            .into_response();
    }

    match LoadPlanFile::load(&path) {
        Ok(plan) => {
            let plan_id = plan.plan_id.clone();
            let stem    = safe_record_prefix(
                std::path::Path::new(&body.filename)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&plan_id),
            );
            info!(
                filename      = %body.filename,
                plan_id       = %plan_id,
                record_prefix = %stem,
                "plan selected via POST /plan/select"
            );
            write.load_plan          = Some(plan);
            write.plan_filename_stem = Some(stem);
            (StatusCode::OK, Json(json!({ "status": "ok", "plan_id": plan_id }))).into_response()
        }
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn wait_for_shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigint  = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = sigint.recv()  => {}
        _ = sigterm.recv() => {}
    }
}
