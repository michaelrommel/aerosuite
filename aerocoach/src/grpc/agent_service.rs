//! tonic implementation of [`AgentService`].
//!
//! Handles the two RPCs defined in `aeromonitor.proto`:
//!
//! - [`register`] - unary: validate agent, assign index, return [`LoadPlan`].
//! - [`session`] - bidirectional stream: receive [`AgentReport`]s, send
//!   [`CoachCommand`]s for the lifetime of the test.
//!
//! # Session lifecycle
//!
//! ```text
//! Agent                            aerocoach
//!   │── Register ────────────────────► │  (unary; gets plan + index)
//!   │                                  │
//!   │── Session (stream open) ────────►│  (bidi stream)
//!   │                                  │  spawn recv task
//!   │◄── SliceTick(0) ────────────────│  (from slice clock via mpsc)
//!   │── SliceAck(0) ────────────────► │
//!   │── MetricsUpdate(...) ──────────►│
//!   │◄── SliceTick(1) ────────────────│
//!   │    ...                           │
//!   │── (stream closes) ──────────────│  deregister
//! ```

use std::pin::Pin;

use tokio::sync::mpsc;
use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, info, warn};

use aeroproto::aeromonitor::{
    agent_report, AgentReport, CoachCommand, RegisterRequest, RegisterResponse,
};
use aeroproto::aeromonitor::agent_service_server::AgentService;

use crate::state::SharedState;

// ── Service struct ────────────────────────────────────────────────────────

/// tonic service implementation for the `AgentService` RPC.
#[derive(Debug, Clone)]
pub struct AgentServiceImpl {
    state: SharedState,
}

impl AgentServiceImpl {
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }
}

// ── Tonic trait implementation ────────────────────────────────────────────

/// Output stream type for the `Session` RPC.
type SessionStream = Pin<Box<dyn Stream<Item = Result<CoachCommand, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl AgentService for AgentServiceImpl {
    // ── Register ──────────────────────────────────────────────────────────

    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let req = request.into_inner();
        let agent_id = req.agent_id.clone();

        info!(
            agent_id    = %agent_id,
            version     = %req.agent_version,
            private_ip  = %req.private_ip,
            instance_id = %req.instance_id,
            "Register request received"
        );

        // ── Validate state ─────────────────────────────────────────────
        let mut write = self.state.write().await;

        if !write.coach_state.is_waiting() {
            return Err(Status::failed_precondition(format!(
                "aerocoach is in state {}; registrations are only accepted while WAITING",
                write.coach_state
            )));
        }

        let Some(ref plan) = write.load_plan else {
            return Err(Status::failed_precondition(
                "no load plan is loaded; supply one via AEROCOACH_PLAN_FILE or PUT /plan",
            ));
        };

        // ── Assign agent index ─────────────────────────────────────────
        let total_agents = plan.total_agents_hint();
        let plan_proto = plan.to_proto(total_agents);

        let agent_index = write
            .registry
            .register(agent_id.clone(), req.private_ip, req.instance_id)
            .map_err(|e| Status::already_exists(e))?;

        info!(agent_id = %agent_id, index = agent_index, "agent accepted");

        Ok(Response::new(RegisterResponse {
            accepted: true,
            reject_reason: String::new(),
            agent_index,
            load_plan: Some(plan_proto),
        }))
    }

    // ── Session ───────────────────────────────────────────────────────────

    type SessionStream = SessionStream;

    async fn session(
        &self,
        request: Request<Streaming<AgentReport>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let mut in_stream = request.into_inner();

        // ── Read the first message to identify the agent ───────────────
        let first = in_stream
            .next()
            .await
            .ok_or_else(|| Status::cancelled("stream closed before first message"))?
            .map_err(|e| Status::internal(e.to_string()))?;

        let agent_id = first.agent_id.clone();

        if agent_id.is_empty() {
            return Err(Status::invalid_argument(
                "first AgentReport must contain a non-empty agent_id",
            ));
        }

        // ── Validate the agent is registered ──────────────────────────
        {
            let read = self.state.read().await;
            if !read.registry.contains(&agent_id) {
                return Err(Status::not_found(format!(
                    "agent {:?} has not registered; call Register first",
                    agent_id
                )));
            }
        }

        info!(agent_id = %agent_id, "Session stream opened");

        // ── Create per-agent command channel ──────────────────────────
        let (cmd_tx, cmd_rx) = mpsc::channel::<CoachCommand>(64);

        let session_gen = {
            let mut write = self.state.write().await;
            write.registry.set_session_channel(&agent_id, cmd_tx)
        };

        // ── Process the first message ─────────────────────────────────
        handle_report(&self.state, &first).await;

        // ── Spawn task to handle remaining inbound messages ───────────
        let state = self.state.clone();
        let aid = agent_id.clone();
        tokio::spawn(async move {
            while let Some(result) = in_stream.next().await {
                match result {
                    Ok(report) => handle_report(&state, &report).await,
                    Err(e) => {
                        warn!(agent_id = %aid, error = %e, "session stream error");
                        break;
                    }
                }
            }
            // Close only if this is still the current session - prevents a
            // stale task from clobbering a newer session the agent opened
            // after reconnecting.
            state.write().await.registry.close_session(&aid, session_gen);
            info!(agent_id = %aid, "session receive task finished");
        });

        // ── Return the outbound command stream ────────────────────────
        let out: SessionStream = Box::pin(ReceiverStream::new(cmd_rx).map(Ok));
        Ok(Response::new(out))
    }
}

// ── Report handler ────────────────────────────────────────────────────────

/// Dispatch one [`AgentReport`] to the appropriate state updates.
async fn handle_report(state: &SharedState, report: &AgentReport) {
    let agent_id = &report.agent_id;

    match &report.payload {
        Some(agent_report::Payload::SliceAck(ack)) => {
            // Acquire one write lock to update registry + fire the notify
            // so the slice clock's ack-wait loop wakes up without missing
            // the notification.
            let mut write = state.write().await;
            write.registry.update_slice(agent_id, ack.slice_index);
            write.signal_ack();
            debug!(
                agent_id = %agent_id,
                slice    = ack.slice_index,
                "slice ack"
            );
        }
        Some(agent_report::Payload::MetricsUpdate(update)) => {
            let mut write = state.write().await;

            // Discard metrics that arrive after a coach reset.  Once the coach
            // is back in WAITING state, any incoming MetricsUpdate belongs to
            // the tail of the previous test run (e.g. the agent's final flush
            // after an abort) and must not repopulate the freshly-cleared
            // MetricsStore with stale error counts.
            if write.coach_state.is_waiting() {
                debug!(
                    agent_id = %agent_id,
                    "ignoring post-reset MetricsUpdate (coach is WAITING)"
                );
                return;
            }

            write
                .registry
                .update_active_connections(agent_id, update.active_connections);
            write.metrics.record_update(agent_id.clone(), update);
            debug!(
                agent_id  = %agent_id,
                slice     = update.current_slice,
                active    = update.active_connections,
                completed = update.completed_transfers.len(),
                "metrics update"
            );
        }
        Some(agent_report::Payload::PlanAck(_)) => {
            let mut write = state.write().await;
            write.registry.ack_plan(agent_id);
            let acked = write.registry.plan_ack_count();
            let total = write.registry.len();
            info!(
                agent_id = %agent_id,
                acked,
                total,
                "plan ack received"
            );
        }
        None => {
            warn!(agent_id = %agent_id, "received AgentReport with no payload");
        }
    }
}
