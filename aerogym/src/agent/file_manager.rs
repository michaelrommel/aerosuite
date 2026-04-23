//! Pre-generates one random-sized test file per file-size bucket.
//!
//! Each agent creates these files once at startup and reuses them for all
//! transfers.  The file for bucket `xs` on agent `a03` is named:
//!
//! ```text
//! /tmp/aerogym/bucket_a03_xs.dat
//! ```
//!
//! The size is chosen at random within the bucket's `[size_min_bytes,
//! size_max_bytes)` range so that files differ across agents while each agent
//! remains self-consistent across slices.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::{
    fs::{self, File},
    io::{AsyncWriteExt, BufWriter},
};
use tracing::info;

use aeroproto::aeromonitor::FileSizeBucket;

const WRITE_BUF_CAPACITY: usize = 256 * 1024;
const CHUNK_SIZE: usize = 8 * 1024;

/// Generate bucket files inside `work_dir`, returning a map from
/// `bucket_id → absolute file path`.
///
/// Before writing, checks whether a file already exists **and its size falls
/// within the bucket's `[size_min_bytes, size_max_bytes)` range**.  If so the
/// existing file is reused — no disk I/O needed.  This makes repeated test
/// runs fast because the large pre-generated files survive across registrations.
///
/// A file is regenerated when:
/// - it does not exist, or
/// - its size is 0, or
/// - its size is outside the bucket range (e.g. the plan changed).
pub async fn generate(
    work_dir: &Path,
    agent_id: &str,
    buckets: &[FileSizeBucket],
) -> Result<HashMap<String, PathBuf>> {
    fs::create_dir_all(work_dir)
        .await
        .with_context(|| format!("cannot create work dir {}", work_dir.display()))?;

    let mut map = HashMap::new();

    for bucket in buckets {
        let path = bucket_file_path(work_dir, agent_id, &bucket.bucket_id);

        let existing_size = fs::metadata(&path).await.ok().map(|m| m.len());

        if let Some(size) = existing_size {
            if size_in_bucket(size, bucket) {
                info!(
                    bucket = %bucket.bucket_id,
                    bytes  = size,
                    path   = %path.display(),
                    "reusing existing bucket file"
                );
                map.insert(bucket.bucket_id.clone(), path);
                continue;
            }
        }

        // File missing, empty, or outside the bucket range — (re)generate.
        let target_bytes = random_size_in_bucket(bucket);
        write_random_file(&path, target_bytes).await.with_context(|| {
            format!(
                "failed to create bucket file {} ({} bytes)",
                path.display(),
                target_bytes
            )
        })?;
        info!(
            bucket = %bucket.bucket_id,
            bytes  = target_bytes,
            path   = %path.display(),
            "bucket file created"
        );

        map.insert(bucket.bucket_id.clone(), path);
    }

    Ok(map)
}

/// Returns `true` when `size` is a valid file size for `bucket`.
///
/// Requires at least 1 byte (guards against empty files left by a previously
/// interrupted write) and that the size sits within `[size_min_bytes,
/// size_max_bytes)`.  When `size_min_bytes` is 0 the effective minimum is 1.
fn size_in_bucket(size: u64, bucket: &FileSizeBucket) -> bool {
    let min = bucket.size_min_bytes.max(1);
    let max = bucket.size_max_bytes;
    size >= min && size < max
}

/// Path to the bucket file for a given agent and bucket.
pub fn bucket_file_path(work_dir: &Path, agent_id: &str, bucket_id: &str) -> PathBuf {
    work_dir.join(format!("bucket_{agent_id}_{bucket_id}.dat"))
}

/// Choose a random file size within `[size_min_bytes, size_max_bytes)`.
fn random_size_in_bucket(bucket: &FileSizeBucket) -> u64 {
    let min = bucket.size_min_bytes;
    let max = bucket.size_max_bytes;
    if max <= min {
        return min.max(1);
    }
    min + (rand::random::<u64>() % (max - min))
}

/// Write `target_bytes` of random data to `path`, creating or overwriting it.
async fn write_random_file(path: &Path, target_bytes: u64) -> Result<()> {
    let file = File::create(path)
        .await
        .with_context(|| format!("cannot create {}", path.display()))?;

    let mut writer = BufWriter::with_capacity(WRITE_BUF_CAPACITY, file);
    let mut written = 0u64;
    let mut buf = vec![0u8; CHUNK_SIZE];

    while written < target_bytes {
        let remaining = target_bytes - written;
        let to_write = (remaining as usize).min(CHUNK_SIZE);
        rand::fill(&mut buf[..to_write]);
        writer
            .write_all(&buf[..to_write])
            .await
            .context("write error")?;
        written += to_write as u64;
    }

    writer.flush().await.context("flush error")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeroproto::aeromonitor::FileSizeBucket;

    fn bucket(id: &str, min: u64, max: u64) -> FileSizeBucket {
        FileSizeBucket {
            bucket_id: id.into(),
            size_min_bytes: min,
            size_max_bytes: max,
            percentage: 1.0,
        }
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "aerogym_test_{:016x}",
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn generates_files_for_all_buckets() {
        let dir = tempdir();
        let buckets = vec![
            bucket("xs", 1024, 2048),
            bucket("sm", 4096, 8192),
        ];
        let map = generate(&dir, "a00", &buckets).await.unwrap();
        assert_eq!(map.len(), 2);
        for (bucket_id, path) in &map {
            let meta = tokio::fs::metadata(path).await.unwrap();
            assert!(meta.len() > 0, "bucket {bucket_id} file is empty");
        }
    }

    /// A file whose size is within the bucket range must be reused — mtime
    /// must not change on the second call.
    #[tokio::test]
    async fn reuses_file_whose_size_is_in_range() {
        let dir = tempdir();
        // Range [1024, 1025) means the only valid size is exactly 1024.
        let buckets = vec![bucket("xs", 1024, 1025)];

        let map1 = generate(&dir, "a00", &buckets).await.unwrap();
        let mtime1 = tokio::fs::metadata(&map1["xs"]).await.unwrap().modified().unwrap();

        // Second call: file still has size 1024, which is in [1024, 1025) — reuse.
        let map2 = generate(&dir, "a00", &buckets).await.unwrap();
        let mtime2 = tokio::fs::metadata(&map2["xs"]).await.unwrap().modified().unwrap();

        assert_eq!(mtime1, mtime2, "file should not be rewritten when size is in range");
    }

    /// A file whose size is outside the bucket range must be regenerated.
    #[tokio::test]
    async fn regenerates_file_when_size_out_of_range() {
        let dir = tempdir();
        let path = bucket_file_path(&dir, "a00", "xs");

        // Write a file that is outside the new bucket range.
        tokio::fs::write(&path, vec![0u8; 512]).await.unwrap();

        // Bucket now requires [1024, 2048) — existing 512-byte file is too small.
        let buckets = vec![bucket("xs", 1024, 2048)];
        let map = generate(&dir, "a00", &buckets).await.unwrap();
        let new_size = tokio::fs::metadata(&map["xs"]).await.unwrap().len();

        assert!(
            new_size >= 1024 && new_size < 2048,
            "regenerated file size {new_size} should be within [1024, 2048)"
        );
    }

    /// A zero-byte file (e.g. interrupted write) must be regenerated.
    #[tokio::test]
    async fn regenerates_empty_file() {
        let dir = tempdir();
        let path = bucket_file_path(&dir, "a00", "xs");

        tokio::fs::write(&path, b"").await.unwrap();

        let buckets = vec![bucket("xs", 1024, 2048)];
        let map = generate(&dir, "a00", &buckets).await.unwrap();
        let new_size = tokio::fs::metadata(&map["xs"]).await.unwrap().len();

        assert!(new_size >= 1024, "empty file should have been replaced, got {new_size}");
    }

    #[test]
    fn size_in_bucket_accepts_valid_sizes() {
        let b = bucket("xs", 1000, 2000);
        assert!(size_in_bucket(1000, &b));
        assert!(size_in_bucket(1500, &b));
        assert!(size_in_bucket(1999, &b));
    }

    #[test]
    fn size_in_bucket_rejects_out_of_range() {
        let b = bucket("xs", 1000, 2000);
        assert!(!size_in_bucket(0, &b));
        assert!(!size_in_bucket(999, &b));
        assert!(!size_in_bucket(2000, &b));
        assert!(!size_in_bucket(9999, &b));
    }

    #[test]
    fn size_in_bucket_treats_zero_min_as_one() {
        let b = bucket("xs", 0, 1000);
        assert!(!size_in_bucket(0, &b), "zero-byte file must be rejected");
        assert!(size_in_bucket(1, &b));
        assert!(size_in_bucket(999, &b));
    }

    #[test]
    fn random_size_within_range() {
        let b = bucket("test", 1000, 2000);
        for _ in 0..100 {
            let s = random_size_in_bucket(&b);
            assert!((1000..2000).contains(&s));
        }
    }
}
