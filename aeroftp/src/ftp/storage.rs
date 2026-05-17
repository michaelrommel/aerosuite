//! Storage backend wrapper: S3 object metadata attachment.
//!
//! [`AeroStorage`] implements libunftp's [`StorageBackend`] for [`AeroUser`].
//! All operations except `put` are delegated straight to the inner
//! [`OpendalStorage`].  For `put`, the wrapper:
//!
//! 1. Opens an opendal writer **with** the device's system metadata attached
//!    as S3 user-defined object metadata (`x-amz-meta-*` headers), so every
//!    uploaded object carries the device identity for auditing and downstream
//!    processing.
//! 2. Streams the incoming FTP data through to S3 on the original path
//!    (no remapping yet — Phase 3 will add path construction).
//! 3. Emits a `ftp.stor.s3_put` tracing span searchable in Grafana Tempo.
//!
//! Quarantined devices (unknown to Redis) are accepted normally; their uploads
//! simply carry empty metadata since `AeroUser::system_meta` is empty.
//! Phase 3 will route them to a separate `unknown/` prefix.
//!
//! The bare [`Operator`] is held alongside [`OpendalStorage`] so that
//! `writer_with().user_metadata()` — which is not exposed through the
//! `StorageBackend` trait — can be called directly.

use std::fmt::Debug;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use opendal::Operator;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio_util::compat::FuturesAsyncWriteCompatExt;
use tracing::{info_span, Instrument};
use unftp_core::storage::{Error, ErrorKind, Fileinfo, Metadata, Result, StorageBackend};
use unftp_sbe_opendal::OpendalStorage;

use super::user::AeroUser;

/// Same fix as `unftp-sbe-opendal` commit b3a6260: avoids `tokio::io::copy`'s
/// pending-read flush path.
///
/// When the FTP client sends data slowly or in small fragments, `tokio::io::copy`
/// calls `flush()` on the opendal writer every time the socket goes momentarily
/// idle (`Poll::Pending`).  Each `flush()` submits whatever partial data is
/// buffered as a multipart upload part and opens a fresh buffer, causing many
/// small concurrent in-flight parts instead of one clean 5 MB part.  For
/// rate-limited or long-running connections this multiplies write-buffer memory
/// well beyond the expected 15 MB per connection.
///
/// The custom loop calls only `write_all` — no implicit flushes — so the
/// opendal writer accumulates data to the 5 MB chunk threshold before
/// submitting a part.
async fn copy_read_write_loop<R, W>(input: &mut R, output: &mut W) -> std::io::Result<u64>
where
    R: AsyncRead + Unpin + ?Sized,
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    let mut copied = 0u64;
    let mut buf = [0u8; 8 * 1024];
    loop {
        let n = input.read(&mut buf).await?;
        if n == 0 {
            return Ok(copied);
        }
        output.write_all(&buf[..n]).await?;
        copied += n as u64;
    }
}

/// Wraps [`OpendalStorage`] and attaches S3 user metadata on every STOR.
#[derive(Debug, Clone)]
pub struct AeroStorage {
    /// Used for all operations other than `put`.
    inner: OpendalStorage,
    /// Used for `put` so we can call `writer_with().user_metadata()`.
    op: Operator,
}

impl AeroStorage {
    /// Construct from the same `Operator` used to build `inner`.
    ///
    /// Pass `op.clone()` when constructing both `OpendalStorage` and this
    /// wrapper — they share the same underlying connection pool.
    pub fn new(inner: OpendalStorage, op: Operator) -> Self {
        Self { inner, op }
    }
}

#[async_trait]
impl StorageBackend<AeroUser> for AeroStorage {
    type Metadata = <OpendalStorage as StorageBackend<AeroUser>>::Metadata;

    async fn metadata<P: AsRef<Path> + Send + Debug>(
        &self,
        user: &AeroUser,
        path: P,
    ) -> Result<Self::Metadata> {
        self.inner.metadata(user, path).await
    }

    async fn list<P: AsRef<Path> + Send + Debug>(
        &self,
        user: &AeroUser,
        path: P,
    ) -> Result<Vec<Fileinfo<PathBuf, Self::Metadata>>>
    where
        Self::Metadata: Metadata,
    {
        self.inner.list(user, path).await
    }

    async fn get<P: AsRef<Path> + Send + Debug>(
        &self,
        user: &AeroUser,
        path: P,
        start_pos: u64,
    ) -> Result<Box<dyn tokio::io::AsyncRead + Send + Sync + Unpin>> {
        self.inner.get(user, path, start_pos).await
    }

    async fn put<P, R>(
        &self,
        user: &AeroUser,
        mut input: R,
        path: P,
        _start_pos: u64,
    ) -> Result<u64>
    where
        P: AsRef<Path> + Send + Debug,
        R: AsyncRead + Send + Sync + Unpin + 'static,
    {
        let ftp_path = path.as_ref().to_string_lossy().into_owned();

        // Phase 1: write to the original path unchanged.
        // Phase 3 will construct the canonical S3 key here.
        let s3_path = ftp_path.clone();

        let span = info_span!(
            "ftp.stor.s3_put",
            ftp.path         = %ftp_path,
            ftp.client_ip    = %user.client_ip,
            ftp.username     = %user.username,
            ftp.quarantined  = user.quarantined,
            ftp.modality     = user.system_meta.get("modality").map(|s| s.as_str()).unwrap_or("?"),
            ftp.serial       = user.system_meta.get("serial").map(|s| s.as_str()).unwrap_or("?"),
        );

        async move {
            let mut writer = self
                .op
                .writer_with(&s3_path)
                .user_metadata(user.system_meta.clone())
                .await
                .map_err(|e| Error::new(ErrorKind::LocalError, e.to_string()))?
                .into_futures_async_write()
                .compat_write();

            let copy_result    = copy_read_write_loop(&mut input, &mut writer).await;
            let shutdown_result = writer.shutdown().await;
            match (copy_result, shutdown_result) {
                (Ok(bytes), Ok(())) => Ok(bytes),
                (Err(e), Ok(())) => Err(Error::new(ErrorKind::LocalError,
                    format!("copy failed: {e}"))),
                (Ok(_), Err(e)) => Err(Error::new(ErrorKind::LocalError,
                    format!("shutdown failed: {e}"))),
                (Err(ce), Err(se)) => Err(Error::new(ErrorKind::LocalError,
                    format!("copy failed: {ce}; shutdown failed: {se}"))),
            }
        }
        .instrument(span)
        .await
    }

    async fn del<P: AsRef<Path> + Send + Debug>(
        &self,
        user: &AeroUser,
        path: P,
    ) -> Result<()> {
        self.inner.del(user, path).await
    }

    async fn mkd<P: AsRef<Path> + Send + Debug>(
        &self,
        user: &AeroUser,
        path: P,
    ) -> Result<()> {
        self.inner.mkd(user, path).await
    }

    async fn rename<P: AsRef<Path> + Send + Debug>(
        &self,
        user: &AeroUser,
        from: P,
        to: P,
    ) -> Result<()> {
        self.inner.rename(user, from, to).await
    }

    async fn rmd<P: AsRef<Path> + Send + Debug>(
        &self,
        user: &AeroUser,
        path: P,
    ) -> Result<()> {
        self.inner.rmd(user, path).await
    }

    async fn cwd<P: AsRef<Path> + Send + Debug>(
        &self,
        user: &AeroUser,
        path: P,
    ) -> Result<()> {
        self.inner.cwd(user, path).await
    }
}
