//! Reading the request body with a configurable in-memory cap.
//!
//! The recorder must decode (and possibly rewrite) a request body before forwarding it, so the body
//! is buffered — but only up to `limits.request_body_bytes` (4 MiB by default). Above the cap the
//! policy decides what happens:
//!
//! * `spill`  — everything is written to a file as it arrives and forwarded from that file, so
//!              memory stays bounded and nothing is lost (default);
//! * `reject` — answer `413` immediately;
//! * `stream` — the caller forwards the body without buffering it (no decode/rewrite then).
//!
//! The first bytes are always kept in `head` so a summary can still be produced for spilled bodies.

use std::path::{Path, PathBuf};

use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How much of a spilled body is still kept in memory for summaries.
pub const HEAD_KEEP: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Spill,
    Reject,
    Stream,
}

impl Policy {
    pub fn parse(value: &str) -> Option<Policy> {
        match value.trim() {
            "spill" => Some(Policy::Spill),
            "reject" => Some(Policy::Reject),
            "stream" => Some(Policy::Stream),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct Buffered {
    /// First bytes of the body (all of it when it fit into the cap).
    pub head: Vec<u8>,
    /// Set when the body was spilled to a file; the file holds the **whole** body.
    pub spill: Option<PathBuf>,
    pub total: u64,
    pub over_limit: bool,
}

#[derive(Debug)]
pub enum BodyError {
    TooLarge(usize),
    Read(String),
    Io(String),
}

impl BodyError {
    pub fn message(&self) -> String {
        match self {
            BodyError::TooLarge(limit) => {
                format!("request body exceeds limits.request_body_bytes ({limit} bytes)")
            }
            BodyError::Read(e) => format!("body read error: {e}"),
            BodyError::Io(e) => format!("body spill error: {e}"),
        }
    }
}

/// Reads the whole body, spilling to `spill_path` when it grows past `limit`.
pub async fn read(
    body: Body,
    limit: usize,
    policy: Policy,
    spill_path: &Path,
) -> Result<Buffered, BodyError> {
    let mut stream = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut file: Option<tokio::fs::File> = None;
    let mut total: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| BodyError::Read(e.to_string()))?;
        total += chunk.len() as u64;

        if file.is_none() && buf.len() + chunk.len() > limit {
            match policy {
                Policy::Reject => return Err(BodyError::TooLarge(limit)),
                Policy::Spill | Policy::Stream => {
                    if let Some(parent) = spill_path.parent() {
                        let _ = tokio::fs::create_dir_all(parent).await;
                    }
                    let mut f = tokio::fs::File::create(spill_path)
                        .await
                        .map_err(|e| BodyError::Io(e.to_string()))?;
                    f.write_all(&buf)
                        .await
                        .map_err(|e| BodyError::Io(e.to_string()))?;
                    f.write_all(&chunk)
                        .await
                        .map_err(|e| BodyError::Io(e.to_string()))?;
                    file = Some(f);
                    // keep only a bounded prefix in memory
                    if buf.len() > HEAD_KEEP {
                        buf.truncate(HEAD_KEEP);
                    }
                    continue;
                }
            }
        }

        match file.as_mut() {
            Some(f) => {
                f.write_all(&chunk)
                    .await
                    .map_err(|e| BodyError::Io(e.to_string()))?;
            }
            None => {
                buf.extend_from_slice(&chunk);
                if buf.len() > HEAD_KEEP {
                    // keep the head bounded even while everything still fits in one buffer
                    let _ = buf.len();
                }
            }
        }
    }

    if let Some(mut f) = file {
        let _ = f.flush().await;
        return Ok(Buffered {
            head: buf,
            spill: Some(spill_path.to_path_buf()),
            total,
            over_limit: true,
        });
    }
    Ok(Buffered {
        head: buf,
        spill: None,
        total,
        over_limit: false,
    })
}

/// Streams a spilled file as a request body without loading it into memory.
pub fn file_body(path: PathBuf) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    enum State {
        Opening(PathBuf),
        Reading(tokio::fs::File),
    }
    futures_util::stream::try_unfold(State::Opening(path), |state| async move {
        let mut file = match state {
            State::Opening(path) => tokio::fs::File::open(path).await?,
            State::Reading(file) => file,
        };
        let mut buf = vec![0u8; 64 * 1024];
        let n = file.read(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.truncate(n);
        Ok(Some((Bytes::from(buf), State::Reading(file))))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read_with(chunks: Vec<&'static [u8]>, limit: usize, policy: Policy) -> Result<Buffered, BodyError> {
        static SPILL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let stream = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, std::io::Error>(Bytes::from_static(c))),
        );
        let body = Body::from_stream(stream);
        // unique per call: cargo runs these tests in parallel
        let path = std::env::temp_dir().join(format!(
            "codex-rec-test-spill-{}-{}.bin",
            std::process::id(),
            SPILL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        read(body, limit, policy, &path).await
    }

    #[tokio::test]
    async fn under_the_limit_stays_in_memory() {
        let out = read_with(vec![b"hello ", b"world"], 1024, Policy::Spill).await.unwrap();
        assert!(!out.over_limit);
        assert!(out.spill.is_none());
        assert_eq!(out.total, 11);
        assert_eq!(&out.head, b"hello world");
    }

    #[tokio::test]
    async fn exactly_at_the_limit_is_not_over() {
        let out = read_with(vec![b"12345678"], 8, Policy::Spill).await.unwrap();
        assert!(!out.over_limit);
        assert_eq!(&out.head, b"12345678");
    }

    #[tokio::test]
    async fn one_byte_over_the_limit_spills_and_keeps_everything() {
        let out = read_with(vec![b"12345678", b"9"], 8, Policy::Spill).await.unwrap();
        assert!(out.over_limit);
        let spill = out.spill.expect("spilled");
        let bytes = std::fs::read(&spill).unwrap();
        assert_eq!(bytes, b"123456789");
        assert_eq!(out.total, 9);
        let _ = std::fs::remove_file(&spill);
    }

    #[tokio::test]
    async fn one_chunk_over_the_limit_spills_everything() {
        let out = read_with(vec![b"0123456789abcdef"], 8, Policy::Spill).await.unwrap();
        assert!(out.over_limit);
        let spill = out.spill.expect("spilled");
        assert_eq!(std::fs::read(&spill).unwrap(), b"0123456789abcdef");
        let _ = std::fs::remove_file(&spill);
    }

    #[tokio::test]
    async fn empty_body_is_fine() {
        let out = read_with(vec![], 8, Policy::Spill).await.unwrap();
        assert_eq!(out.total, 0);
        assert!(!out.over_limit);
        assert!(out.head.is_empty());
    }

    #[tokio::test]
    async fn limit_zero_spills_anything_non_empty() {
        let out = read_with(vec![b"x"], 0, Policy::Spill).await.unwrap();
        assert!(out.over_limit);
        let spill = out.spill.unwrap();
        assert_eq!(std::fs::read(&spill).unwrap(), b"x");
        let _ = std::fs::remove_file(&spill);
    }

    #[tokio::test]
    async fn reject_policy_refuses_and_spill_policy_is_default() {
        let err = read_with(vec![b"123456789"], 8, Policy::Reject).await.unwrap_err();
        assert!(matches!(err, BodyError::TooLarge(8)));
        assert_eq!(Policy::parse("spill"), Some(Policy::Spill));
        assert_eq!(Policy::parse("reject"), Some(Policy::Reject));
        assert_eq!(Policy::parse("stream"), Some(Policy::Stream));
        assert_eq!(Policy::parse("nonsense"), None);
    }

    #[tokio::test]
    async fn spilled_files_stream_back_out() {
        let out = read_with(vec![b"abcdefghij"], 4, Policy::Spill).await.unwrap();
        let spill = out.spill.unwrap();
        let mut stream = std::pin::pin!(file_body(spill.clone()));
        let mut got = Vec::new();
        while let Some(chunk) = stream.next().await {
            got.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(got, b"abcdefghij");
        let _ = std::fs::remove_file(&spill);
    }
}
