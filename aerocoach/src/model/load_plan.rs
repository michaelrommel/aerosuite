//! Load plan model: deserialisation, validation, and conversion to proto types.
//!
//! The JSON load-plan file is designed to be hand-editable.  This module owns
//! the serde structs that mirror that file format, validates their contents,
//! and converts them to the generated [`aeroproto::aeromonitor::LoadPlan`] type
//! used on the wire.
//!
//! # Example JSON
//! ```json
//! {
//!   "plan_id": "test-2026-04-22",
//!   "slice_duration_ms": 60000,
//!   "total_bandwidth_bps": 104857600,
//!   "file_distribution": {
//!     "buckets": [
//!       { "bucket_id": "xs",    "size_min_bytes":        0, "size_max_bytes":  10485760, "percentage": 0.580 },
//!       { "bucket_id": "sm",    "size_min_bytes": 10485760, "size_max_bytes":  52428800, "percentage": 0.129 },
//!       { "bucket_id": "md",    "size_min_bytes": 52428800, "size_max_bytes": 104857600, "percentage": 0.087 },
//!       { "bucket_id": "lg",    "size_min_bytes":104857600, "size_max_bytes": 209715200, "percentage": 0.063 },
//!       { "bucket_id": "xl",    "size_min_bytes":209715200, "size_max_bytes": 524288000, "percentage": 0.052 },
//!       { "bucket_id": "xxl",   "size_min_bytes":524288000, "size_max_bytes":1073741824, "percentage": 0.040 },
//!       { "bucket_id": "giant", "size_min_bytes":1073741824,"size_max_bytes":2147483648, "percentage": 0.049 }
//!     ]
//!   },
//!   "slices": [
//!     { "slice_index": 0, "total_connections": 50  },
//!     { "slice_index": 1, "total_connections": 120 }
//!   ]
//! }
//! ```

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use aeroproto::aeromonitor as proto;

// ── File-format structs (serde) ────────────────────────────────────────────

/// Top-level load plan as read from a JSON file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadPlanFile {
    /// Human-readable identifier included in result file names.
    pub plan_id: String,

    /// Duration of every time slice in milliseconds.  All slices share the
    /// same duration; per-slice overrides are intentionally unsupported to
    /// keep the model simple.
    pub slice_duration_ms: u64,

    /// Aggregate bandwidth ceiling across **all** agents (bytes per second).
    /// Each agent's individual share is computed by [`super::distributor`].
    pub total_bandwidth_bps: u64,

    /// File-size histogram used when agents generate their test files.
    pub file_distribution: FileDistributionSpec,

    /// Ordered connection-count profile; one entry per time slice.
    pub slices: Vec<SliceSpec>,
}

/// File-size distribution histogram.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDistributionSpec {
    pub buckets: Vec<BucketSpec>,
}

/// One bucket in the file-size histogram.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BucketSpec {
    /// Short identifier used in filenames and logs, e.g. `"xs"`, `"giant"`.
    pub bucket_id: String,
    pub size_min_bytes: u64,
    pub size_max_bytes: u64,
    /// Fraction of connections that should use this bucket; must be in
    /// `[0.0, 1.0]` and all buckets must sum to approximately `1.0`.
    pub percentage: f32,
}

/// Connection-count specification for one time slice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SliceSpec {
    /// Zero-based index; slices must be present in order `0, 1, …, n-1`.
    pub slice_index: u32,
    /// Target number of **concurrent** connections across all agents combined.
    pub total_connections: u32,
}

// ── Loading ────────────────────────────────────────────────────────────────

impl LoadPlanFile {
    /// Read and validate a load plan from a JSON file on disk.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read, the JSON is malformed, or
    /// [`Self::validate`] finds a semantic problem.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read plan file {}", path.display()))?;
        let plan: Self = serde_json::from_str(&raw)
            .with_context(|| format!("cannot parse plan file {}", path.display()))?;
        plan.validate()?;
        Ok(plan)
    }

    // ── Validation ────────────────────────────────────────────────────────

    /// Validate the plan for semantic correctness.
    ///
    /// Checks performed:
    /// - `plan_id` is non-empty
    /// - `slice_duration_ms` and `total_bandwidth_bps` are positive
    /// - At least one slice and one bucket are present
    /// - Slice indices form a contiguous `0..n` sequence without gaps or duplicates
    /// - Bucket IDs are unique
    /// - Each bucket has `size_min_bytes < size_max_bytes`
    /// - All bucket percentages are in `[0.0, 1.0]` and sum to `1.0 ± 0.01`
    ///
    /// # Errors
    /// Returns a descriptive error for the first validation failure encountered.
    pub fn validate(&self) -> Result<()> {
        if self.plan_id.trim().is_empty() {
            bail!("plan_id must not be empty");
        }
        if self.slice_duration_ms == 0 {
            bail!("slice_duration_ms must be greater than zero");
        }
        // total_bandwidth_bps == 0 means unlimited; any positive value is a
        // bytes-per-second ceiling shared across all agents.

        // ── Slices ────────────────────────────────────────────────────────
        if self.slices.is_empty() {
            bail!("plan must contain at least one slice");
        }
        // Require slices in order 0, 1, …, n-1 with no gaps.
        for (pos, slice) in self.slices.iter().enumerate() {
            if slice.slice_index != pos as u32 {
                bail!(
                    "slice at position {pos} has slice_index {}; expected {pos} \
                     (slices must be ordered 0, 1, …, n-1 with no gaps)",
                    slice.slice_index
                );
            }
        }

        // ── Buckets ───────────────────────────────────────────────────────
        if self.file_distribution.buckets.is_empty() {
            bail!("file_distribution must contain at least one bucket");
        }
        let mut seen_ids: HashSet<&str> = HashSet::new();
        let mut pct_sum: f64 = 0.0;

        for bucket in &self.file_distribution.buckets {
            if bucket.bucket_id.trim().is_empty() {
                bail!("every bucket must have a non-empty bucket_id");
            }
            if !seen_ids.insert(bucket.bucket_id.as_str()) {
                bail!("duplicate bucket_id {:?}", bucket.bucket_id);
            }
            if bucket.size_min_bytes >= bucket.size_max_bytes {
                bail!(
                    "bucket {:?}: size_min_bytes ({}) must be less than size_max_bytes ({})",
                    bucket.bucket_id,
                    bucket.size_min_bytes,
                    bucket.size_max_bytes
                );
            }
            if !(0.0..=1.0).contains(&bucket.percentage) {
                bail!(
                    "bucket {:?}: percentage {} is outside [0.0, 1.0]",
                    bucket.bucket_id,
                    bucket.percentage
                );
            }
            pct_sum += bucket.percentage as f64;
        }

        const PCT_TOLERANCE: f64 = 0.01;
        if (pct_sum - 1.0).abs() > PCT_TOLERANCE {
            bail!(
                "bucket percentages sum to {pct_sum:.4}; expected 1.0 ± {PCT_TOLERANCE}"
            );
        }

        Ok(())
    }

    // ── Proto conversion ──────────────────────────────────────────────────

    /// Convert to the protobuf [`proto::LoadPlan`] sent to agents.
    ///
    /// `total_agents` is required here because it is part of the proto message
    /// (agents use it to compute their individual connection and bandwidth
    /// shares) but is not part of the JSON file (it is only known at runtime
    /// once agents register).
    pub fn to_proto(&self, total_agents: u32) -> proto::LoadPlan {
        proto::LoadPlan {
            plan_id: self.plan_id.clone(),
            start_time_ms: 0, // Filled in by the slice clock when the test starts.
            slice_duration_ms: self.slice_duration_ms,
            slices: self
                .slices
                .iter()
                .map(|s| proto::TimeSlice {
                    slice_index: s.slice_index,
                    total_connections: s.total_connections,
                })
                .collect(),
            file_distribution: Some(proto::FileSizeDistribution {
                buckets: self
                    .file_distribution
                    .buckets
                    .iter()
                    .map(|b| proto::FileSizeBucket {
                        bucket_id: b.bucket_id.clone(),
                        size_min_bytes: b.size_min_bytes,
                        size_max_bytes: b.size_max_bytes,
                        percentage: b.percentage,
                    })
                    .collect(),
            }),
            total_bandwidth_bps: self.total_bandwidth_bps,
            total_agents,
        }
    }

    /// Total number of slices in the plan.
    pub fn total_slices(&self) -> u32 {
        self.slices.len() as u32
    }

    /// Returns 1 as a placeholder for the `RegisterResponse`.
    ///
    /// The real agent count is pushed to all connected agents via a
    /// [`LoadPlanUpdate`] when the operator clicks **Confirm Plan**
    /// (`POST /confirm`), and again at `POST /start` as a safety net for
    /// any agents that joined after the last Confirm.
    pub fn total_agents_hint(&self) -> u32 {
        1
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_plan() -> LoadPlanFile {
        LoadPlanFile {
            plan_id: "test".into(),
            slice_duration_ms: 60_000,
            total_bandwidth_bps: 10_000_000,
            file_distribution: FileDistributionSpec {
                buckets: vec![BucketSpec {
                    bucket_id: "xs".into(),
                    size_min_bytes: 0,
                    size_max_bytes: 10_485_760,
                    percentage: 1.0,
                }],
            },
            slices: vec![SliceSpec {
                slice_index: 0,
                total_connections: 10,
            }],
        }
    }

    #[test]
    fn valid_plan_passes() {
        assert!(minimal_plan().validate().is_ok());
    }

    #[test]
    fn empty_plan_id_rejected() {
        let mut p = minimal_plan();
        p.plan_id = "  ".into();
        assert!(p.validate().is_err());
    }

    #[test]
    fn zero_slice_duration_rejected() {
        let mut p = minimal_plan();
        p.slice_duration_ms = 0;
        assert!(p.validate().is_err());
    }

    #[test]
    fn zero_bandwidth_means_unlimited() {
        // 0 is the sentinel for "no rate limit" — the validator must accept it.
        let mut p = minimal_plan();
        p.total_bandwidth_bps = 0;
        assert!(p.validate().is_ok());
    }

    #[test]
    fn no_slices_rejected() {
        let mut p = minimal_plan();
        p.slices.clear();
        assert!(p.validate().is_err());
    }

    #[test]
    fn gap_in_slice_indices_rejected() {
        let mut p = minimal_plan();
        p.slices.push(SliceSpec {
            slice_index: 2, // gap: missing index 1
            total_connections: 20,
        });
        assert!(p.validate().is_err());
    }

    #[test]
    fn duplicate_bucket_id_rejected() {
        let mut p = minimal_plan();
        p.file_distribution.buckets.push(BucketSpec {
            bucket_id: "xs".into(), // duplicate
            size_min_bytes: 0,
            size_max_bytes: 1024,
            percentage: 0.0,
        });
        assert!(p.validate().is_err());
    }

    #[test]
    fn inverted_bucket_range_rejected() {
        let mut p = minimal_plan();
        p.file_distribution.buckets[0].size_min_bytes = 1024;
        p.file_distribution.buckets[0].size_max_bytes = 512;
        assert!(p.validate().is_err());
    }

    #[test]
    fn percentages_not_summing_to_one_rejected() {
        let mut p = minimal_plan();
        p.file_distribution.buckets[0].percentage = 0.5; // sum = 0.5, not 1.0
        assert!(p.validate().is_err());
    }

    #[test]
    fn to_proto_round_trip() {
        let plan = minimal_plan();
        let proto = plan.to_proto(3);
        assert_eq!(proto.plan_id, "test");
        assert_eq!(proto.slice_duration_ms, 60_000);
        assert_eq!(proto.total_agents, 3);
        assert_eq!(proto.slices.len(), 1);
        assert_eq!(proto.slices[0].slice_index, 0);
        assert_eq!(proto.slices[0].total_connections, 10);
        let dist = proto.file_distribution.unwrap();
        assert_eq!(dist.buckets.len(), 1);
        assert_eq!(dist.buckets[0].bucket_id, "xs");
    }

    #[test]
    fn full_distribution_valid() {
        // The exact distribution from the architecture plan.
        let plan = LoadPlanFile {
            plan_id: "reference".into(),
            slice_duration_ms: 60_000,
            total_bandwidth_bps: 104_857_600,
            file_distribution: FileDistributionSpec {
                buckets: vec![
                    BucketSpec { bucket_id: "xs".into(),    size_min_bytes: 0,           size_max_bytes: 10_485_760,  percentage: 0.580 },
                    BucketSpec { bucket_id: "sm".into(),    size_min_bytes: 10_485_760,  size_max_bytes: 52_428_800,  percentage: 0.129 },
                    BucketSpec { bucket_id: "md".into(),    size_min_bytes: 52_428_800,  size_max_bytes: 104_857_600, percentage: 0.087 },
                    BucketSpec { bucket_id: "lg".into(),    size_min_bytes: 104_857_600, size_max_bytes: 209_715_200, percentage: 0.063 },
                    BucketSpec { bucket_id: "xl".into(),    size_min_bytes: 209_715_200, size_max_bytes: 524_288_000, percentage: 0.052 },
                    BucketSpec { bucket_id: "xxl".into(),   size_min_bytes: 524_288_000, size_max_bytes: 1_073_741_824, percentage: 0.040 },
                    BucketSpec { bucket_id: "giant".into(), size_min_bytes: 1_073_741_824, size_max_bytes: 2_147_483_648, percentage: 0.049 },
                ],
            },
            slices: vec![
                SliceSpec { slice_index: 0, total_connections: 50  },
                SliceSpec { slice_index: 1, total_connections: 120 },
                SliceSpec { slice_index: 2, total_connections: 280 },
            ],
        };
        assert!(plan.validate().is_ok());
    }
}
