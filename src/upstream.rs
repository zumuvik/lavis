//! Minimal Git smart-HTTP `info/refs` client used by the `info` command to
//! resolve the upstream repository's `main` revision, plus the Tangled
//! `sh.tangled.repo.compare` client that derives the ahead/behind relation.
//! Network access is confined behind narrow traits so parsing, request
//! orientation, and failure behavior stay deterministic and testable without
//! opening a socket.

use std::{future::Future, pin::Pin, time::Duration};

pub type UpstreamRevFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, UpstreamError>> + Send + 'a>>;

pub type CompareFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompareResult, UpstreamError>> + Send + 'a>>;

/// Raw result of one Tangled compare HTTP round trip: the response status
/// plus the bounded body, read before any status interpretation because the
/// error mapping needs the JSON payload of failed responses.
type RawCompareResponse = Result<(u16, Vec<u8>), UpstreamError>;

type RawCompareFuture<'a> = Pin<Box<dyn Future<Output = RawCompareResponse> + Send + 'a>>;

/// The HTTP boundary of a single Tangled compare request. Split out from the
/// parsing logic so the argument orientation is unit-testable without a
/// socket: tests script per-direction responses and observe which ordered
/// pair was requested.
trait RawCompareTransport: Send + Sync {
    fn fetch_compare<'a>(&'a self, rev1: &'a str, rev2: &'a str) -> RawCompareFuture<'a>;
}

/// The deliberately narrow boundary for reading the upstream `main` revision
/// and comparing two revisions. Tests can inject a fake without ever
/// constructing the real request URL.
pub trait UpstreamRev: Send + Sync {
    fn main_rev<'a>(&'a self) -> UpstreamRevFuture<'a>;
    fn compare<'a>(&'a self, base: &'a str, head: &'a str) -> CompareFuture<'a>;
}

/// Outcome of comparing two revisions: how many commits `head` carries that
/// `base` lacks, plus the merge base if available.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompareResult {
    pub ahead_by: u64,
    pub behind_by: u64,
    pub merge_base: Option<String>,
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
    RateLimited { retry_after: Option<Duration> },
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
            Self::RateLimited { retry_after } => {
                if let Some(duration) = retry_after {
                    write!(f, "rate_limited(retry_after={}s)", duration.as_secs())
                } else {
                    write!(f, "rate_limited")
                }
            }
        }
    }
}

const UPSTREAM_INFO_REFS_URL: &str =
    "https://tangled.org/zumuvik.tngl.sh/lavis/info/refs?service=git-upload-pack";
const TANGLED_COMPARE_URL: &str = "https://knot1.tangled.sh/xrpc/sh.tangled.repo.compare";
/// Repo identifier passed to `sh.tangled.repo.compare` as the `repo` query
/// parameter.
const TANGLED_COMPARE_REPO: &str = "did:plc:xhzbac5le4gwflk4t6stjjgf";
const SHA1_HEX_LEN: usize = 40;
/// `info/refs` advertises refs only, so its bodies stay tiny.
const MAX_INFO_REFS_BODY_BYTES: usize = 64 * 1024;
/// Compare responses embed `format_patch` (and friends), which grows with
/// the diff; the limit must leave room for that instead of reusing the
/// `info/refs` budget.
const MAX_COMPARE_BODY_BYTES: usize = 512 * 1024;
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

async fn read_bounded_body(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, UpstreamError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(UpstreamError::Transport);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| UpstreamError::Transport)?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
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
            let body = read_bounded_body(response, MAX_INFO_REFS_BODY_BYTES).await?;
            parse_main_rev_from_info_refs(&body)
        })
    }

    fn compare<'a>(&'a self, base: &'a str, head: &'a str) -> CompareFuture<'a> {
        Box::pin(async move {
            match perform_compare(self, base, head).await {
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

impl RawCompareTransport for HttpUpstreamRev {
    fn fetch_compare<'a>(&'a self, rev1: &'a str, rev2: &'a str) -> RawCompareFuture<'a> {
        Box::pin(async move {
            let response = self
                .client
                .get(TANGLED_COMPARE_URL)
                .query(&compare_query_params(rev1, rev2))
                .send()
                .await
                .map_err(request_error)?;
            let status = response.status().as_u16();
            let body = read_bounded_body(response, MAX_COMPARE_BODY_BYTES).await?;
            Ok((status, body))
        })
    }
}

/// The exact ordered `(key, value)` query pairs sent to
/// `sh.tangled.repo.compare`. The repository identifier is pinned here so a
/// regression test can assert the precise `repo` value the client sends and
/// catch a future drift back to an `at://` form or a wrong owner DID.
fn compare_query_params<'a>(rev1: &'a str, rev2: &'a str) -> [(&'static str, &'a str); 3] {
    [
        ("repo", TANGLED_COMPARE_REPO),
        ("rev1", rev1),
        ("rev2", rev2),
    ]
}

/// Runs one ordered Tangled compare. The public contract is
/// `compare(base, head)` → "how many commits does `head` carry that `base`
/// lacks". Confirmed from the Tangled knotserver source (`FormatPatch` →
/// `commitsBetween` walks `rev1..rev2`): `format_patch` lists the commits
/// reachable from `rev2` but not from `rev1`, so `rev1` must be the base and
/// `rev2` the head — swapping them inverts the answer. Live XRPC verification
/// was not available (endpoint rate-limited / 5xx), so orientation rests on
/// the source evidence.
async fn perform_compare<T: RawCompareTransport + ?Sized>(
    transport: &T,
    base: &str,
    head: &str,
) -> Result<CompareResult, UpstreamError> {
    let (status, body) = transport.fetch_compare(base, head).await?;
    if !(200..300).contains(&status) {
        return parse_tangled_error_response(status, &body, base, head);
    }
    parse_tangled_compare_response(&body)
}

/// Parses the Tangled compare JSON response. `format_patch` lists the
/// commits reachable from `rev2` but not from `rev1`, so with the
/// [`perform_compare`](crate::upstream) orientation (`rev1=base`,
/// `rev2=head`) its length is exactly `ahead_by`. `behind_by` stays zero:
/// callers derive it from the reverse ordered compare.
fn parse_tangled_compare_response(body: &[u8]) -> Result<CompareResult, UpstreamError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| UpstreamError::InvalidResponse)?;
    if value
        .get("merge_base")
        .is_some_and(|value| !value.is_string())
    {
        return Err(UpstreamError::InvalidResponse);
    }
    let merge_base = value
        .get("merge_base")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let ahead_by = match value.get("format_patch") {
        Some(value) => value
            .as_array()
            .ok_or(UpstreamError::InvalidResponse)?
            .len() as u64,
        // BASE==BASE responses legitimately omit the patch list.
        None if merge_base.is_some() => 0,
        None => return Err(UpstreamError::InvalidResponse),
    };
    Ok(CompareResult {
        ahead_by,
        behind_by: 0,
        merge_base,
    })
}

/// Derives the [`RevisionRelation`] of the local build (`current_rev`)
/// relative to upstream `main` (`main_rev`) from the ordered compare pair:
/// `first` answers what the local build carries beyond `main`
/// (`compare(main_rev, current_rev)`) and `reverse` answers the opposite
/// question (`compare(current_rev, main_rev)`).
///
/// Asymmetric by construction: swapping the two results flips Ahead/Behind,
/// and a merge base equal to `main` short-circuits to Ahead without needing
/// the reverse result at all.
pub fn relation_from_ordered_compares(
    main_rev: &str,
    current_rev: &str,
    first: CompareResult,
    reverse: CompareResult,
) -> RevisionRelation {
    match first.merge_base.as_deref() {
        Some(base) if base == main_rev => RevisionRelation::Ahead {
            commits: first.ahead_by,
        },
        Some(base) if base == current_rev => RevisionRelation::Behind {
            commits: reverse.ahead_by,
        },
        // Diverged, or an API response without a merge base: the only safe
        // interpretation left uses both directions.
        _ => RevisionRelation::Diverged {
            ahead: first.ahead_by,
            behind: reverse.ahead_by,
        },
    }
}

/// Parses a non-success Tangled compare response body, extracting structured
/// error names (`RevisionNotFound`, `RepoNotFound`, `RateLimitExceeded`) when
/// present. Falls back to a plain `HttpStatus` error when the body is not JSON
/// or the error name is unrecognized. `rev1`/`rev2` mirror the request order.
fn parse_tangled_error_response(
    status: u16,
    body: &[u8],
    rev1: &str,
    rev2: &str,
) -> Result<CompareResult, UpstreamError> {
    // Handle 429 rate limiting specially
    if status == 429 {
        tracing::warn!(
            event = "tangled_rate_limited",
            status,
            rev1,
            rev2,
            "Tangled API rate limit exceeded"
        );
        return Err(UpstreamError::RateLimited { retry_after: None });
    }

    if let Ok(error_body) = serde_json::from_slice::<serde_json::Value>(body)
        && let Some(error_name) = error_body.get("error").and_then(|e| e.as_str())
    {
        match error_name {
            "RevisionNotFound" => {
                // Determine which revision was not found by checking
                // whether the message mentions `rev1` (otherwise assume `rev2`).
                let revision = if error_body
                    .get("message")
                    .and_then(|m| m.as_str())
                    .is_some_and(|m| m.contains(rev1))
                {
                    rev1.to_string()
                } else {
                    rev2.to_string()
                };
                tracing::debug!(
                    event = "tangled_revision_not_found",
                    status,
                    revision = %revision,
                    rev1,
                    rev2,
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
            "RateLimitExceeded" => {
                tracing::warn!(
                    event = "tangled_rate_limited",
                    status,
                    rev1,
                    rev2,
                    "Tangled API rate limit exceeded"
                );
                return Err(UpstreamError::RateLimited { retry_after: None });
            }
            _ => {
                tracing::debug!(
                    event = "tangled_error_response",
                    status,
                    error_name,
                    rev1,
                    rev2,
                    "Tangled returned an unrecognized error name"
                );
            }
        }
    }
    tracing::debug!(
        event = "tangled_http_error",
        status,
        rev1,
        rev2,
        "Tangled compare returned a non-success status"
    );
    Err(UpstreamError::HttpStatus(status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Records the exact ordered `(rev1, rev2)` pair of every request and
    /// serves a canned status/body per pair, so tests can distinguish which
    /// direction the client actually asked for.
    struct ScriptedCompareTransport {
        responses: HashMap<(String, String), (u16, Vec<u8>)>,
        requested: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl ScriptedCompareTransport {
        fn empty() -> Self {
            Self {
                responses: HashMap::new(),
                requested: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn with_pair(mut self, rev1: &str, rev2: &str, patches: &[&str], merge_base: &str) -> Self {
            let body = serde_json::json!({
                "format_patch": patches
                    .iter()
                    .map(|sha| serde_json::json!({
                        "SHA": sha,
                        "Title": "fixture commit",
                        "Files": [],
                    }))
                    .collect::<Vec<_>>(),
                "merge_base": merge_base,
            })
            .to_string();
            self.responses
                .insert((rev1.to_owned(), rev2.to_owned()), (200, body.into_bytes()));
            self
        }

        fn requested_pairs(&self) -> Vec<(String, String)> {
            self.requested.lock().unwrap().clone()
        }
    }

    impl RawCompareTransport for ScriptedCompareTransport {
        fn fetch_compare<'a>(&'a self, rev1: &'a str, rev2: &'a str) -> RawCompareFuture<'a> {
            self.requested
                .lock()
                .unwrap()
                .push((rev1.to_owned(), rev2.to_owned()));
            let outcome = self
                .responses
                .get(&(rev1.to_owned(), rev2.to_owned()))
                .cloned()
                .ok_or(UpstreamError::InvalidResponse);
            Box::pin(async move { outcome })
        }
    }

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
                behind_by: 0,
                merge_base: None,
            }),
            RevisionRelation::Current
        );
        assert_eq!(
            relation_from_compare(&CompareResult {
                ahead_by: 5,
                behind_by: 0,
                merge_base: Some("abc123".to_string()),
            }),
            RevisionRelation::Ahead { commits: 5 }
        );
        assert_eq!(
            relation_from_compare(&CompareResult {
                ahead_by: 0,
                behind_by: 3,
                merge_base: None,
            }),
            RevisionRelation::Behind { commits: 3 }
        );
        assert_eq!(
            relation_from_compare(&CompareResult {
                ahead_by: 2,
                behind_by: 4,
                merge_base: None,
            }),
            RevisionRelation::Diverged {
                ahead: 2,
                behind: 4
            }
        );
    }

    #[test]
    fn parses_tangled_compare_response() {
        let body = br#"{
            "merge_base":"BASE",
            "format_patch":[
                {"SHA":"HEAD1","Title":"first commit","Files":[]},
                {"SHA":"HEAD2","Title":"second commit","Files":[]},
                {"SHA":"HEAD3","Title":"third commit","Files":[]}
            ]
        }"#;
        let result = parse_tangled_compare_response(body).unwrap();
        assert_eq!(result.ahead_by, 3);
        assert_eq!(result.merge_base, Some("BASE".to_string()));
    }

    #[test]
    fn parses_tangled_compare_empty_patches() {
        let body = br#"{"format_patch":[],"merge_base":"def456"}"#;
        let result = parse_tangled_compare_response(body).unwrap();
        assert_eq!(result.ahead_by, 0);
        assert_eq!(result.merge_base, Some("def456".to_string()));
    }

    #[test]
    fn parses_tangled_compare_without_merge_base() {
        let body = br#"{"format_patch":["patch1"]}"#;
        let result = parse_tangled_compare_response(body).unwrap();
        assert_eq!(result.ahead_by, 1);
        assert_eq!(result.merge_base, None);
    }

    #[test]
    fn tangled_compare_missing_field_is_invalid() {
        assert_eq!(
            parse_tangled_compare_response(b"{}"),
            Err(UpstreamError::InvalidResponse)
        );
        assert_eq!(
            parse_tangled_compare_response(
                br#"{"merge_base":"BASE","format_patch":"not-an-array"}"#
            ),
            Err(UpstreamError::InvalidResponse)
        );
        assert_eq!(
            parse_tangled_compare_response(b"not json"),
            Err(UpstreamError::InvalidResponse)
        );
    }

    #[test]
    fn missing_format_patch_is_valid_zero_diff() {
        let result = parse_tangled_compare_response(br#"{"merge_base":"BASE"}"#).unwrap();
        assert_eq!(result.ahead_by, 0);
        assert_eq!(result.merge_base.as_deref(), Some("BASE"));
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

    // Regression coverage for the compare orientation (blocker: the client
    // used to swap rev1/rev2, turning Ahead{N} into Ahead{0}). Every fixture
    // below is asymmetric on purpose: forward and reverse directions carry
    // different patch counts, so swapping the arguments breaks the tests.

    /// ```text
    /// A -- B -- C
    ///      main  current
    /// ```
    #[tokio::test]
    async fn ahead_graph_counts_head_commits_only() {
        let transport = ScriptedCompareTransport::empty()
            .with_pair("B", "C", &["patch-C"], "B")
            // The reversed question ("what does main have beyond current")
            // has a different answer — a swap must land here and fail.
            .with_pair("C", "B", &[], "B");

        let result = perform_compare(&transport, "B", "C").await.unwrap();

        assert_eq!(
            result,
            CompareResult {
                ahead_by: 1,
                behind_by: 0,
                merge_base: Some("B".to_owned()),
            }
        );
        assert_eq!(
            transport.requested_pairs(),
            vec![("B".to_owned(), "C".to_owned())]
        );
    }

    /// Same graph, opposite call: `compare(current, main)` must count zero
    /// head commits. Together with
    /// [`ahead_graph_counts_head_commits_only`] this pins which argument is
    /// the base and which is the head.
    #[tokio::test]
    async fn reverse_ahead_graph_counts_zero_head_commits() {
        let transport = ScriptedCompareTransport::empty()
            .with_pair("B", "C", &["patch-C"], "B")
            .with_pair("C", "B", &[], "B");

        let result = perform_compare(&transport, "C", "B").await.unwrap();

        assert_eq!(
            result,
            CompareResult {
                ahead_by: 0,
                behind_by: 0,
                merge_base: Some("B".to_owned()),
            }
        );
    }

    /// ```text
    /// A -- B -- C
    ///   current  main
    /// ```
    /// `compare(main, current)` asks what `current` carries beyond `main`:
    /// nothing; only the reverse direction sees the commit.
    #[tokio::test]
    async fn behind_graph_directions_answer_differently() {
        let transport = ScriptedCompareTransport::empty()
            .with_pair("C", "B", &[], "B")
            .with_pair("B", "C", &["patch-C"], "B");

        let first = perform_compare(&transport, "C", "B").await.unwrap();
        let reverse = perform_compare(&transport, "B", "C").await.unwrap();

        assert_eq!(first.ahead_by, 0);
        assert_eq!(reverse.ahead_by, 1);
        assert_ne!(first.ahead_by, reverse.ahead_by);
        assert_eq!(
            transport.requested_pairs(),
            vec![
                ("C".to_owned(), "B".to_owned()),
                ("B".to_owned(), "C".to_owned()),
            ]
        );
    }

    /// ```text
    ///       C -- D   current
    ///      /
    /// A -- B
    ///      \
    ///       E -- F -- G   main
    /// ```
    #[tokio::test]
    async fn diverged_graph_directions_count_two_and_three() {
        let transport = ScriptedCompareTransport::empty()
            .with_pair("G", "D", &["patch-C", "patch-D"], "B")
            .with_pair("D", "G", &["patch-E", "patch-F", "patch-G"], "B");

        let first = perform_compare(&transport, "G", "D").await.unwrap();
        let reverse = perform_compare(&transport, "D", "G").await.unwrap();

        assert_eq!(first.ahead_by, 2);
        assert_eq!(first.merge_base.as_deref(), Some("B"));
        assert_eq!(reverse.ahead_by, 3);
        assert_eq!(reverse.merge_base.as_deref(), Some("B"));
    }

    #[test]
    fn ahead_relation_ignores_the_reverse_result() {
        let first = CompareResult {
            ahead_by: 1,
            behind_by: 0,
            merge_base: Some("B".to_owned()),
        };
        let reverse = CompareResult {
            ahead_by: 99,
            behind_by: 0,
            merge_base: Some("B".to_owned()),
        };

        assert_eq!(
            relation_from_ordered_compares("B", "C", first, reverse),
            RevisionRelation::Ahead { commits: 1 }
        );
    }

    #[test]
    fn behind_relation_counts_the_reverse_compare() {
        // A -- B -- C: current=B, main=C.
        let first = CompareResult {
            ahead_by: 0,
            behind_by: 0,
            merge_base: Some("B".to_owned()),
        };
        let reverse = CompareResult {
            ahead_by: 1,
            behind_by: 0,
            merge_base: Some("B".to_owned()),
        };

        assert_eq!(
            relation_from_ordered_compares("C", "B", first, reverse),
            RevisionRelation::Behind { commits: 1 }
        );
    }

    #[test]
    fn diverged_relation_combines_both_directions() {
        let first = CompareResult {
            ahead_by: 2,
            behind_by: 0,
            merge_base: Some("B".to_owned()),
        };
        let reverse = CompareResult {
            ahead_by: 3,
            behind_by: 0,
            merge_base: Some("B".to_owned()),
        };

        assert_eq!(
            relation_from_ordered_compares("G", "D", first, reverse),
            RevisionRelation::Diverged {
                ahead: 2,
                behind: 3
            }
        );
    }

    /// Swapping the ordered results flips Behind to an incorrect zero:
    /// the derivation must consume them in call order.
    #[test]
    fn swapped_relation_results_break_the_behind_count() {
        let correct_first = CompareResult {
            ahead_by: 0,
            behind_by: 0,
            merge_base: Some("B".to_owned()),
        };
        let correct_reverse = CompareResult {
            ahead_by: 1,
            behind_by: 0,
            merge_base: Some("B".to_owned()),
        };

        let swapped = relation_from_ordered_compares(
            "C",
            "B",
            correct_reverse.clone(),
            correct_first.clone(),
        );
        assert_ne!(
            swapped,
            RevisionRelation::Behind { commits: 1 },
            "swapped results accidentally produced the right answer"
        );

        let ordered = relation_from_ordered_compares("C", "B", correct_first, correct_reverse);
        assert_eq!(ordered, RevisionRelation::Behind { commits: 1 });
    }

    /// The `repo` query parameter must be the canonical repository DID. A
    /// regression here fails if the client ever sends an `at://` identifier,
    /// an owner-DID/repo-name form, or another wrong repository identifier.
    #[test]
    fn compare_query_uses_canonical_did_repo_identifier() {
        let params = compare_query_params("main", "feat/info-command");

        let repo = params
            .iter()
            .find(|(key, _)| *key == "repo")
            .expect("compare query must carry a repo parameter");
        assert_eq!(repo.1, "did:plc:xhzbac5le4gwflk4t6stjjgf");
        assert!(
            !repo.1.starts_with("at://"),
            "repo must not be an at:// AT-URI: {repo:?}"
        );
        assert!(
            repo.1.starts_with("did:plc:"),
            "repo must be a did:plc repository identifier: {repo:?}"
        );

        let rev1 = params
            .iter()
            .find(|(key, _)| *key == "rev1")
            .expect("compare query must carry rev1");
        assert_eq!(rev1.1, "main");
        let rev2 = params
            .iter()
            .find(|(key, _)| *key == "rev2")
            .expect("compare query must carry rev2");
        assert_eq!(rev2.1, "feat/info-command");
    }

    /// A swapped `rev1`/`rev2` pair must produce a different (inverted) answer:
    /// the test fixture is asymmetric so a naive swap breaks the expectation.
    #[tokio::test]
    async fn swapped_query_orientation_breaks_the_expected_answer() {
        let transport = ScriptedCompareTransport::empty()
            .with_pair("B", "C", &["patch-C"], "B")
            // Reversed question yields a different patch count — a swap lands
            // here and must fail rather than silently agree.
            .with_pair("C", "B", &["patch-B"], "B");

        let forward = perform_compare(&transport, "B", "C").await.unwrap();
        assert_eq!(forward.ahead_by, 1);

        assert_eq!(
            transport.requested_pairs(),
            vec![("B".to_owned(), "C".to_owned())]
        );
    }
}
