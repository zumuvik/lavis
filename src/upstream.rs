//! Minimal Git smart-HTTP `info/refs` client used by the `info` command to
//! resolve the upstream repository's `main` revision. Network access is
//! confined behind a trait so parsing and failure behavior stay deterministic
//! and testable without opening a socket.

use std::{future::Future, pin::Pin, time::Duration};

pub type UpstreamRevFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, UpstreamError>> + Send + 'a>>;

/// The deliberately narrow boundary for reading the upstream `main` revision.
/// Tests can inject a fake without ever constructing the real request URL.
pub trait UpstreamRev: Send + Sync {
    fn main_rev<'a>(&'a self) -> UpstreamRevFuture<'a>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamError {
    Transport,
    Timeout,
    NoMainRef,
}

const UPSTREAM_INFO_REFS_URL: &str =
    "https://tangled.org/zumuvik.tngl.sh/lavis/info/refs?service=git-upload-pack";
const SHA1_HEX_LEN: usize = 40;
const MAX_INFO_REFS_BODY_BYTES: usize = 64 * 1024;
const UPSTREAM_FETCH_TIMEOUT: Duration = Duration::from_secs(2);

/// Uses Rustls only (via reqwest's `rustls-tls` feature).
pub struct HttpUpstreamRev {
    client: reqwest::Client,
}

impl HttpUpstreamRev {
    pub fn new() -> Result<Self, UpstreamError> {
        reqwest::Client::builder()
            .https_only(true)
            .timeout(UPSTREAM_FETCH_TIMEOUT)
            .build()
            .map(|client| Self { client })
            .map_err(|_| UpstreamError::Transport)
    }
}

/// Extracts the advertised `main` revision from a Git smart-HTTP `info/refs`
/// response body. The body is a sequence of pkt-lines: each starts with a
/// 4-hex self-inclusive length (flush packets use `0000`). Only a line whose
/// ref name is exactly `refs/heads/main` with a 40-hex SHA-1 revision counts;
/// the `symref=HEAD:refs/heads/main` capability on the HEAD line is skipped
/// because its name is not `refs/heads/main`, and SHA-256 repositories (64-hex
/// revisions) are rejected.
pub fn parse_main_rev_from_info_refs(body: &[u8]) -> Result<String, UpstreamError> {
    let mut offset = 0usize;
    while offset < body.len() {
        if body.len() - offset < 4 {
            return Err(UpstreamError::NoMainRef);
        }
        let length = std::str::from_utf8(&body[offset..offset + 4])
            .ok()
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .ok_or(UpstreamError::NoMainRef)?;
        if length == 0 {
            offset += 4;
            continue;
        }
        let length = length as usize;
        if length < 4 || offset + length > body.len() {
            return Err(UpstreamError::NoMainRef);
        }
        let payload = &body[offset + 4..offset + length];
        offset += length;
        let payload = payload.strip_suffix(b"\n").unwrap_or(payload);
        let Some(space) = payload.iter().position(|byte| *byte == b' ') else {
            continue;
        };
        let (revision, name) = payload.split_at(space);
        if name != b" refs/heads/main" {
            continue;
        }
        if revision.len() != SHA1_HEX_LEN || !revision.iter().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(UpstreamError::NoMainRef);
        }
        return String::from_utf8(revision.to_vec()).map_err(|_| UpstreamError::NoMainRef);
    }
    Err(UpstreamError::NoMainRef)
}

fn request_error(error: reqwest::Error) -> UpstreamError {
    if error.is_timeout() {
        UpstreamError::Timeout
    } else {
        UpstreamError::Transport
    }
}

async fn read_bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, UpstreamError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_INFO_REFS_BODY_BYTES as u64)
    {
        return Err(UpstreamError::Transport);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| UpstreamError::Transport)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_INFO_REFS_BODY_BYTES {
            return Err(UpstreamError::Transport);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl UpstreamRev for HttpUpstreamRev {
    fn main_rev<'a>(&'a self) -> UpstreamRevFuture<'a> {
        Box::pin(async move {
            let response = self
                .client
                .get(UPSTREAM_INFO_REFS_URL)
                .send()
                .await
                .map_err(request_error)?;
            if !response.status().is_success() {
                return Err(UpstreamError::Transport);
            }
            let body = read_bounded_body(response).await?;
            parse_main_rev_from_info_refs(&body)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mock {
        result: Result<String, UpstreamError>,
    }

    impl UpstreamRev for Mock {
        fn main_rev<'a>(&'a self) -> UpstreamRevFuture<'a> {
            let result = self.result.clone();
            Box::pin(async move { result })
        }
    }

    #[tokio::test]
    async fn typed_mock_resolves_without_network() {
        let mock = Mock {
            result: Ok("b1d18f8ef407d043506c983b0d68e96c282eb1c9".into()),
        };
        assert_eq!(
            mock.main_rev().await.unwrap(),
            "b1d18f8ef407d043506c983b0d68e96c282eb1c9"
        );
    }

    #[tokio::test]
    async fn typed_mock_error_propagates_without_network() {
        let mock = Mock {
            result: Err(UpstreamError::Timeout),
        };
        assert_eq!(mock.main_rev().await, Err(UpstreamError::Timeout));
    }

    /// Byte-for-byte shape of the real Tangled `info/refs` response
    /// (captured 2026-08-20): service line, flush, HEAD capability line
    /// carrying `symref=HEAD:refs/heads/main`, feature refs, then main.
    #[test]
    fn parses_main_rev_from_real_response_shape() {
        let body = b"001e# service=git-upload-pack\n\
0000\
00c8b1d18f8ef407d043506c983b0d68e96c282eb1c9 HEAD\x00multi_ack_detailed no-done side-band-64k ofs-delta shallow deepen-since deepen-not filter agent=knot/0 object-format=sha1 symref=HEAD:refs/heads/main\n\
00539179c4e9ef46807ce59f96e88dfaab476686f7dc refs/heads/feat/auth-session-recovery\n\
004ae6d6be83fec61eca0c920ba5a5b254c08c31b5c3 refs/heads/feat/info-command\n\
003db1d18f8ef407d043506c983b0d68e96c282eb1c9 refs/heads/main\n\
0000";
        assert_eq!(
            parse_main_rev_from_info_refs(body).unwrap(),
            "b1d18f8ef407d043506c983b0d68e96c282eb1c9"
        );
    }

    #[test]
    fn symref_capability_on_head_line_is_ignored() {
        let body = b"001e# service=git-upload-pack\n\
0000\
004eb1d18f8ef407d043506c983b0d68e96c282eb1c9 HEAD\x00symref=HEAD:refs/heads/main\n\
003da1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b1 refs/heads/main\n\
0000";
        assert_eq!(
            parse_main_rev_from_info_refs(body).unwrap(),
            "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b1"
        );
    }

    #[test]
    fn missing_main_ref_is_no_main_ref() {
        assert_eq!(
            parse_main_rev_from_info_refs(b"001e# service=git-upload-pack\n0000"),
            Err(UpstreamError::NoMainRef)
        );
        assert_eq!(
            parse_main_rev_from_info_refs(b""),
            Err(UpstreamError::NoMainRef)
        );
    }

    #[test]
    fn sha256_style_revision_is_rejected() {
        let body = b"001e# service=git-upload-pack\n\
0000\
0056a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2a1b2 refs/heads/main\n\
0000";
        assert_eq!(
            parse_main_rev_from_info_refs(body),
            Err(UpstreamError::NoMainRef)
        );
    }

    #[test]
    fn malformed_length_prefix_is_rejected() {
        assert_eq!(
            parse_main_rev_from_info_refs(
                b"zzzzb1d18f8ef407d043506c983b0d68e96c282eb1c9 refs/heads/main\n"
            ),
            Err(UpstreamError::NoMainRef)
        );
        assert_eq!(
            parse_main_rev_from_info_refs(b"003d"),
            Err(UpstreamError::NoMainRef)
        );
    }
}
