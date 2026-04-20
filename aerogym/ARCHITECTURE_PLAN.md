# Aerostress Monitoring System - Architecture Plan

## Overview

This document outlines the architecture for a real-time monitoring system that visualizes FTP stress testing progress across multiple nodes. The system consists of three main components:

1. **Stress Tester Agent** (existing codebase) - collects per-connection metrics
2. **Aggregator Service** (new Rust application) - receives gRPC, computes deltas, broadcasts via WebSocket
3. **Frontend Dashboard** (new Svelte app) - WebGL canvas visualization with real-time updates

## System Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    FRONTEND DASHBOARD                           │
│              Svelte + WebGL Canvas (Deck.gl/Three.js)           │
│                  WebSocket Client (updates every 3s)            │
└────────────────────────────▲────────────────────────────────────┘
                             │ WebSocket JSON
                             ▼
┌─────────────────────────────────────────────────────────────────┐
│                    AGGREGATOR SERVICE                           │
│              Rust + axum/Actix-web + gRPC + WebSockets          │
│         - Receives gRPC from all agents                         │
│         - Computes delta updates                                │
│         - Broadcasts to WebSocket clients                       │
└────────────────────────────▲────────────────────────────────────┘
                             │ gRPC streaming (protobuf)
                             ▼
┌───────────┬───────────┬───────────┬───────────┬───────────┐
│ Agent 1   │ Agent 2   │ ...       │ Agent N-1 │ Agent N   │
│ 500 conns │ 750 conns │           │ 300 conns │ 900 conns │
│ (Node A)  │ (Node B)  │           │ (Node C)  │ (Node D)  │
└───────────┴───────────┴───────────┴───────────┴───────────┘
                             │ FTP Protocol
                             ▼
                    ┌─────────────────────┐
                    │   AEROFTP SERVER    │
                    │      (21 port)      │
                    └─────────────────────┘
```

---

## Component 1: Stress Tester Agent (Existing Codebase)

### Goal
Modify the existing `aerostress` binary to collect and report per-connection metrics via gRPC streaming.

### Changes Required

#### A. Add Node Identification
**File:** `src/config.rs` or new file `src/node_id.rs`

Add configuration for:
```rust
// Environment variable: AEROSTRESS_NODE_ID (optional, auto-generate if not set)
pub node_id: String  // e.g., "node-a", "node-b", or UUID format

// Environment variable: AEROSTRESS_AGGREGATOR_URL (required when reporting enabled)
pub aggregator_url: Option<String>  // e.g., "grpc://aggregator.local:50051"

// Environment variable: AEROSTRESS_REPORT_INTERVAL (optional, default 3 seconds)
pub report_interval_secs: u64  // how often to send metrics snapshot
```

#### B. Connection Tracking Data Structure
**New file:** `src/metrics.rs`

Each connection has a **target byte volume** that it must transfer before terminating. This can be achieved by:
- One large file (e.g., 200MB single transfer)
- Multiple smaller files with same filename repeated (e.g., 20MB × 10 transfers = 200MB total)

```rust
/// Per-file transfer result within a connection
#[derive(Debug, Clone)]
pub struct FileTransferResult {
    pub file_index: u32,           // Which transfer in the sequence (0-indexed)
    pub filename: String,
    pub bytes_attempted: u64,
    pub bytes_successfully_transferred: u64,
    pub success: bool,
    pub error_code: Option<String>,
    pub start_time_ms: i64,
    pub end_time_ms: i64,
}

/// Per-connection metric snapshot
#[derive(Debug, Clone)]
pub struct ConnectionMetric {
    pub connection_id: String,     // Format: "{batch}_{task}" e.g., "01_0042"
    pub batch_number: i32,
    pub task_number: i32,
    
    // Target volume for this connection
    pub target_bytes: u64,         // Total bytes this connection must transfer
    
    // Current progress
    pub transferred_bytes: u64,    // Cumulative bytes successfully transferred
    pub file_count: u32,           // Number of files attempted in this connection
    pub successful_files: u32,     // Files transferred without error
    pub failed_files: u32,         // Files that encountered errors
    
    // Detailed per-file tracking (only sent if significant changes)
    pub file_results: Vec<FileTransferResult>,
    
    // Start/End timing for the entire connection
    pub start_time_ms: Option<i64>,
    pub end_time_ms: Option<i64>,  // When target_bytes reached or connection terminated
}

/// Agent-level metrics snapshot for gRPC transmission
#[derive(Debug, Clone)]
pub struct AgentMetricsSnapshot {
    pub node_id: String,
    pub timestamp_ms: i64,
    pub connections: Vec<ConnectionMetric>,
    
    // Aggregated totals (for quick dashboard overview)
    pub total_bytes: u64,
    pub successful_connections: u32,
    pub failed_connections: u32,
}
```

#### C. Modify Main Execution Flow
**File:** `src/main.rs`

Current flow needs modification:

**Before (current):**
```rust
for j in 1..=config.batches {
    for i in 1..=config.tasks {
        set.spawn(async move { /* FTP transfer */ });
    }
    sleep(Duration::from_secs(config.delay)).await;
}
// Wait for all tasks, collect success/error counts
```

**After (proposed):**
```rust
// 1. Initialize metrics collector
let metrics_collector = MetricsCollector::new(node_id.clone());

// 2. Spawn FTP tasks with callback to record completion
for j in 1..=config.batches {
    for i in 1..=config.tasks {
        let collector_tx = metrics_collector.handle();
        set.spawn(async move { 
            // ... existing FTP logic ...
            // On success/failure:
            collector_tx.record_completion(
                batch: j,
                task: i,
                bytes: bytes_written,
                success: true/false,
                error: optional_error_message
            );
        });
    }
    sleep(Duration::from_secs(config.delay)).await;
}

// 3. After all tasks complete, send final snapshot
let final_snapshot = metrics_collector.take_snapshot();
if let Some(aggregator_url) = config.aggregator_url {
    send_to_aggregator(&aggregator_url, final_snapshot).await?;
}
```

#### D. Metrics Collector Implementation
**New file:** `src/metrics_collector.rs`

Thread-safe collector using `Arc<Mutex<>>`:

```rust
pub struct MetricsCollector {
    node_id: String,
    connections: Arc<Mutex<HashMap<String, ConnectionMetric>>>,
}

impl MetricsCollector {
    pub fn new(node_id: String) -> Self;
    
    pub fn record_completion(
        &self,
        batch: i32,
        task: i32,
        bytes: u64,
        success: bool,
        error: Option<String>,
    );
    
    pub fn take_snapshot(&self) -> AgentMetricsSnapshot;
}
```

#### E. gRPC Client Implementation
**New file:** `src/grpc_client.rs`

Use `tonic` crate for gRPC streaming client:

```rust
use tonic::{Request, Streaming};

pub struct GrpcClient {
    channel: Channel,
    client: stress_monitor::StressMonitorClient<Channel>,
}

impl GrpcClient {
    pub async fn connect(url: &str) -> Result<Self>;
    
    /// Send metrics snapshot via streaming RPC
    pub async fn send_metrics(&mut self, snapshot: AgentMetricsSnapshot) -> Result<()>;
}
```

**Proto definition (see Component 2 for full spec):**
```proto
service StressMonitor {
    rpc ReportMetrics(stream MetricsRequest) returns (MetricsResponse);
}

message MetricsRequest {
    string node_id = 1;
    int64 timestamp_ms = 2;
    repeated ConnectionMetric connections = 3;
    uint64 total_bytes = 4;
    uint32 successful_connections = 5;
    uint32 failed_connections = 6;
}

message MetricsResponse {
    bool ack = 1;
}
```

#### F. Cargo.toml Dependencies
Add to `[dependencies]`:
```toml
tonic = "0.12"
prost = "0.13"
tokio-stream = { version = "0.1", features = ["sync"] }
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1.17", features = ["v4", "serde"] }
```

#### G. Environment Variables Summary
| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `AEROSTRESS_NODE_ID` | No | auto-generated UUID | Unique identifier for this agent node |
| `AEROSTRESS_AGGREGATOR_URL` | Optional | none | gRPC endpoint (e.g., "grpc://localhost:50051") - omit to disable reporting |
| `AEROSTRESS_REPORT_INTERVAL` | No | 3 | Seconds between metric snapshots |

---

## Component 2: Aggregator Service (New Rust Application)

### Goal
Receive gRPC streams from all stress tester nodes, compute delta updates efficiently, and broadcast via WebSocket to frontend dashboard.

### Project Structure
```
aggregator/
├── Cargo.toml
├── src/
│   ├── main.rs          # Entry point, server setup
│   ├── grpc_server.rs   # gRPC service implementation
│   ├── websocket_server.rs  # WebSocket broadcasting
│   ├── state.rs         # Connection state management + delta calculation
│   └── proto/
│       └── stress_monitor.proto  # Protocol buffer definitions
└── proto/
    └── stress_monitor.proto
```

### A. Proto Definition (shared between agent and aggregator)
**File:** `proto/stress_monitor.proto`

```protobuf
syntax = "proto3";

package stressmonitor;

service StressMonitor {
    // Agents stream metrics snapshots to aggregator
    rpc ReportMetrics(stream MetricsRequest) returns (MetricsResponse);
}

message MetricsRequest {
    string node_id = 1;
    int64 timestamp_ms = 2;
    
    repeated ConnectionMetric connections = 3;
    
    // Aggregated totals for quick dashboard overview
    uint64 total_bytes = 4;
    uint32 successful_connections = 5;
    uint32 failed_connections = 6;
}

message MetricsResponse {
    bool ack = 1;
    optional string error = 2;  // If something went wrong
}

message ConnectionMetric {
    string connection_id = 1;      // "{batch}_{task}" format
    int32 batch_number = 2;
    int32 task_number = 3;
    
    // Target volume for this connection
    uint64 target_bytes = 4;
    
    // Current progress
    uint64 transferred_bytes = 5;   // Cumulative bytes successfully transferred
    uint32 file_count = 6;          // Number of files attempted in this connection
    uint32 successful_files = 7;    // Files transferred without error
    uint32 failed_files = 8;        // Files that encountered errors
    
    // Start/End timing for the entire connection
    optional int64 start_time_ms = 9;
    optional int64 end_time_ms = 10;
}

// Per-file transfer tracking (optional, included in full snapshots)
message FileTransferResult {
    uint32 file_index = 1;          // Which transfer in the sequence (0-indexed)
    string filename = 2;
    uint64 bytes_attempted = 3;
    uint64 bytes_successfully_transferred = 4;
    bool success = 5;
    optional string error_code = 6;
    int64 start_time_ms = 7;
    int64 end_time_ms = 8;
}

// WebSocket broadcast message (JSON)
message DashboardUpdate {
    int64 timestamp_ms = 1;
    
    // Per-node aggregates
    repeated NodeSnapshot nodes = 2;
    
    // Optional: full connection details (only if significant changes)
    repeated ConnectionDelta connections_delta = 3;
}

// Color legend for frontend display
message VisualizationLegend {
    string error_rate_range = 1;      // e.g., "0%", ">0-1%", ">1-3%", ">3%"
    string color_hex = 2;             // e.g., "#38CC60"
    bool is_completed_variant = 3;    // true for darker completion colors
}

message NodeSnapshot {
    string node_id = 1;
    uint64 total_bytes = 2;
    uint32 total_connections = 3;
    uint32 successful_connections = 4;
    uint32 failed_connections = 5;
    uint32 active_connections = 6;  // Currently transferring
}

message ConnectionDelta {
    string connection_id = 1;
    optional uint64 target_bytes = 2;      // Total bytes this connection must transfer
    optional uint64 transferred_bytes = 3; // Cumulative bytes successfully transferred
    optional uint32 file_count = 4;        // Number of files attempted
    optional uint32 successful_files = 5;  // Files transferred without error
    optional uint32 failed_files = 6;      // Files that encountered errors
    optional bool success = 7;             // true=completed_success, false=failed
    optional int64 end_time_ms = 8;        // When completion occurred
}
```

### B. Delta Calculation Strategy (Critical Performance Optimization)

**File:** `src/state.rs`

The aggregator must efficiently compute deltas to minimize WebSocket payload size.

```rust
use std::collections::{HashMap, HashSet};
use chrono::{DateTime, Utc};

/// Tracks state of all connections across all nodes
pub struct AggregatorState {
    /// Last known snapshot per node (for delta computation)
    last_snapshots: HashMap<String, NodeSnapshot>,
    
    /// Detailed connection state per node
    /// Key: "{node_id}:{connection_id}"
    connection_state: HashMap<String, ConnectionDetail>,
}

pub struct ConnectionDetail {
    pub bytes_transferred: u64,
    pub success: bool,
    pub completed_at: Option<DateTime<Utc>>,
}

impl AggregatorState {
    /// Compute delta between previous and current snapshot
    pub fn compute_delta(
        &mut self,
        node_id: &str,
        new_snapshot: &AgentMetricsSnapshot,
    ) -> DashboardUpdate {
        let prev = self.last_snapshots.get(node_id);
        
        // Calculate which connections are new, updated, or completed
        let mut connection_deltas: Vec<ConnectionDelta> = vec![];
        
        for conn in &new_snapshot.connections {
            let key = format!("{}:{}", node_id, conn.connection_id);
            
            match self.connection_state.get(&key) {
                None => {
                    // New connection - include full details
                    connection_deltas.push(ConnectionDelta::full(conn));
                }
                Some(prev_conn) => {
                    if prev_conn.bytes_transferred != conn.bytes_transferred 
                        || prev_conn.success != conn.success {
                        // Changed connection - send delta
                        connection_deltas.push(ConnectionDelta::partial(
                            &conn,
                            prev_conn.bytes_transferred,
                        ));
                    }
                }
            }
            
            // Update state
            self.connection_state.insert(key, ConnectionDetail::from(conn));
        }
        
        // Mark previously active connections as inactive if not in new snapshot
        let current_ids: HashSet<String> = new_snapshot.connections
            .iter()
            .map(|c| format!("{}:{}", node_id, c.connection_id))
            .collect();
            
        self.connection_state.retain(|key, _| {
            if key.starts_with(node_id) && !current_ids.contains(key) {
                // Connection removed - mark as inactive/completed
                true  // keep in state for historical reference
            } else {
                true
            }
        });
        
        self.last_snapshots.insert(
            node_id.to_string(),
            NodeSnapshot::from(new_snapshot),
        );
        
        DashboardUpdate {
            timestamp_ms: Utc::now().timestamp_millis(),
            nodes: self.compute_node_snapshots(),
            connections_delta: connection_deltas,
        }
    }
    
    /// Only send aggregate changes if < 5% of connections changed
    fn should_send_full_update(&self) -> bool {
        // Heuristic: if >95% unchanged, only send aggregates
        self.last_snapshots.len() == 0 || false  // Simplified logic
    }
}
```

**Delta Optimization Rules:**
1. **First snapshot from node**: Send full connection details
2. **Subsequent snapshots**: Only send connections that changed (bytes increased or status completed)
3. **Batch updates**: If <5% of connections changed, only include aggregate totals in WebSocket message
4. **Compression**: Use gzip compression on WebSocket frames

### C. gRPC Server Implementation
**File:** `src/grpc_server.rs`

```rust
use tonic::{Request, Response, Status, Streaming};
use tokio_stream::StreamExt;

pub struct StressMonitorService {
    state: Arc<Mutex<AggregatorState>>,
    ws_broadcaster: WebSocketBroadcaster,
}

#[tonic::async_trait]
impl stress_monitor::StressMonitor for StressMonitorService {
    type ReportMetricsStream = Response<Streaming<MetricsResponse>>;
    
    async fn report_metrics(
        &self,
        request: Request<Streaming<MetricsRequest>>,
    ) -> Result<Response<Self::ReportMetricsStream>, Status> {
        let mut stream = request.into_inner();
        let state = self.state.clone();
        let ws = self.ws_broadcaster.clone();
        
        // Spawn task to handle streaming connection
        tokio::spawn(async move {
            while let Some(request) = stream.next().await {
                match request {
                    Ok(metrics_request) => {
                        // Compute delta and broadcast via WebSocket
                        let mut state_guard = state.lock().unwrap();
                        let update = state_guard.compute_delta(
                            &metrics_request.node_id,
                            &metrics_request.into(),  // Convert proto to internal struct
                        );
                        
                        // Broadcast to all WebSocket clients
                        ws.broadcast_json(&update).await;
                    }
                    Err(e) => {
                        eprintln!("gRPC stream error: {}", e);
                        break;
                    }
                }
            }
        });
        
        Ok(Response::new(Box::pin(
            futures::stream::empty()  // No response needed per request
        )))
    }
}
```

### D. WebSocket Server Implementation
**File:** `src/websocket_server.rs`

Use `tokio-tungstenite` or `axum-extra` for WebSocket handling:

```rust
use tokio_tungstenite::tungstenite::{Message, Error};
use std::collections::HashSet;
use serde_json::json;

pub struct WebSocketBroadcaster {
    /// Connected clients (WebSocket connections)
    clients: Arc<Mutex<HashSet<Arc<TokioWebSocket>>>>,
}

impl WebSocketBroadcaster {
    pub async fn broadcast_json<T: Serialize>(&self, data: &T) -> Result<(), Error> {
        let json = serde_json::to_string(data).unwrap();
        let msg = Message::Text(json);
        
        // Remove dead clients
        self.clients.lock().unwrap().retain(|client| {
            client.send(msg.clone()).is_ok()
        });
        
        Ok(())
    }
    
    pub fn register(&self, ws: Arc<TokioWebSocket>) {
        self.clients.lock().unwrap().insert(ws);
    }
}

/// WebSocket endpoint handler (using axum)
pub async fn websocket_handler(
    ws: UpgradeWebsocket,
) -> Result<Response<Body>, Infallible> {
    // ... handle upgrade and register client
}
```

### E. Main Entry Point
**File:** `src/main.rs`

```rust
use axum::{Router, routing::get};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load config from environment
    let grpc_port: u16 = env::var("AGGREGATOR_GRPC_PORT")
        .unwrap_or_else(|_| "50051".to_string())
        .parse()?;
        
    let ws_port: u16 = env::var("AGGREGATOR_WS_PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()?;
    
    // Initialize state
    let state = Arc::new(Mutex::new(AggregatorState::new()));
    let ws_broadcaster = WebSocketBroadcaster::new();
    
    // Build gRPC service
    let grpc_service = StressMonitorService {
        state: state.clone(),
        ws_broadcaster: ws_broadcaster.clone(),
    };
    
    // Build axum router for WebSocket + optional HTTP health check
    let app = Router::new()
        .route("/ws", get(websocket_handler))
        .route("/health", get(|| async { "OK" }))
        .with_state(state);
    
    // Run gRPC server (separate thread or tokio runtime)
    let grpc_handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(stress_monitor::StressMonitorServer::new(grpc_service))
            .serve(format!("0.0.0.0:{}", grpc_port).parse().unwrap())
            .await
    });
    
    // Run HTTP/WebSocket server
    let http_addr = format!("0.0.0.0:{}", ws_port);
    let listener = tokio::net::TcpListener::bind(&http_addr).await?;
    axum::serve(listener, app).await?;
    
    Ok(())
}
```

### F. Cargo.toml Dependencies
```toml
[package]
name = "aerostress-aggregator"
version = "0.1.0"
edition = "2024"

[dependencies]
# Web framework
axum = { version = "0.8", features = ["ws"] }
tokio = { version = "1.49", features = ["full"] }

# gRPC
tonic = "0.12"
prost = "0.13"
tonic-build = "0.12"

# WebSocket
tokio-tungstenite = "0.26"

# Utils
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
chrono = { version = "0.4", features = ["serde"] }
futures = "0.3"
anyhow = "1.0"
log = "0.4"
env_logger = "0.11"

[build-dependencies]
tonic-build = "0.12"
```

### G. Build Script for Protobuf
**File:** `build.rs` (automatically generated by tonic-build)

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile(&["proto/stress_monitor.proto"], &["proto/"])
        .expect("Failed to compile protobuf");
    
    Ok(())
}
```

### H. Environment Variables Summary
| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `AGGREGATOR_GRPC_PORT` | No | 50051 | Port for gRPC server |
| `AGGREGATOR_WS_PORT` | No | 8080 | Port for WebSocket/HTTP server |
| `LOG_LEVEL` | No | info | Log level (debug, info, warn, error) |

---

## Component 3: Frontend Dashboard (New Svelte Application)

### Goal
Real-time visualization of all connections across all nodes using WebGL canvas, receiving updates via WebSocket.

### Project Structure
```
dashboard/
├── package.json
├── svelte.config.js
├── vite.config.js
├── src/
│   ├── main.svelte          # App entry point
│   ├── App.svelte           # Root component
│   ├── lib/
│   │   ├── components/
│   │   │   ├── MapCanvas.svelte    # WebGL canvas for connection dots
│   │   │   ├── Legend.svelte       # Status legend (green/red/yellow)
│   │   │   ├── StatsPanel.svelte   # Aggregate stats per node
│   │   │   └── NodeDetailModal.svelte  # Click to expand node details
│   │   ├── stores/
│   │   │   └── metrics.svelte.ts   # Reactive state store
│   │   └── utils/
│   │       └── websocket.ts    # WebSocket client helper
│   └── styles/
│       └── global.css
└── static/
    └── favicon.png
```

### A. State Management (Svelte Store)
**File:** `src/lib/stores/metrics.svelte.ts`

```typescript
import { writable, type Readable } from 'svelte/store';

export interface ConnectionMetric {
  connection_id: string;
  batch_number: number;
  task_number: number;
  bytes_transferred: number;
  success: boolean;
  error_code?: string;
  start_time_ms?: number;
  end_time_ms?: number;
}

export interface NodeSnapshot {
  node_id: string;
  total_bytes: number;
  total_connections: number;
  successful_connections: number;
  failed_connections: number;
  active_connections: number;
}

// Extended connection data with calculated properties (for tooltip/hover)
export interface ConnectionWithDetails extends ConnectionMetric {
  progress: number;           // transferred_bytes / target_bytes, clamped to [0,1]
  error_rate: number;         // failed_files / (successful_files + failed_files)
  is_completed: boolean;      // transferred_bytes >= target_bytes
}

export interface DashboardUpdate {
  timestamp_ms: number;
  nodes: NodeSnapshot[];
  connections_delta?: ConnectionMetric[];
}

class MetricsStore {
  private ws: WebSocket | null = null;
  
  // Reactive state
  private nodes = writable<Map<string, NodeSnapshot>>(new Map());
  private allConnections = writable<Map<string, ConnectionMetric>>(new Map());
  
  constructor() {
    this.connect();
  }
  
  connect(url?: string) {
    const wsUrl = url || `ws://${window.location.host}/ws`;
    
    this.ws = new WebSocket(wsUrl);
    
    this.ws.onmessage = (event) => {
      const update: DashboardUpdate = JSON.parse(event.data);
      this.updateState(update);
    };
    
    this.ws.onerror = (error) => {
      console.error('WebSocket error:', error);
    };
  }
  
  private updateState(update: DashboardUpdate) {
    // Update node aggregates
    const nodesMap = new Map<string, NodeSnapshot>();
    for (const node of update.nodes) {
      nodesMap.set(node.node_id, node);
    }
    this.nodes.update(state => new Map(nodesMap));
    
    // Apply connection deltas
    if (update.connections_delta) {
      const connMap = new Map(this.allConnections.get());
      for (const conn of update.connections_delta) {
        connMap.set(conn.connection_id, conn);
      }
      this.allConnections.update(state => connMap);
    }
  }
  
  // Computed properties (derived stores could be added here)
}

export const metricsStore = new MetricsStore();
```

### C. WebSocket Client Helper (Updated)
**File:** `src/lib/utils/websocket.ts`

```typescript
export class MetricsWebSocket {
  private ws: WebSocket | null = null;
  private reconnectAttempts = 0;
  private maxReconnectAttempts = 5;
  
  connect(url: string) {
    this.ws = new WebSocket(url);
    
    this.ws.onopen = () => {
      console.log('WebSocket connected');
      this.reconnectAttempts = 0;
    };
    
    this.ws.onclose = () => {
      console.log('WebSocket disconnected, attempting reconnect...');
      if (this.reconnectAttempts < this.maxReconnectAttempts) {
        setTimeout(() => this.connect(url), 2000 * (this.reconnectAttempts + 1));
        this.reconnectAttempts++;
      }
    };
    
    return this.ws;
  }
  
  send(data: string) {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(data);
    }
  }
}

// Helper functions for connection calculations
export function calculateProgress(transferredBytes: number, targetBytes: number): number {
  return Math.min(1.0, transferredBytes / targetBytes);
}

export function calculateErrorRate(successfulFiles: number, failedFiles: number): number {
  const total = successfulFiles + failedFiles;
  if (total === 0) return 0;
  return failedFiles / total;
}
```

---

### D. Helper Functions for Error Rate and Progress Calculation (Reusable Utils)
**File:** `src/lib/utils/calculations.ts`

```typescript
// Color constants (RGB arrays)
export const COLORS = {
  green: [56, 204, 96],        // Light green - 0% errors
  yellow: [253, 203, 77],      // Light yellow - >0-1% errors
  orange: [252, 141, 89],      // Light orange - ≥1-3% errors
  red: [244, 114, 106],        // Light red - >3% errors
  
  darkGreen: [34, 153, 72],    // Dark green - completed, 0% errors
  darkYellow: [189, 152, 62],  // Dark yellow - completed, >0-1% errors
  darkOrange: [189, 101, 67],  // Dark orange - completed, ≥1-3% errors
  darkRed: [189, 71, 64],      // Dark red - completed, >3% errors
} as const;

export function getColorForConnection(
  successfulFiles: number,
  failedFiles: number,
  isCompleted: boolean
): [number, number, number] {
  const total = successfulFiles + failedFiles;
  if (total === 0) return COLORS.green; // Default to light green
  
  const errorRate = failedFiles / total;
  
  let baseColor: [number, number, number];
  if (errorRate > 0.03) {
    baseColor = COLORS.red;
  } else if (errorRate >= 0.01) {
    baseColor = COLORS.orange;
  } else if (errorRate > 0) {
    baseColor = COLORS.yellow;
  } else {
    baseColor = COLORS.green;
  }
  
  // Return darker variant if completed
  return isCompleted ? getDarkVariant(baseColor) : baseColor;
}

function getDarkVariant([r, g, b]: [number, number, number]): [number, number, number] {
  // Reduce brightness by ~30%
  const factor = 0.7;
  return [
    Math.round(r * factor),
    Math.round(g * factor),
    Math.round(b * factor)
  ] as [number, number, number];
}

export function calculateProgress(transferredBytes: number, targetBytes: number): number {
  if (targetBytes === 0) return 1.0;
  return Math.min(1.0, transferredBytes / targetBytes);
}

export function calculateErrorRate(successfulFiles: number, failedFiles: number): number {
  const total = successfulFiles + failedFiles;
  if (total === 0) return 0;
  return failedFiles / total;
}
```

---

### E. WebGL Canvas Component (Deck.gl or Three.js)
**File:** `src/lib/components/MapCanvas.svelte`

#### Visualization Design

Each connection is represented as a geometric shape with two visual encodings:

1. **Fill State**: Progress toward target bytes (0-100%)
2. **Color Encoding**: Based on error rate percentage

**Error Rate Color Map:**
| Error Rate | Color (RGB) | Visual Meaning |
|------------|-------------|----------------|
| 0%         | `[56, 204, 96]` (light green) | Perfect transfer |
| >0% to <1% | `[253, 203, 77]` (light yellow) | Minor errors |
| >=1% to <=3% | `[252, 141, 89]` (light orange) | Moderate errors |
| >3%        | `[244, 114, 106]` (light red) | High error rate |

**Completion State:**
When `transferred_bytes >= target_bytes`, the shape uses a **darker variant** of its final color for contrast:
- Dark green: `[34, 153, 72]`
- Dark yellow: `[189, 152, 62]`
- Dark orange: `[189, 101, 67]`
- Dark red: `[189, 71, 64]`

**Shape Choice:**
For WebGL performance with 10k+ connections:
- **Circle (Pie Chart Style)**: Uses arc geometry, fills from -π/2 to angle based on progress. More visually intuitive for "completion" metaphor.
- **Square (Progress Bar Style)**: Draws rectangle clipped by progress ratio. Simpler GPU shader logic, potentially faster.

**Recommendation**: Use **square with left-to-right fill**. Three.js `ClippingPlanes` or custom shader can clip the square efficiently. Circle requires more complex arc math per vertex which adds GPU overhead at scale.

#### Implementation (Three.js with Custom Shader)
```svelte
<script lang="ts">
  import { onMount, onDestroy, tick } from 'svelte';
  import * as THREE from 'three';
  
  let canvasRef: HTMLCanvasElement;
  const nodes = $state<Map<string, NodeSnapshot>>(new Map());
  const connections = $state<Map<string, ConnectionMetric>>(new Map());
  
  let scene: THREE.Scene | null = null;
  let camera: THREE.Camera | null = null;
  let renderer: THREE.WebGLRenderer | null = null;
  let pointsMesh: THREE.Points | null = null;
  
  // Custom shader material for progress-filled squares
  const vertexShader = `
    attribute vec2 position;
    varying vec2 vUv;
    void main() {
      vUv = uv;
      gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
    }
  `;

  const fragmentShader = `
    uniform float progress;       // 0.0 to 1.0 filled ratio
    uniform vec3 baseColor;       // Error-rate-based color
    varying vec2 vUv;
    
    void main() {
      if (vUv.x > progress) {
        discard;  // Not yet filled portion - transparent
      }
      gl_FragColor = vec4(baseColor, 1.0);
    }
  `;
  
  onMount(async () => {
    await tick();
    
    scene = new THREE.Scene();
    scene.background = new THREE.Color(0x1a1a2e);
    
    camera = new THREE.PerspectiveCamera(75, canvasRef.clientWidth / canvasRef.clientHeight, 0.1, 1000);
    camera.position.z = 50;
    
    renderer = new THREE.WebGLRenderer({ canvas: canvasRef, antialias: true });
    renderer.setSize(canvasRef.clientWidth, canvasRef.clientHeight);
    
    const animate = () => {
      requestAnimationFrame(animate);
      if (renderer && scene) {
        renderer.render(scene, camera!);
      }
    };
    animate();
  });
  
  // Helper: Calculate color based on error rate
  function getErrorRateColor(successfulFiles: number, failedFiles: number): [number, number, number] {
    const total = successfulFiles + failedFiles;
    if (total === 0) return [56, 204, 96]; // Default light green
    
    const errorRate = failedFiles / total;
    
    if (errorRate > 0.03) {
      return [244, 114, 106]; // Light red
    } else if (errorRate >= 0.01) {
      return [252, 141, 89]; // Light orange
    } else if (errorRate > 0) {
      return [253, 203, 77]; // Light yellow
    } else {
      return [56, 204, 96]; // Light green
    }
  }
  
  function getCompletionColor(successfulFiles: number, failedFiles: number): [number, number, number] {
    const total = successfulFiles + failedFiles;
    if (total === 0) return [34, 153, 72];
    
    const errorRate = failedFiles / total;
    
    if (errorRate > 0.03) {
      return [189, 71, 64]; // Dark red
    } else if (errorRate >= 0.01) {
      return [189, 101, 67]; // Dark orange
    } else if (errorRate > 0) {
      return [189, 152, 62]; // Dark yellow
    } else {
      return [34, 153, 72]; // Dark green
    }
  }
  
  $: updatePoints() {
    const positions = [];
    const colors = [];
    const progressVals = [];
    
    for (const conn of connections.values()) {
      const x = (conn.batch_number % 10) * 10 - 45;
      const y = Math.floor(conn.batch_number / 10) * 10 - 45;
      
      // Calculate fill progress
      const progress = Math.min(1.0, conn.transferred_bytes / conn.target_bytes);
      
      // Determine color based on error rate and completion state
      let [r, g, b]: [number, number, number];
      if (conn.transferred_bytes >= conn.target_bytes) {
        // Completed - use darker variant
        [r, g, b] = getCompletionColor(conn.successful_files, conn.failed_files);
      } else {
        // In-progress - use lighter variant
        [r, g, b] = getErrorRateColor(conn.successful_files, conn.failed_files);
      }
      
      positions.push(x, y, 0);
      colors.push(r / 255, g / 255, b / 255); // Normalize to 0-1
      progressVals.push(progress);
    }
    
    const geometry = new THREE.BufferGeometry();
    geometry.setAttribute('position', new THREE.Float32BufferAttribute(positions, 3));
    geometry.setAttribute('color', new THREE.Float32BufferAttribute(colors, 3));
    
    // Pass progress as attribute for shader
    geometry.setAttribute('progress', new THREE.Float32BufferAttribute(progressVals, 1));
    
    const material = new THREE.ShaderMaterial({
      uniforms: {
        progress: { value: 0.5 }, // Will be updated per-instance in advanced impl
      },
      vertexShader,
      fragmentShader,
      transparent: true,
    });
    
    if (pointsMesh) {
      scene!.remove(pointsMesh);
    }
    
    pointsMesh = new THREE.Points(geometry, material);
    scene!.add(pointsMesh);
  }
</script>

```svelte
<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { DeckGL, ScatterplotLayer } from '@deck.gl/react';
  import { metricsStore } from '$lib/stores/metrics';
  
  interface Props {
    width?: number;
    height?: number;
  }

  let { width = 800, height = 600 }: Props = $props();
  
  const nodes = $state<Map<string, NodeSnapshot>>(new Map());
  const connections = $state<Map<string, ConnectionMetric>>(new Map());
  
  // Subscribe to store updates
  onMount(() => {
    const unsubNodes = metricsStore.nodes.subscribe(val => nodes.set(val));
    const unsubConns = metricsStore.allConnections.subscribe(val => connections.set(val));
    
    return () => {
      unsubNodes();
      unsubConns();
    };
  });
  
  // Convert connection map to Deck.gl data format
  $: connectionData = Array.from(connections.values()).map(conn => ({
    id: conn.connection_id,
    x: (conn.batch_number % 10) * 100,  // Simple grid layout
    y: Math.floor(conn.batch_number / 10) * 100,
    color: conn.success ? [0, 255, 0] : [255, 0, 0],  // Green/red
    radius: 3,
    bytes: conn.bytes_transferred,
  }));
  
  const layer = new ScatterplotLayer({
    id: 'connections',
    data: connectionData,
    getPosition: (d) => [d.x, d.y],
    getFillColor: (d) => d.color as [number, number, number],
    getRadius: 3,
  });
</script>

<div class="canvas-container">
  <DeckGL 
    width={width} 
    height={height} 
    layers={[layer]}
    initialViewState={{
      longitude: 0,
      latitude: 0,
      zoom: 1,
    }}
  />
</div>

<style>
  .canvas-container {
    position: relative;
    width: 100%;
    height: 100%;
    background: #1a1a2e;
  }
</style>
```

**Alternative: Pure Three.js Implementation** (if Deck.gl is too heavy):

```svelte
<script lang="ts">
  import { onMount, onDestroy, tick } from 'svelte';
  import * as THREE from 'three';
  
  let canvasRef: HTMLCanvasElement;
  const nodes = $state<Map<string, NodeSnapshot>>(new Map());
  const connections = $state<Map<string, ConnectionMetric>>(new Map());
  
  let scene: THREE.Scene | null = null;
  let camera: THREE.Camera | null = null;
  let renderer: THREE.WebGLRenderer | null = null;
  let pointsMesh: THREE.Points | null = null;
  
  onMount(async () => {
    await tick();
    
    // Initialize Three.js scene
    scene = new THREE.Scene();
    scene.background = new THREE.Color(0x1a1a2e);
    
    camera = new THREE.PerspectiveCamera(75, canvasRef.clientWidth / canvasRef.clientHeight, 0.1, 1000);
    camera.position.z = 50;
    
    renderer = new THREE.WebGLRenderer({ canvas: canvasRef, antialias: true });
    renderer.setSize(canvasRef.clientWidth, canvasRef.clientHeight);
    
    // Animation loop
    const animate = () => {
      requestAnimationFrame(animate);
      if (renderer && scene) {
        renderer.render(scene, camera!);
      }
    };
    animate();
  });
  
  $: updatePoints() {
    const positions = [];
    const colors = [];
    
    for (const conn of connections.values()) {
      const x = (conn.batch_number % 10) * 10 - 45;
      const y = Math.floor(conn.batch_number / 10) * 10 - 45;
      
      positions.push(x, y, 0);
      colors.push(
        conn.success ? 0 : 1,   // r
        conn.success ? 1 : 0,   // g
        0                       // b
      );
    }
    
    const geometry = new THREE.BufferGeometry();
    geometry.setAttribute('position', new THREE.Float32BufferAttribute(positions, 3));
    geometry.setAttribute('color', new THREE.Float32BufferAttribute(colors, 3));
    
    const material = new THREE.PointsMaterial({ size: 0.5, vertexColors: true });
    
    if (pointsMesh) {
      scene!.remove(pointsMesh);
    }
    
    pointsMesh = new THREE.Points(geometry, material);
    scene!.add(pointsMesh);
  }
</script>

<canvas bind:this={canvasRef} class="webgl-canvas"></canvas>

<style>
  .webgl-canvas {
    width: 100%;
    height: 100%;
    display: block;
  }
</style>

---

### Alternative: Deck.gl ScatterplotLayer Approach

If Three.js custom shaders are too complex, **Deck.gl** offers a simpler approach using built-in layers:

```svelte
<script lang="ts">
  import { onMount } from 'svelte';
  import { DeckGL, CompositeLayer } from '@deck.gl/react';
  import { metricsStore } from '$lib/stores/metrics';
  
  interface Props {
    width?: number;
    height?: number;
  }

  let { width = 800, height = 600 }: Props = $props();
  
  const nodes = $state<Map<string, NodeSnapshot>>(new Map());
  const connections = $state<Map<string, ConnectionMetric>>(new Map());
  
  onMount(() => {
    metricsStore.nodes.subscribe(val => nodes.set(val));
    metricsStore.allConnections.subscribe(val => connections.set(val));
  });
  
  // Helper functions for color calculation
  const getErrorRateColor = (successfulFiles: number, failedFiles: number) => {
    const total = successfulFiles + failedFiles;
    if (total === 0) return [56, 204, 96];
    
    const errorRate = failedFiles / total;
    
    if (errorRate > 0.03) {
      return [244, 114, 106]; // Light red
    } else if (errorRate >= 0.01) {
      return [252, 141, 89]; // Light orange
    } else if (errorRate > 0) {
      return [253, 203, 77]; // Light yellow
    } else {
      return [56, 204, 96]; // Light green
    }
  };
  
  const getCompletionColor = (successfulFiles: number, failedFiles: number) => {
    const total = successfulFiles + failedFiles;
    if (total === 0) return [34, 153, 72];
    
    const errorRate = failedFiles / total;
    
    if (errorRate > 0.03) {
      return [189, 71, 64]; // Dark red
    } else if (errorRate >= 0.01) {
      return [189, 101, 67]; // Dark orange
    } else if (errorRate > 0) {
      return [189, 152, 62]; // Dark yellow
    } else {
      return [34, 153, 72]; // Dark green
    }
  };
  
  $: connectionData = Array.from(connections.values()).map(conn => {
    const progress = Math.min(1.0, conn.transferred_bytes / conn.target_bytes);
    let fillColor: [number, number, number];
    
    if (conn.transferred_bytes >= conn.target_bytes) {
      fillColor = getCompletionColor(conn.successful_files, conn.failed_files);
    } else {
      fillColor = getErrorRateColor(conn.successful_files, conn.failed_files);
    }
    
    return {
      id: conn.connection_id,
      x: (conn.batch_number % 10) * 10 - 45,
      y: Math.floor(conn.batch_number / 10) * 10 - 45,
      progress, // Pass to custom layer for fill rendering
      fillColor,
    };
  });
</script>

<div class="canvas-container">
  <DeckGL 
    width={width} 
    height={height} 
    layers={[
      new CompositeLayer({
        id: 'progress-squares',
        data: connectionData,
        render: ({ gl, modelMatrix, projectionMatrix, viewport }) => {
          // Custom rendering for progress-filled squares
          // This would use deck.gl's WebGL API directly
        }
      })
    ]}
    initialViewState={{
      longitude: 0,
      latitude: 0,
      zoom: 1,
    }}
  />
</div>
```

### F. Stats Panel Component (Updated for Per-Connection Metrics)
**File:** `src/lib/components/StatsPanel.svelte`

Now includes error rate breakdown and per-node progress visualization.

```svelte
<script lang="ts">
  import { metricsStore } from '$lib/stores/metrics';
  
  const nodes = $state<Map<string, NodeSnapshot>>(new Map());
  const connections = $state<Map<string, ConnectionMetric>>(new Map());
  
  onMount(() => {
    const unsubNodes = metricsStore.nodes.subscribe(val => nodes.set(val));
    const unsubConns = metricsStore.allConnections.subscribe(val => connections.set(val));
    return () => { unsubNodes(); unsubConns(); };
  });
  
  $: totalNodes = nodes.size;
  $: totalConnections = Array.from(nodes.values()).reduce((sum, n) => sum + n.total_connections, 0);
  $: totalBytes = Array.from(nodes.values()).reduce((sum, n) => sum + n.total_bytes, 0);
  
  // Calculate aggregate error rate per node
  function getNodeErrorRate(nodeId: string): number {
    const nodeConns = Array.from(connections.values()).filter(c => 
      c.connection_id.startsWith(`${nodeId}`)
    );
    const totalFiles = nodeConns.reduce((sum, c) => sum + c.file_count, 0);
    const failedFiles = nodeConns.reduce((sum, c) => sum + c.failed_files, 0);
    return totalFiles > 0 ? (failedFiles / totalFiles * 100).toFixed(2) : '0.00';
  }
</script>

<div class="stats-panel">
  <h3>Stress Test Overview</h3>
  
  <div class="summary-cards">
    <div class="card">
      <span class="label">Nodes Active</span>
      <span class="value">{totalNodes}</span>
    </div>
    
    <div class="card">
      <span class="label">Total Connections</span>
      <span class="value">{totalConnections.toLocaleString()}</span>
    </div>
    
    <div class="card">
      <span class="label">Bytes Transferred</span>
      <span class="value">{(totalBytes / 1024 / 1024).toFixed(2)} MB</span>
    </div>
  </div>
  
  <div class="node-list">
    {#each Array.from(nodes.entries()) as [nodeId, node]}
      <div class="node-row" on:click={() => showNodeDetails(nodeId)}>
        <span class="node-id">{nodeId}</span>
        <span class="conn-count">{node.total_connections} conns</span>
        <span class="bytes-count">{(node.total_bytes / 1024 / 1024).toFixed(1)} MB</span>
        {#if node.failed_connections > 0}
          <span class="error-count" title="{node.failed_connections} errors">⚠️ {node.failed_connections}</span>
        {/if}
      </div>
    {/each}
  </div>
</div>

<style>
  .stats-panel {
    background: #16213e;
    color: white;
    padding: 1rem;
    border-radius: 8px;
    width: 300px;
  }
  
  .summary-cards {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 0.5rem;
    margin-bottom: 1rem;
  }
  
  .card {
    background: #0f3460;
    padding: 0.5rem;
    border-radius: 4px;
    text-align: center;
  }
  
  .label {
    font-size: 0.75rem;
    color: #888;
  }
  
  .value {
    font-size: 1.2rem;
    font-weight: bold;
  }
  
  .node-list {
    max-height: 300px;
    overflow-y: auto;
  }
  
  .node-row {
    display: flex;
    justify-content: space-between;
    padding: 0.5rem;
    border-bottom: 1px solid #0f3460;
    cursor: pointer;
  }
  
  .node-row:hover {
    background: #0f3460;
  }
</style>
```

### G. Legend Component (Color Guide)
**File:** `src/lib/components/Legend.svelte`

Displays the error rate color mapping for user reference.

```svelte
<script lang="ts">
  import { onMount } from 'svelte';
</script>

<div class="legend-panel">
  <h4>Error Rate Color Guide</h4>
  
  <div class="legend-item completed">
    <div class="color-box" style="background: #229948;"></div>
    <span>0% errors (completed)</span>
  </div>
  
  <div class="legend-item in-progress">
    <div class="color-box" style="background: #38CC60;"></div>
    <span>0% errors</span>
  </div>
  
  <div class="legend-item warning-low">
    <div class="color-box" style="background: #FDCB4D;"></div>
    <span>>0% to &lt;1% errors</span>
  </div>
  
  <div class="legend-item warning-mid">
    <div class="color-box" style="background: #FC8D59;"></div>
    <span>≥1% to ≤3% errors</span>
  </div>
  
  <div class="legend-item warning-high">
    <div class="color-box" style="background: #F4726A;"></div>
    <span>>3% errors</span>
  </div>
</div>

<style>
  .legend-panel {
    background: #16213e;
    padding: 0.75rem;
    border-radius: 8px;
    margin-bottom: 1rem;
  }
  
  h4 {
    font-size: 0.875rem;
    color: #aaa;
    margin-bottom: 0.5rem;
  }
  
  .legend-item {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.375rem 0;
    font-size: 0.75rem;
    color: #ddd;
  }
  
  .color-box {
    width: 16px;
    height: 16px;
    border-radius: 4px;
    border: 1px solid rgba(255,255,255,0.2);
  }
</style>
```

---

### H. Main App Component (Updated with Legend and Tooltips)
**File:** `src/App.svelte`

```svelte
<script lang="ts">
  import MapCanvas from './lib/components/MapCanvas.svelte';
  import StatsPanel from './lib/components/StatsPanel.svelte';
  import Legend from './lib/components/Legend.svelte';
  
  interface ConnectionTooltip {
    connection_id: string;
    batch_number: number;
    task_number: number;
    target_bytes: number;
    transferred_bytes: number;
    progress: number;
    successful_files: number;
    failed_files: number;
    error_rate: number;
  }
  
  let hoveredConnection = $state<ConnectionTooltip | null>(null);
</script>

<svelte:head>
  <title>Aerostress Monitor</title>
</svelte:head>

<div class="app">
  <header class="header">
    <h1>AeroFTP Stress Test Monitor</h1>
    <span class="status-indicator {connected ? 'connected' : 'disconnected'}"></span>
  </header>
  
  <main class="main-content">
    <aside class="sidebar">
      <StatsPanel />
      <Legend />
    </aside>
    
    <section class="canvas-area">
      <MapCanvas 
        width={window.innerWidth - 320} 
        height={window.innerHeight - 150}
        on:connectionHover={(e) => hoveredConnection = e.detail}
        on:connectionLeave={() => hoveredConnection = null}
      />
      
      {#if hoveredConnection}
        <div class="tooltip">
          <h4>Connection Details</h4>
          <p><strong>ID:</strong> {hoveredConnection.connection_id}</p>
          <p><strong>Batch/Task:</strong> {hoveredConnection.batch_number}/{hoveredConnection.task_number}</p>
          <p><strong>Progress:</strong> {(hoveredConnection.progress * 100).toFixed(1)}%</p>
          <p><strong>Transferred:</strong> {(hoveredConnection.transferred_bytes / 1024 / 1024).toFixed(1)} MB</p>
          <p><strong>Target:</strong> {(hoveredConnection.target_bytes / 1024 / 1024).toFixed(1)} MB</p>
          <p><strong>Files:</strong> {hoveredConnection.successful_files} successful, {hoveredConnection.failed_files} failed</p>
          <p><strong>Error Rate:</strong> {(hoveredConnection.error_rate * 100).toFixed(2)}%</p>
        </div>
      {/if}
      
      {#if connections.size === 0}
        <div class="loading-overlay">
          <p>Waiting for stress test data...</p>
          <small>Start agents with --reporter enabled</small>
        </div>
      {/if}
    </section>
  </main>
  
  <footer class="footer">
    <span>Last update: {$lastUpdate}</span>
  </footer>
</div>

<style>
  /* ... existing styles ... */
  
  .tooltip {
    position: absolute;
    top: 20px;
    right: 20px;
    background: rgba(22, 33, 62, 0.95);
    color: white;
    padding: 1rem;
    border-radius: 8px;
    border: 1px solid #0f3460;
    box-shadow: 0 4px 12px rgba(0,0,0,0.3);
    max-width: 280px;
    font-size: 0.875rem;
    z-index: 100;
  }
  
  .tooltip h4 {
    margin: 0 0 0.5rem 0;
    color: #38CC60;
  }
  
  .tooltip p {
    margin: 0.25rem 0;
    line-height: 1.4;
  }
</style>
```

---

### I. Summary: Color Mapping Table (Reference for Developers)
| Error Rate | RGB Color | Hex Code | Visual Meaning |
|------------|-----------|----------|----------------|
| 0% | `[56, 204, 96]` | `#38CC60` | Light green - Perfect transfer |
| >0% to <1% | `[253, 203, 77]` | `#FDCB4D` | Light yellow - Minor errors |
| ≥1% to ≤3% | `[252, 141, 89]` | `#FC8D59` | Light orange - Moderate errors |
| >3% | `[244, 114, 106]` | `#F4726A` | Light red - High error rate |

**Completed State (Same colors, ~30% darker):**
| Error Rate | RGB Color | Hex Code | Visual Meaning |
|------------|-----------|----------|----------------|
| 0% | `[34, 153, 72]` | `#229948` | Dark green - Completed without errors |
| >0% to <1% | `[189, 152, 62]` | `#BD983E` | Dark yellow - Completed with minor errors |
| ≥1% to ≤3% | `[189, 101, 67]` | `#BD6543` | Dark orange - Completed with moderate errors |
| >3% | `[189, 71, 64]` | `#BD4740` | Dark red - Completed with high error rate |

---

### J. package.json Dependencies
```json
{
  "name": "aerostress-dashboard",
  "version": "0.1.0",
  "type": "module",
  "scripts": {
    "dev": "vite dev",
    "build": "vite build",
    "preview": "vite preview"
  },
  "dependencies": {
    "svelte": "^5.0.0",
    "@deck.gl/react": "^8.9.0",
    "three": "^0.160.0",
    "d3": "^7.8.0"
  },
  "devDependencies": {
    "@sveltejs/vite-plugin-svelte": "^4.0.0",
    "vite": "^5.0.0",
    "@types/three": "^0.160.0",
    "@types/d3": "^7.4.0"
  }
}
```

### K. Environment Variables Summary (Frontend)
| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `VITE_WS_URL` | No | `ws://localhost:8080/ws` | WebSocket endpoint URL |

---

## Implementation Order & Dependencies

### Phase 1: Aggregator Service (Week 1-2)
1. ✅ Set up Rust project with Cargo.toml dependencies
2. ✅ Define protobuf schema and generate code
3. ✅ Implement basic gRPC server that receives metrics
4. ✅ Implement state management for delta calculation
5. ✅ Add WebSocket broadcasting
6. ✅ Test with mock data

### Phase 2: Stress Tester Modifications (Week 2-3)
1. ✅ Add node ID generation/configuration to existing codebase
2. ✅ Create metrics collector with Arc<Mutex<>> thread-safety
3. ✅ Integrate connection tracking into main execution flow
4. ✅ Implement gRPC client for sending snapshots
5. ✅ Add all new environment variables (AEROSTRESS_NODE_ID, AEROSTRESS_AGGREGATOR_URL)
6. ✅ Test locally with aggregator running

### Phase 3: Frontend Dashboard (Week 3-4)
1. ✅ Set up Svelte + Vite project
2. ✅ Implement WebSocket client store
3. ✅ Create WebGL canvas component (Deck.gl or Three.js)
4. ✅ Add stats panel and node detail views
5. ✅ Style and polish UI
6. ✅ Test with real data from aggregator

### Phase 4: Integration Testing (Week 4-5)
1. ✅ Deploy multiple agent nodes (simulate 5-15 nodes)
2. ✅ Load test with 1000 connections per node
3. ✅ Verify delta efficiency (<5% payload when stable)
4. ✅ Test WebSocket reconnection handling
5. ✅ Performance benchmark browser rendering at scale

---

## Performance Considerations

### gRPC Communication
- **Compression**: Enable gzip compression on gRPC channel (default in tonic)
- **Batching**: Send snapshots every 3 seconds as configured (not per-connection)
- **Streaming**: Use bidirectional streaming for continuous connection

### WebSocket Delta Optimization
- Only send changed connections (<5% threshold triggers aggregate-only mode)
- Use binary protocol (msgpack or protobuf over WS) instead of JSON if needed
- Implement client-side delta merging to avoid redundant data

### Browser Rendering
- **Level of Detail**: Reduce dot count when zoomed out, show only node aggregates
- **Spatial Hashing**: Group connections into grid cells for efficient rendering
- **GPU Instancing**: Use WebGL instanced drawing for 10k+ dots efficiently

---

## Deployment Architecture

### Docker Compose Example (for local testing)
```yaml
version: '3.8'
services:
  aggregator:
    build: ./aggregator
    ports:
      - "50051:50051"  # gRPC
      - "8080:8080"    # WebSocket/HTTP
    environment:
      - AGGREGATOR_GRPC_PORT=50051
      - AGGREGATOR_WS_PORT=8080
  
  stress-node-1:
    build: .
    command: >
      AEROSTRESS_NODE_ID=node-a 
      AEROSTRESS_AGGREGATOR_URL=grpc://aggregator:50051
      AEROSTRESS_BATCHES=8 AEROSTRESS_TASKS=20 ...
  
  stress-node-2:
    build: .
    command: >
      AEROSTRESS_NODE_ID=node-b 
      AEROSTRESS_AGGREGATOR_URL=grpc://aggregator:50051
      AEROSTRESS_BATCHES=8 AEROSTRESS_TASKS=20 ...
  
  dashboard:
    build: ./dashboard
    ports:
      - "3000:3000"
    environment:
      - VITE_WS_URL=ws://localhost:8080/ws
```

---

## Success Criteria

1. ✅ All 5-15 agent nodes can report metrics simultaneously
2. ✅ WebSocket updates arrive every 3 seconds with <5% payload when stable
3. ✅ Browser can render 10,000+ connection dots at 60 FPS
4. ✅ Delta calculation is CPU-efficient (<10ms per snapshot)
5. ✅ System recovers gracefully from agent disconnections
6. ✅ Frontend displays real-time visual feedback of stress test progress

---

## Open Questions / Decisions Needed

1. **Connection Visualization**: Grid layout vs geographic map vs network topology?
2. **Historical Data**: Do we need to store metrics for replay/analysis (Redis/InfluxDB)?
3. **Authentication**: Should WebSocket/gRPC require auth tokens?
4. **Scaling**: If >50 nodes, should aggregator cluster with Redis Pub/Sub?

---

## Next Steps

Each component can be developed in parallel by separate coding agents:
- **Agent A**: Focus on Component 1 (Stress Tester modifications)
- **Agent B**: Focus on Component 2 (Aggregator Service from scratch)
- **Agent C**: Focus on Component 3 (Frontend Dashboard from scratch)

All three share the protobuf schema as a contract, which should be finalized first.
