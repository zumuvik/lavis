//! Minimal Git smart-HTTP `info/refs` client used by the `info` command to
//! resolve the upstream repository's `main` revision. Network access is
//! confined behind a trait so parsing and failure behavior stay deterministic
//! and testable without opening a socket.

use std::{future::Future, pin::Pin, time::Duration};

pub type UpstreamRevFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, UpstreamError>> + Send + 'a>>;

pub type CompareFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompareResult, UpstreamError>> + Send + 'a>>;

/// The deliberately narrow boundary for reading the upstream `main` revision
/// and comparing two revisions. Tests can inject a fake without ever
/// constructing the real request URL.
pub trait UpstreamRev: Send + Sync {
    fn main_rev<'a>(&'a self) -> UpstreamRevFuture<'a>;
    fn compare<'a>(&'a self, base: &'a str, head: &'a str) -> CompareFuture<'a>;
}

/// Outcome of comparing two revisions: how many commits `head` is ahead of
/// and/or behind `base`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompareResult {
    pub ahead_by: u64,
    pub behind_by: u64,
}

/// How the current build relates to the upstream `main` branch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevisionRelation {
    Current,
    Ahead { commits: u64 },
    Behind { commits: u64 },
    Diverged { ahead: u64, behind: u64 },
    Unavailable,
}

/// A resolved upstream revision together with its relation to the local build.
#[derive(Clone)]
pub struct UpstreamRevision {
    pub revision: String,
    pub relation: RevisionRelation,
}

/// Derives the [`RevisionRelation`] from raw ahead/behind counts.
pub fn relation_from_compare(compare: &CompareResult) -> RevisionRelation {
    match (compare.ahead_by, compare.behind_by) {
        (0, 0) => RevisionRelation::Current,
        (ahead, 0) => RevisionRelation::Ahead { commits: ahead },
        (0, behind) => RevisionRelation::Behind { commits: behind },
        (ahead, behind) => RevisionRelation::Diverged { ahead, behind },
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpstreamError {
    Transport,
    Timeout,
    HttpStatus(u16),
    RevisionNotFound { revision: String },
    RepoNotFound,
    InvalidResponse,
    NoMainRef,
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport => write!(f, "transport"),
            Self::Timeout => write!(f, "timeout"),
            Self::HttpStatus(code) => write!(f, "http_status_{}", code),
            Self::RevisionNotFound { revision } => {
                write!(f, "revision_not_found({})", revision)
            }
            Self::RepoNotFound => write!(f, "repo_not_found"),
            Self::InvalidResponse => write!(f, "invalid_response"),
            Self::NoMainRef => write!(f, "no_main_ref"),
        }
    }
}

const UPSTREAM_INFO_REFS_URL: &str =
    "https://tangled.org/zumuvik.tngl.sh/lavis/info/refs?service=git-upload-pack";
const TANGLED_COMPARE_URL: &str = "https://api.tangled.org/xrpc/sh.tangled.repo.compare";
const TANGLED_COMPARE_REPO: &str = "at://did:plc:trc7yr7p6ikl5fxfupm5mia2/sh.tangled.repo/lavis";
const SHA1_HEX_LEN: usize = 40;
const MAX_INFO_REFS_BODY_BYTES: usize = 64 * 1024;
const UPSTREAM_FETCH_TIMEOUT: Duration = Duration::from_secs(3);

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
            return Err(UpstreamError::InvalidResponse);
        }
        let length = std::str::from_utf8(&body[offset..offset + 4])
            .ok()
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .ok_or(UpstreamError::InvalidResponse)?;
        if length == 0 {
            offset += 4;
            continue;
        }
        let length = length as usize;
        if length < 4 || offset + length > body.len() {
            return Err(UpstreamError::InvalidResponse);
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
            return Err(UpstreamError::InvalidResponse);
        }
        return String::from_utf8(revision.to_vec()).map_err(|_| UpstreamError::InvalidResponse);
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
            let status = response.status();
            if !status.is_success() {
                return Err(UpstreamError::HttpStatus(status.as_u16()));
            }
            let body = read_bounded_body(response).await?;
            parse_main_rev_from_info_refs(&body)
        })
    }

    fn compare<'a>(&'a self, base: &'a str, head: &'a str) -> CompareFuture<'a> {
        Box::pin(async move {
            match self.compare_tangled(base, head).await {
                Ok(result) => Ok(result),
                Err(UpstreamError::RevisionNotFound { .. }) => {
                    // Current commit not on Tangled — expected for unpublished commits.
                    tracing::info!(
                        event = "upstream_revision_not_on_tangled",
                        base,
                        head,
                        "Current commit not found on Tangled, marking as unavailable"
                    );
                    Err(UpstreamError::RevisionNotFound {
                        revision: head.to_string(),
                    })
                }
                Err(error) => {
                    tracing::warn!(
                        event = "tangled_compare_failed",
                        %error,
                        base,
                        head,
                        "Tangled compare failed"
                    );
                    Err(error)
                }
            }
        })
    }
}

impl HttpUpstreamRev {
    /// Tangled compare: two calls with swapped rev1/rev2 to derive ahead/behind
    /// from the `format_patch` array length in each direction.
    async fn compare_tangled(
        &self,
        base: &str,
        head: &str,
    ) -> Result<CompareResult, UpstreamError> {
        let ahead_by = self.tangled_patch_count(head, base).await?;
        let behind_by = self.tangled_patch_count(base, head).await?;
        Ok(CompareResult {
            ahead_by,
            behind_by,
        })
    }

    /// Returns the `format_patch` array length for a single Tangled compare
    /// call with `rev1=from` and `rev2=to`.
    async fn tangled_patch_count(&self, from: &str, to: &str) -> Result<u64, UpstreamError> {
        let response = self
            .client
            .get(TANGLED_COMPARE_URL)
            .query(&[("repo", TANGLED_COMPARE_REPO), ("rev1", from), ("rev2", to)])
            .send()
            .await
            .map_err(request_error)?;

        let status = response.status();
        let body = read_bounded_body(response).await?;

        if !status.is_success() {
            return parse_tangled_error_response(status.as_u16(), &body, from, to);
        }

        parse_tangled_compare_response(&body)
    }
}

/// Parses the Tangled compare JSON response, returning the `format_patch`
/// array length.
fn parse_tangled_compare_response(body: &[u8]) -> Result<u64, UpstreamError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| UpstreamError::InvalidResponse)?;
    let patches = value
        .get("format_patch")
        .and_then(|v| v.as_array())
        .ok_or(UpstreamError::InvalidResponse)?;
    Ok(patches.len() as u64)
}

/// Parses a non-success Tangled compare response body, extracting structured
/// error names (`RevisionNotFound`, `RepoNotFound`) when present. Falls back
/// to a plain `HttpStatus` error when the body is not JSON or the error name
/// is unrecognized.
fn parse_tangled_error_response(
    status: u16,
    body: &[u8],
    from: &str,
    to: &str,
) -> Result<u64, UpstreamError> {
    if let Ok(error_body) = serde_json::from_slice::<serde_json::Value>(body)
        && let Some(error_name) = error_body.get("error").and_then(|e| e.as_str())
    {
        match error_name {
            "RevisionNotFound" => {
                // Determine which revision was not found by checking
                // whether the message mentions `from` (otherwise assume `to`).
                let revision = if error_body
                    .get("message")
                    .and_then(|m| m.as_str())
                    .is_some_and(|m| m.contains(from))
                {
                    from.to_string()
                } else {
                    to.to_string()
                };
                tracing::debug!(
                    event = "tangled_revision_not_found",
                    status,
                    revision = %revision,
                    from,
                    to,
                    "Tangled reported RevisionNotFound"
                );
                return Err(UpstreamError::RevisionNotFound { revision });
            }
            "RepoNotFound" => {
                tracing::debug!(
                    event = "tangled_repo_not_found",
                    status,
                    "Tangled reported RepoNotFound"
                );
                return Err(UpstreamError::RepoNotFound);
            }
            _ => {
                tracing::debug!(
                    event = "tangled_error_response",
                    status,
                    error_name,
                    from,
                    to,
                    "Tangled returned an unrecognized error name"
                );
            }
        }
    }
    tracing::debug!(
        event = "tangled_http_error",
        status,
        from,
        to,
        "Tangled compare returned a non-success status"
    );
    Err(UpstreamError::HttpStatus(status))
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

        fn compare<'a>(&'a self, _base: &'a str, _head: &'a str) -> CompareFuture<'a> {
            Box::pin(async { Err(UpstreamError::Transport) })
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
            Err(UpstreamError::InvalidResponse)
        );
    }

    #[test]
    fn malformed_length_prefix_is_rejected() {
        assert_eq!(
            parse_main_rev_from_info_refs(
                b"zzzzb1d18f8ef407d043506c983b0d68e96c282eb1c9 refs/heads/main\n"
            ),
            Err(UpstreamError::InvalidResponse)
        );
        assert_eq!(
            parse_main_rev_from_info_refs(b"003d"),
            Err(UpstreamError::InvalidResponse)
        );
    }

    #[test]
    fn revision_relation_variants_are_distinct() {
        assert_eq!(RevisionRelation::Current, RevisionRelation::Current);
        assert_eq!(
            RevisionRelation::Ahead { commits: 3 },
            RevisionRelation::Ahead { commits: 3 }
        );
        assert_ne!(
            RevisionRelation::Ahead { commits: 3 },
            RevisionRelation::Ahead { commits: 4 }
        );
        assert_ne!(
            RevisionRelation::Ahead { commits: 1 },
            RevisionRelation::Behind { commits: 1 }
        );
        assert_ne!(RevisionRelation::Current, RevisionRelation::Unavailable);
    }

    #[test]
    fn relation_from_compare_maps_all_cases() {
        assert_eq!(
            relation_from_compare(&CompareResult {
                ahead_by: 0,
                behind_by: 0
            }),
            RevisionRelation::Current
        );
        assert_eq!(
            relation_from_compare(&CompareResult {
                ahead_by: 5,
                behind_by: 0
            }),
            RevisionRelation::Ahead { commits: 5 }
        );
        assert_eq!(
            relation_from_compare(&CompareResult {
                ahead_by: 0,
                behind_by: 3
            }),
            RevisionRelation::Behind { commits: 3 }
        );
        assert_eq!(
            relation_from_compare(&CompareResult {
                ahead_by: 2,
                behind_by: 4
            }),
            RevisionRelation::Diverged {
                ahead: 2,
                behind: 4
            }
        );
    }

    #[test]
    fn parses_tangled_compare_response() {
        let body = br#"{"format_patch":["patch1","patch2","patch3"]}"#;
        assert_eq!(parse_tangled_compare_response(body).unwrap(), 3);
    }

    #[test]
    fn parses_tangled_compare_empty_patches() {
        let body = br#"{"format_patch":[]}"#;
        assert_eq!(parse_tangled_compare_response(body).unwrap(), 0);
    }

    #[test]
    fn tangled_compare_missing_field_is_invalid() {
        assert_eq!(
            parse_tangled_compare_response(b"{}"),
            Err(UpstreamError::InvalidResponse)
        );
        assert_eq!(
            parse_tangled_compare_response(b"not json"),
            Err(UpstreamError::InvalidResponse)
        );
    }

    #[tokio::test]
    async fn mock_compare_returns_transport_error() {
        let mock = Mock {
            result: Ok("abc123".into()),
        };
        assert_eq!(
            mock.compare("base", "head").await,
            Err(UpstreamError::Transport)
        );
    }

    #[test]
    fn parses_revision_not_found_error_with_from_revision() {
        let body = br#"{"error":"RevisionNotFound","message":"revision abc123 not found"}"#;
        let result = parse_tangled_error_response(400, body, "abc123", "def456");
        assert_eq!(
            result,
            Err(UpstreamError::RevisionNotFound {
                revision: "abc123".to_string()
            })
        );
    }

    #[test]
    fn parses_revision_not_found_error_defaults_to_to_revision() {
        let body = br#"{"error":"RevisionNotFound","message":"revision not found"}"#;
        let result = parse_tangled_error_response(400, body, "abc123", "def456");
        assert_eq!(
            result,
            Err(UpstreamError::RevisionNotFound {
                revision: "def456".to_string()
            })
        );
    }

    #[test]
    fn parses_revision_not_found_error_without_message() {
        let body = br#"{"error":"RevisionNotFound"}"#;
        let result = parse_tangled_error_response(400, body, "abc123", "def456");
        assert_eq!(
            result,
            Err(UpstreamError::RevisionNotFound {
                revision: "def456".to_string()
            })
        );
    }

    #[test]
    fn parses_repo_not_found_error() {
        let body = br#"{"error":"RepoNotFound","message":"repo does not exist"}"#;
        let result = parse_tangled_error_response(400, body, "abc123", "def456");
        assert_eq!(result, Err(UpstreamError::RepoNotFound));
    }

    #[test]
    fn unrecognized_error_name_falls_back_to_http_status() {
        let body = br#"{"error":"SomeNewError","message":"something"}"#;
        let result = parse_tangled_error_response(500, body, "abc123", "def456");
        assert_eq!(result, Err(UpstreamError::HttpStatus(500)));
    }

    #[test]
    fn non_json_error_body_falls_back_to_http_status() {
        let body = b"Internal Server Error";
        let result = parse_tangled_error_response(500, body, "abc123", "def456");
        assert_eq!(result, Err(UpstreamError::HttpStatus(500)));
    }

    #[test]
    fn upstream_error_display_variants() {
        assert_eq!(UpstreamError::Transport.to_string(), "transport");
        assert_eq!(UpstreamError::Timeout.to_string(), "timeout");
        assert_eq!(
            UpstreamError::HttpStatus(404).to_string(),
            "http_status_404"
        );
        assert_eq!(
            UpstreamError::RevisionNotFound {
                revision: "abc123".to_string()
            }
            .to_string(),
            "revision_not_found(abc123)"
        );
        assert_eq!(UpstreamError::RepoNotFound.to_string(), "repo_not_found");
        assert_eq!(
            UpstreamError::InvalidResponse.to_string(),
            "invalid_response"
        );
        assert_eq!(UpstreamError::NoMainRef.to_string(), "no_main_ref");
    }
}
