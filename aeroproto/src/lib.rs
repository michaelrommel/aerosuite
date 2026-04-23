//! Generated protobuf types and gRPC service definitions for the aeromonitor
//! protocol.
//!
//! This crate is the single source of truth for the wire format shared between
//! `aerogym` (agents) and `aerocoach` (controller / aggregator).
//!
//! # Re-exports
//!
//! All generated items live in the [`aeromonitor`] module, mirroring the
//! `package aeromonitor` declaration in the `.proto` file.  Import paths
//! therefore look like:
//!
//! ```rust,ignore
//! use aeroproto::aeromonitor::{RegisterRequest, LoadPlan, AgentReport};
//! use aeroproto::aeromonitor::agent_service_client::AgentServiceClient;
//! use aeroproto::aeromonitor::agent_service_server::{AgentService, AgentServiceServer};
//! ```

/// All types and service stubs generated from `proto/aeromonitor.proto`.
pub mod aeromonitor {
    tonic::include_proto!("aeromonitor");
}
