//! Bidirectional gRPC session with aerocoach.
//!
//! [`run`] opens the `Session` stream, sends an initial identification
//! report, then drives the slice execution loop until aerocoach sends a
//! [`ShutdownCmd`] or the stream closes.
//!
//! # Slice loop
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │  tokio::select!                                              │
//! │   ├─ inbound.next()      → SliceTick / PlanUpdate / Shutdown│
//! │   └─ active_tasks.join_next() → completed TransferOutcome   │
//! └──────────────────────────────────────────────────────────────┘
//!
//! On SliceTick:
//!   1. Send SliceAck immediately
//!   2. Flush accumulated metrics (previous slice completions)
//!   3. Ramp up: spawn (target - running) new transfer tasks
//!   (Ramp down: do nothing - running tasks complete naturally)
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tokio_stream::{wrappers::ReceiverStream, StreamExt};
use tonic::transport::Channel;
use tracing::{info, warn};

use aeroproto::aeromonitor::{
    agent_report, coach_command, AgentReport, MetricsUpdate, SliceAck,
    agent_service_client::AgentServiceClient,
};

use super::{
    config::Config,
    load_plan::{make_transfer_filename, AgentPlan},
    metrics::MetricsAccumulator,
    rate_limit::RateLimiterConfig,
    transfer::{run_transfer, TransferOutcome},
};

/// Report channel buffer: large enough to avoid back-pressure during
/// burst acking after many slice transitions.
const REPORT_CHANNEL_CAP: usize = 128;

/// How often the session loop sends a heartbeat [`MetricsUpdate`] to the
/// coach while transfers are in-flight.  Matches the coach's dashboard
/// broadcast interval so each tick produces a fresh data point.
const METRICS_HEARTBEAT: Duration = Duration::from_secs(3);

// ── InFlightInfo ─────────────────────────────────────────────────────────────

/// Metadata retained for each transfer task while it is still running.
/// Used to (a) compute the `bytes_in_flight` total for heartbeat reports
/// and (b) synthesise an error [`TransferOutcome`] for any task that is
/// aborted by the graceful-drain timeout at shutdown.
struct InFlightInfo {
    filename:        String,
    bucket_id:       String,
    time_slice:      u32,
    start_ms:        i64,
    file_size_bytes: u64,
    /// Atomically updated by the transfer task as chunks leave the buffer.
    bytes_sent:      Arc<AtomicU64>,
}

/// Open the `Session` stream and run the slice loop until the test ends.
pub async fn run(
    channel: Channel,
    config: &Config,
    plan: &mut AgentPlan,
    bucket_files: &HashMap<String, PathBuf>,
) -> Result<()> {
    // ── Open bidirectional stream ─────────────────────────────────────────
    //
    // IMPORTANT: the aerocoach session() handler reads the *first* inbound
    // message before it returns its Response (to identify which agent this
    // session belongs to).  That means it will not send back HTTP/2 response
    // headers until it has received at least one DATA frame from us.
    //
    // If we were to send the initial report *after* client.session().await,
    // we'd deadlock: the client waits for response headers, the server waits
    // for the first message, and neither side can proceed.
    //
    // Fix: pre-buffer the identification report in the mpsc channel *before*
    // calling client.session().  The H2 transport will deliver it as a DATA
    // frame concurrently with waiting for the response headers, breaking the
    // cycle.
    let (report_tx, report_rx) = mpsc::channel::<AgentReport>(REPORT_CHANNEL_CAP);

    report_tx
        .send(make_metrics_report(
            &config.agent_id,
            MetricsUpdate {
                current_slice: 0,
                active_connections: 0,
                queued_connections: 0,
                completed_transfers: vec![],
                bytes_in_flight: 0,
            },
        ))
        .await
        .context("failed to pre-buffer initial identification report")?;

    let mut client = AgentServiceClient::new(channel);

    let response = client
        .session(ReceiverStream::new(report_rx))
        .await
        .context("Session RPC failed to open")?;
    let mut inbound = response.into_inner();

    info!(agent_id = %config.agent_id, "session open - waiting for first SliceTick");

    // ── Slice execution loop ──────────────────────────────────────────────
    let mut active_tasks: JoinSet<TransferOutcome> = JoinSet::new();
    let mut metrics     = MetricsAccumulator::new();
    let mut conn_id: u64 = 0;
    // conn_id → per-transfer in-flight state (byte counter + metadata).
    let mut in_flight: HashMap<u64, InFlightInfo> = HashMap::new();
    // conn_id → bytes/sec allocated to each still-running transfer.
    // Used to calculate available bandwidth when ramping up.
    let mut running_rates: HashMap<u64, u64> = HashMap::new();
    // Current slice index, updated on every SliceTick for use in heartbeats.
    let mut current_slice: u32 = 0;

    // Periodic heartbeat: sends a MetricsUpdate even when no transfer
    // completes, so aerocoach always has fresh active_connections and
    // bytes_in_flight data.
    let mut metrics_tick = tokio::time::interval(METRICS_HEARTBEAT);
    metrics_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Consume the first (immediate) tick so we don't flood the coach with
    // an empty update before the first SliceTick arrives.
    metrics_tick.tick().await;

    loop {
        tokio::select! {
            biased; // prioritise commands over completed tasks

            // ── Inbound command from aerocoach ─────────────────────────
            cmd_opt = inbound.next() => {
                match cmd_opt {
                    None => {
                        info!("aerocoach closed the session stream - exiting");
                        break;
                    }
                    Some(Err(e)) => {
                        return Err(e).context("session stream error");
                    }
                    Some(Ok(cmd)) => {
                        match cmd.payload {

                            Some(coach_command::Payload::SliceTick(tick)) => {
                                current_slice = tick.slice_index;
                                handle_slice_tick(
                                    tick.slice_index,
                                    config,
                                    plan,
                                    bucket_files,
                                    &mut active_tasks,
                                    &mut metrics,
                                    &mut conn_id,
                                    &report_tx,
                                    &mut running_rates,
                                    &mut in_flight,
                                )
                                .await?;
                            }

                            Some(coach_command::Payload::PlanUpdate(update)) => {
                                info!(
                                    from_slice = update.effective_from_slice,
                                    "plan update received"
                                );
                                plan.apply_update(update);
                            }

                            Some(coach_command::Payload::Shutdown(cmd)) => {
                                info!(
                                    graceful = cmd.graceful,
                                    reason   = %cmd.reason,
                                    "shutdown command received"
                                );
                                // Consume report_tx so shutdown() can drop it
                                // explicitly to flush the gRPC transport before
                                // we tear down the connection.
                                return shutdown(
                                    cmd.graceful,
                                    &config.agent_id,
                                    &mut active_tasks,
                                    &mut metrics,
                                    in_flight,
                                    report_tx,
                                )
                                .await;
                            }

                            None => {
                                warn!("received CoachCommand with no payload");
                            }
                        }
                    }
                }
            }

            // ── Completed transfer task ────────────────────────────────
            Some(result) = active_tasks.join_next(), if !active_tasks.is_empty() => {
                match result {
                    Ok(outcome) => {
                        // Release bandwidth allocation and in-flight tracking.
                        running_rates.remove(&outcome.conn_id);
                        in_flight.remove(&outcome.conn_id);
                        metrics.record(outcome);
                    }
                    Err(e) => warn!(error = %e, "transfer task panicked"),
                }
            }

            // ── Periodic heartbeat ─────────────────────────────────────
            _ = metrics_tick.tick() => {
                let bytes_in_flight: u64 = in_flight
                    .values()
                    .map(|i| i.bytes_sent.load(Ordering::Relaxed))
                    .sum();
                let active = active_tasks.len() as u32;
                if let Some(update) = metrics.drain_into_update(
                    current_slice,
                    active,
                    bytes_in_flight,
                    active > 0, // force a send while transfers are running
                ) {
                    if let Err(e) = report_tx
                        .send(make_metrics_report(&config.agent_id, update))
                        .await
                    {
                        warn!(error = %e, "failed to send heartbeat MetricsUpdate");
                    }
                }
            }
        }
    }

    Ok(())
}

// ── Slice tick handler ────────────────────────────────────────────────────

async fn handle_slice_tick(
    slice_index: u32,
    config: &Config,
    plan: &AgentPlan,
    bucket_files: &HashMap<String, PathBuf>,
    active_tasks: &mut JoinSet<TransferOutcome>,
    metrics: &mut MetricsAccumulator,
    conn_id: &mut u64,
    report_tx: &mpsc::Sender<AgentReport>,
    running_rates: &mut HashMap<u64, u64>,
    in_flight: &mut HashMap<u64, InFlightInfo>,
) -> Result<()> {
    let my_target = plan.my_connections_for_slice(slice_index);
    let running = active_tasks.len() as u32;

    info!(
        slice   = slice_index,
        target  = my_target,
        running,
        "slice tick"
    );

    // ── 1. Ack immediately so the clock isn’t stalled ─────────────────────
    report_tx
        .send(make_ack_report(&config.agent_id, slice_index))
        .await
        .context("failed to send SliceAck")?;

    // ── 2. Flush metrics accumulated since the last tick ──────────────────
    let bytes_in_flight: u64 = in_flight
        .values()
        .map(|i| i.bytes_sent.load(Ordering::Relaxed))
        .sum();
    if let Some(update) = metrics.drain_into_update(slice_index, running, bytes_in_flight, false) {
        report_tx
            .send(make_metrics_report(&config.agent_id, update))
            .await
            .context("failed to send MetricsUpdate")?;
    }

    // ── 3. Ramp up new transfers if target > currently running ────────────
    if my_target > running {
        let to_start  = my_target - running;
        let agent_bps = plan.my_bandwidth_bps();

        // Bandwidth already committed to carry-over transfers from previous
        // slices.  Each entry was recorded when the transfer was spawned.
        let carry_over_bps: u64 = running_rates.values().sum();

        // Remaining budget for the new batch.
        let available_bps = agent_bps.saturating_sub(carry_over_bps);

        // Per-transfer rate for the new connections.
        //
        // Ideal = agent_bps / target  (what every transfer would get if all
        //         started together).
        //
        // When carry-overs have exhausted the budget (available == 0) we fall
        // back to the ideal rate.  This causes a temporary over-allocation
        // that resolves naturally as carry-overs complete.
        let ideal_bps = if my_target > 0 { agent_bps / my_target as u64 } else { 0 };
        let per_new_bps = if available_bps > 0 && to_start > 0 {
            available_bps / to_start as u64
        } else {
            ideal_bps
        };
        let rate_cfg = RateLimiterConfig::from_bps(per_new_bps);

        for _ in 0..to_start {
            let Some(bucket) = plan.weighted_random_bucket() else {
                warn!(slice = slice_index, "no bucket available, skipping spawn");
                continue;
            };

            let Some(local_file) = bucket_files.get(&bucket.bucket_id).cloned() else {
                warn!(bucket = %bucket.bucket_id, "no local file for bucket, skipping spawn");
                continue;
            };

            *conn_id += 1;
            let cid      = *conn_id;
            let filename = make_transfer_filename(&config.agent_id, slice_index, cid);

            let ftp_target = config.ftp_target.clone();
            let ftp_user   = config.ftp_user.clone();
            let ftp_pass   = config.ftp_pass.clone();
            let bucket_id  = bucket.bucket_id.clone();

            // Record the allocation and in-flight metadata before spawning
            // so both maps are always consistent with what’s running.
            running_rates.insert(cid, per_new_bps);

            let bytes_sent = Arc::new(AtomicU64::new(0));
            let file_size_bytes = tokio::fs::metadata(&local_file)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            in_flight.insert(cid, InFlightInfo {
                filename:        filename.clone(),
                bucket_id:       bucket_id.clone(),
                time_slice:      slice_index,
                start_ms:        now_ms(),
                file_size_bytes,
                bytes_sent:      bytes_sent.clone(),
            });

            active_tasks.spawn(run_transfer(
                cid,
                filename,
                bucket_id,
                local_file,
                ftp_target,
                ftp_user,
                ftp_pass,
                rate_cfg,
                slice_index,
                bytes_sent,
            ));
        }

        info!(
            slice        = slice_index,
            started      = to_start,
            total        = active_tasks.len(),
            per_new_bps,
            carry_bps    = carry_over_bps,
            available_bps,
            "ramped up transfers"
        );
    }
    // Ramp DOWN: do nothing — running transfers complete naturally.

    Ok(())
}

// ── Graceful / immediate shutdown ─────────────────────────────────────────

/// Maximum time to wait for in-flight FTP transfers to drain after a
/// graceful `ShutdownCmd`.  Transfers still running at the deadline are
/// aborted so the agent can re-register promptly for the next test run.
///
/// 60 s covers even the largest files in normal network conditions.  If
/// the FTP server is unreachable or a connection stalls permanently the
/// agent still recovers within this window instead of hanging forever.
const GRACEFUL_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

async fn shutdown(
    graceful: bool,
    agent_id: &str,
    active_tasks: &mut JoinSet<TransferOutcome>,
    metrics: &mut MetricsAccumulator,
    // Taken by value so we can record aborted-transfer outcomes and then
    // drop it explicitly to close the gRPC stream cleanly.
    in_flight: HashMap<u64, InFlightInfo>,
    report_tx: mpsc::Sender<AgentReport>,
) -> Result<()> {
    if graceful {
        let n = active_tasks.len();
        info!(tasks = n, "waiting for in-flight transfers to finish");

        let deadline = tokio::time::sleep(GRACEFUL_DRAIN_TIMEOUT);
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                biased;

                // Drain timeout - abort whatever is left and move on.
                _ = &mut deadline => {
                    let remaining = active_tasks.len();
                    if remaining > 0 {
                        warn!(
                            tasks = remaining,
                            timeout_secs = GRACEFUL_DRAIN_TIMEOUT.as_secs(),
                            "graceful drain timed out - aborting remaining transfers"
                        );
                        active_tasks.shutdown().await;
                    }
                    break;
                }

                // Normal completion path: record outcome, remove from in_flight.
                result = active_tasks.join_next() => {
                    match result {
                        Some(Ok(outcome))  => {
                            // in_flight is owned here so we can’t remove; aborted
                            // entries are handled below after the loop.
                            metrics.record(outcome);
                        }
                        Some(Err(e))       => warn!(error = %e, "transfer task panicked"),
                        None               => break, // all tasks finished cleanly
                    }
                }
            }
        }
    } else {
        active_tasks.shutdown().await;
    }

    // Any entries remaining in in_flight were aborted (either by the graceful
    // drain timeout or the non-graceful shutdown path).  Synthesize an error
    // record for each one so the final report always accounts for them.
    let abort_end_ms = now_ms();
    for (conn_id, info) in in_flight {
        let bytes = info.bytes_sent.load(Ordering::Relaxed);
        warn!(
            conn_id,
            filename = %info.filename,
            bytes_sent = bytes,
            "transfer aborted — synthesising error record"
        );
        metrics.record(TransferOutcome {
            conn_id,
            filename:        info.filename,
            bucket_id:       info.bucket_id,
            bytes_transferred: bytes,
            file_size_bytes: info.file_size_bytes,
            bandwidth_kibps: 0,
            success:         false,
            error_reason:    Some("transfer aborted: test ended before completion".into()),
            start_time_ms:   info.start_ms,
            end_time_ms:     abort_end_ms,
            time_slice:      info.time_slice,
        });
    }

    // Final metrics flush (force=true so we always send at least one update).
    if let Some(update) = metrics.drain_into_update(0, 0, 0, true) {
        let _ = report_tx
            .send(make_metrics_report(agent_id, update))
            .await;
    }

    // Explicitly drop the sender.  This closes the mpsc channel, which
    // causes ReceiverStream to signal end-of-stream.  Yielding twice
    // gives the tokio runtime a chance to poll the stream and forward
    // the final message through the gRPC transport before we return.
    drop(report_tx);
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    info!("shutdown complete");
    Ok(())
}

// ── Message builders ──────────────────────────────────────────────────────

fn make_ack_report(agent_id: &str, slice_index: u32) -> AgentReport {
    AgentReport {
        agent_id: agent_id.to_string(),
        timestamp_ms: now_ms(),
        payload: Some(agent_report::Payload::SliceAck(SliceAck { slice_index })),
    }
}

fn make_metrics_report(agent_id: &str, update: MetricsUpdate) -> AgentReport {
    AgentReport {
        agent_id: agent_id.to_string(),
        timestamp_ms: now_ms(),
        payload: Some(agent_report::Payload::MetricsUpdate(update)),
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::net::TcpListener;
    use tokio::sync::{mpsc, Notify};
    use tokio_stream::{wrappers::ReceiverStream, StreamExt};
    use tokio_util::sync::CancellationToken;
    use tonic::{Request, Response, Status, Streaming};

    use aeroproto::aeromonitor::{
        agent_report, coach_command,
        agent_service_server::{AgentService, AgentServiceServer},
        AgentReport, CoachCommand, FileSizeDistribution, FileSizeBucket,
        LoadPlan, RegisterRequest, RegisterResponse, ShutdownCmd, SliceTick, TimeSlice,
    };

    use super::super::{config::Config, file_manager, load_plan::AgentPlan, registration};
    use super::run as session_run;

    // ── Shared mock aerocoach ─────────────────────────────────────────────

    /// Configures the mock coach's scripted behaviour.
    struct MockCoach {
        plan:             LoadPlan,
        /// Connections expected in slice 0 (mock waits for all to complete).
        expect_transfers: usize,
        /// Notified when the mock has received everything it expected.
        done:             Arc<Notify>,
        /// Counts successfully-completed transfers seen by the mock.
        success_count:    Arc<AtomicUsize>,
    }

    type MockStream = ReceiverStream<Result<CoachCommand, Status>>;

    #[tonic::async_trait]
    impl AgentService for MockCoach {
        async fn register(
            &self,
            _req: Request<RegisterRequest>,
        ) -> Result<Response<RegisterResponse>, Status> {
            Ok(Response::new(RegisterResponse {
                accepted:      true,
                reject_reason: String::new(),
                agent_index:   0,
                load_plan:     Some(self.plan.clone()),
            }))
        }

        type SessionStream = MockStream;

        async fn session(
            &self,
            request: Request<Streaming<AgentReport>>,
        ) -> Result<Response<Self::SessionStream>, Status> {
            let (cmd_tx, cmd_rx) = mpsc::channel::<Result<CoachCommand, Status>>(32);
            let mut inbound     = request.into_inner();
            let done            = self.done.clone();
            let success_count   = self.success_count.clone();
            let _expect         = self.expect_transfers; // kept for struct compat

            tokio::spawn(async move {
                // 1. Discard initial identification report.
                let _ = inbound.next().await;

                // 2. Send SliceTick(0).
                let _ = cmd_tx.send(Ok(CoachCommand {
                    payload: Some(coach_command::Payload::SliceTick(SliceTick {
                        slice_index: 0, wall_clock_ms: 0,
                    })),
                })).await;

                // 3. Wait for SliceAck(0).
                // NOTE: we do NOT wait for MetricsUpdates here.  The agent only
                // sends MetricsUpdate when it receives the *next* SliceTick or
                // ShutdownCmd.  Waiting here would cause a deadlock.
                loop {
                    match inbound.next().await {
                        Some(Ok(report)) => {
                            if let Some(agent_report::Payload::SliceAck(ack)) = &report.payload {
                                if ack.slice_index == 0 { break; }
                            }
                            // Any other payload (rare) - keep waiting.
                        }
                        _ => { done.notify_one(); return; }
                    }
                }

                // 4. Send ShutdownCmd immediately after SliceAck.
                // The agent will gracefully drain all in-flight transfer tasks,
                // then send a final MetricsUpdate containing all completions.
                let _ = cmd_tx.send(Ok(CoachCommand {
                    payload: Some(coach_command::Payload::Shutdown(ShutdownCmd {
                        graceful: true,
                        reason:   "mock test complete".into(),
                    })),
                })).await;

                // 5. Collect the final MetricsUpdate (sent during graceful shutdown).
                let mut seen_transfers = 0usize;
                while let Some(msg) = inbound.next().await {
                    if let Ok(report) = msg {
                        if let Some(agent_report::Payload::MetricsUpdate(mu)) = &report.payload {
                            for t in &mu.completed_transfers {
                                if t.success { seen_transfers += 1; }
                            }
                        }
                    }
                }
                success_count.store(seen_transfers, Ordering::Relaxed);
                done.notify_one();
            });

            Ok(Response::new(ReceiverStream::new(cmd_rx)))
        }
    }

    // ── Helper: start mock server on a random port ────────────────────────

    struct MockServer {
        port:     u16,
        shutdown: CancellationToken,
    }

    async fn start_mock(coach: MockCoach) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port     = listener.local_addr().unwrap().port();
        let shutdown = CancellationToken::new();
        let srv_shutdown = shutdown.clone();

        tokio::spawn(async move {
            let shutdown_stream = srv_shutdown.clone();
            let incoming = async_stream::stream! {
                loop {
                    tokio::select! {
                        _ = shutdown_stream.cancelled() => break,
                        r = listener.accept() => match r {
                            Ok((s, _)) => yield Ok::<_, std::io::Error>(s),
                            Err(e)     => yield Err(e),
                        }
                    }
                }
            };
            let _ = tonic::transport::Server::builder()
                .add_service(AgentServiceServer::new(coach))
                .serve_with_incoming_shutdown(incoming, srv_shutdown.cancelled())
                .await;
        });

        // Wait until the port is reachable.
        for _ in 0..20 {
            if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        MockServer { port, shutdown }
    }

    // ── Helper: build agent config ────────────────────────────────────────

    fn agent_config(port: u16, ftp_user: &str, ftp_pass: &str) -> Config {
        let port_s    = port.to_string();
        let user_s    = ftp_user.to_string();
        let pass_s    = ftp_pass.to_string();
        Config::from_source(move |name| match name {
            "AEROGYM_AGENT_ID"  => Some("a00".into()),
            "AEROCOACH_URL"     => Some(format!("http://127.0.0.1:{}", port_s)),
            "AEROSTRESS_TARGET" => Some("127.0.0.1:21".into()),
            "AEROSTRESS_USER"   => Some(user_s.clone()),
            "AEROSTRESS_PASS"   => Some(pass_s.clone()),
            _ => None,
        }).unwrap()
    }

    // ── Helper: run the agent through registration + session ──────────────

    async fn run_agent(
        port:     u16,
        _plan:    &LoadPlan,
        ftp_user: &str,
        ftp_pass: &str,
        ftp_port: u16,
    ) {
        let mut cfg = agent_config(port, ftp_user, ftp_pass);
        cfg.ftp_target = format!("127.0.0.1:{ftp_port}");

        let reg = registration::register(&cfg).await.expect("register failed");
        let mut plan_local = AgentPlan::new(reg.load_plan, reg.agent_index);

        let work_dir = std::env::temp_dir()
            .join(format!("aerogym_test_{:016x}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&work_dir).await.unwrap();

        let bucket_files =
            file_manager::generate(&work_dir, &cfg.agent_id, plan_local.buckets())
            .await.expect("file gen failed");

        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            session_run(reg.channel, &cfg, &mut plan_local, &bucket_files),
        )
        .await
        .expect("session timed out")
        .expect("session returned an error");

        let _ = tokio::fs::remove_dir_all(&work_dir).await;
    }

    // ── Plan builders ─────────────────────────────────────────────────────

    fn plan_no_transfers() -> LoadPlan {
        LoadPlan {
            plan_id: "mock-no-ftp".into(),
            start_time_ms: 0,
            slice_duration_ms: 1_000,
            total_agents: 1,
            total_bandwidth_bps: 0,
            slices: vec![TimeSlice { slice_index: 0, total_connections: 0 }],
            file_distribution: Some(FileSizeDistribution {
                buckets: vec![FileSizeBucket {
                    bucket_id: "xs".into(),
                    size_min_bytes: 1, size_max_bytes: 2, percentage: 1.0,
                }],
            }),
        }
    }

    fn plan_with_transfers(connections: u32) -> LoadPlan {
        LoadPlan {
            plan_id: "mock-with-ftp".into(),
            start_time_ms: 0,
            slice_duration_ms: 1_000,
            total_agents: 1,
            total_bandwidth_bps: 0,   // unlimited
            slices: vec![TimeSlice { slice_index: 0, total_connections: connections }],
            file_distribution: Some(FileSizeDistribution {
                buckets: vec![FileSizeBucket {
                    // Tiny files so the upload completes fast even with pyftpdlib.
                    bucket_id: "xs".into(),
                    size_min_bytes: 512, size_max_bytes: 1024, percentage: 1.0,
                }],
            }),
        }
    }

    // ═════════════════════════════════════════════════════════════════════
    // Test 1: full gRPC conversation, no FTP transfers needed
    // ═════════════════════════════════════════════════════════════════════

    /// Register → Session → SliceTick(0) → SliceAck(0) → ShutdownCmd →
    /// final MetricsUpdate → agent exits cleanly.
    ///
    /// Uses `total_connections = 0` so no FTP server is required.  The test
    /// verifies the entire gRPC slice-cycle protocol without any file I/O
    /// on the FTP side.
    #[tokio::test]
    async fn agent_completes_session_with_mock_coach() {
        let done   = Arc::new(Notify::new());
        let mock   = MockCoach {
            plan:             plan_no_transfers(),
            expect_transfers: 0,
            done:             done.clone(),
            success_count:    Arc::new(AtomicUsize::new(0)),
        };

        let server = start_mock(mock).await;

        run_agent(server.port, &plan_no_transfers(), "anon", "", 21).await;

        // The mock notifies once it has received the final MetricsUpdate.
        tokio::time::timeout(std::time::Duration::from_secs(5), done.notified())
            .await
            .expect("mock did not receive final MetricsUpdate within 5 s");

        server.shutdown.cancel();
    }

    // ═════════════════════════════════════════════════════════════════════
    // Test 2: full end-to-end with real FTP uploads (pyftpdlib)
    //
    // Requires:  python3 -m pyftpdlib -p 2121 -w
    // Run with:  cargo test -p aerogym agent_transfers -- --nocapture
    // ═════════════════════════════════════════════════════════════════════

    /// Complete slice cycle with real FTP uploads to a local pyftpdlib server.
    ///
    /// Spawns 2 transfer tasks in slice 0, waits for both to succeed,
    /// then gracefully shuts down.  Verifies that the mock coach sees
    /// exactly 2 successful `TransferRecord`s.
    #[tokio::test]
    async fn agent_transfers_files_via_ftp() {
        const FTP_PORT:   u16 = 2121;
        const TRANSFERS:  u32 = 2;

        // Skip gracefully if the FTP server is not reachable.
        if std::net::TcpStream::connect(format!("127.0.0.1:{FTP_PORT}")).is_err() {
            eprintln!("SKIP: pyftpdlib not running on port {FTP_PORT}");
            return;
        }

        let done          = Arc::new(Notify::new());
        let success_count = Arc::new(AtomicUsize::new(0));
        let mock = MockCoach {
            plan:             plan_with_transfers(TRANSFERS),
            expect_transfers: TRANSFERS as usize,
            done:             done.clone(),
            success_count:    success_count.clone(),
        };

        let server = start_mock(mock).await;

        run_agent(
            server.port,
            &plan_with_transfers(TRANSFERS),
            "test", "secret",
            FTP_PORT,
        ).await;

        tokio::time::timeout(std::time::Duration::from_secs(15), done.notified())
            .await
            .expect("mock did not see transfer completions within 15 s");

        assert_eq!(
            success_count.load(Ordering::Relaxed),
            TRANSFERS as usize,
            "expected {TRANSFERS} successful transfers, got {}",
            success_count.load(Ordering::Relaxed)
        );

        server.shutdown.cancel();
    }
}
