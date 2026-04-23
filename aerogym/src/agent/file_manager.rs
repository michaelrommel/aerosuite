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
/// If a file already exists and its size matches the desired random size it is
/// reused (idempotent across restarts within the same session).  If the file
/// exists but is the wrong size it is re-created.
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
        let target_bytes = random_size_in_bucket(bucket);

        let existing_size = fs::metadata(&path).await.ok().map(|m| m.len());

        if existing_size == Some(target_bytes) {
            info!(
                bucket = %bucket.bucket_id,
                bytes  = target_bytes,
                path   = %path.display(),
                "reusing existing bucket file"
            );
        } else {
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
        }

        map.insert(bucket.bucket_id.clone(), path);
    }

    Ok(map)
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

    #[tokio::test]
    async fn reuses_correct_existing_file() {
        let dir = tempdir();
        let buckets = vec![bucket("xs", 1024, 1025)]; // fixed size
        let map1 = generate(&dir, "a00", &buckets).await.unwrap();
        let mtime1 = tokio::fs::metadata(&map1["xs"]).await.unwrap().modified().unwrap();

        // Second call should reuse the file
        let map2 = generate(&dir, "a00", &buckets).await.unwrap();
        let mtime2 = tokio::fs::metadata(&map2["xs"]).await.unwrap().modified().unwrap();

        assert_eq!(mtime1, mtime2, "file should not be rewritten");
    }

    #[test]
    fn random_size_within_range() {
        let b = bucket("test", 1000, 2000);
        for _ in 0..100 {
            let s = random_size_in_bucket(&b);
            assert!((1000..2000).contains(&s));
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
}
