//! The control plane: the site's authed routes, and nothing else. A trimmed
//! sibling of Wolf2 Desktop's `desktop/crates/core/src/api.rs` (avstream commit
//! d64f6f1f671dee560549d5015d281ba03932f571): the same bearer header and the
//! same 401-vs-403 classification, with only the three routes the tray calls.
//!
//! The ceiling, stated so it isn't drifted past: no bucket credential, no
//! LIST, no master key. The tray reads which recording is live and writes its
//! own segments. That is all.

use crate::auth::{self, Session};

/// A 401 from any route. The implicit grant has no refresh token, so the only
/// remedy is another browser dance — surfaced rather than silently retried.
#[derive(Debug)]
pub struct AuthExpired(pub String);

impl std::fmt::Display for AuthExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug)]
pub enum ApiError {
    Expired(AuthExpired),
    Denied(String),
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Expired(e) => write!(f, "{e}"),
            ApiError::Denied(s) => write!(f, "{s}"),
            ApiError::Other(s) => write!(f, "{s}"),
        }
    }
}

fn authorization_error(status: reqwest::StatusCode, resource: &str) -> Option<ApiError> {
    match status.as_u16() {
        401 => Some(ApiError::Expired(AuthExpired(format!(
            "signed out (HTTP 401 from {resource}) — sign in again"
        )))),
        403 => Some(ApiError::Denied(format!(
            "access denied (HTTP 403 from {resource})"
        ))),
        _ => None,
    }
}

/// `/api/recordings` entries (web-client/src/lib/server/recordingsIndex.ts).
/// Only the fields the tray uses; `#[serde(default)]` so an index version bump
/// doesn't break it.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingEntry {
    pub partition: String,
    pub recording: String,
    pub instant: String,
    #[serde(default)]
    pub started_ms: i64,
    /// "live", "finalizing", "complete", "failed", "discarded"; None before
    /// status docs existed.
    #[serde(default)]
    pub status: Option<String>,
}

impl RecordingEntry {
    pub fn scope(&self) -> String {
        format!("{}/{}/{}", self.partition, self.recording, self.instant)
    }
    pub fn is_live(&self) -> bool {
        self.status.as_deref() == Some("live")
    }
    pub fn accepts_pushes(&self) -> bool {
        matches!(self.status.as_deref(), Some("live") | Some("finalizing"))
    }
}

/// One registered tenant this member may access (GET /api/tenants).
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantEntry {
    pub id: String,
    #[serde(default)]
    pub visible: bool,
}

/// What a segment PUT came back with (spec §5).
#[derive(Debug, PartialEq, Eq)]
pub enum PutOutcome {
    /// 200 `{ stored: true }`.
    Stored,
    /// 200 `{ stored: false }` — a byte-identical retry.
    AlreadyStored,
    /// 409 with a different body for an existing key.
    Conflict,
    /// 409 `Recording is not live`.
    NotLive,
    /// 404/410: the recording is gone or archived.
    Gone,
    /// 413/400: the server refused the bytes; retrying can't help.
    Rejected(String),
}

/// Cheap to clone — `reqwest::Client` is an Arc internally.
#[derive(Clone)]
pub struct Api {
    client: reqwest::Client,
    base: String,
}

impl Api {
    pub fn new(base: String) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("wolf2-tray/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { client, base })
    }

    /// The session is read PER REQUEST (never snapshotted into the client):
    /// a sign-in while the tray runs must reach the next upload, and a 401
    /// must become recoverable by signing in again rather than permanent.
    fn session() -> Result<Session, ApiError> {
        auth::current_session().ok_or_else(|| {
            ApiError::Expired(AuthExpired(
                "not signed in — sign in from the tray menu".into(),
            ))
        })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        let session = Self::session()?;
        let url = format!("{}{path}", self.base);
        let res = self
            .client
            .get(&url)
            .header("authorization", session.authorization())
            .send()
            .await
            .map_err(|e| ApiError::Other(format!("GET {path}: {e}")))?;
        let status = res.status();
        if let Some(error) = authorization_error(status, path) {
            return Err(error);
        }
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            return Err(ApiError::Other(format!(
                "GET {path}: HTTP {} {}",
                status.as_u16(),
                body.chars().take(200).collect::<String>()
            )));
        }
        res.json::<T>()
            .await
            .map_err(|e| ApiError::Other(format!("GET {path}: bad response: {e}")))
    }

    /// The recordings index for one tenant. `partial: true` means a budgeted
    /// backfill is still converging; the tray polls anyway, so it only logs.
    pub async fn recordings(&self, tenant: &str) -> Result<Vec<RecordingEntry>, ApiError> {
        #[derive(serde::Deserialize)]
        struct Index {
            #[serde(default)]
            recordings: Vec<RecordingEntry>,
            #[serde(default)]
            partial: bool,
        }
        let path = format!("/api/recordings?tenant={}", url_encode(tenant));
        let idx: Index = self.get_json(&path).await?;
        if idx.partial {
            tracing::debug!("recordings index is partial");
        }
        Ok(idx.recordings)
    }

    pub async fn tenants(&self) -> Result<Vec<TenantEntry>, ApiError> {
        #[derive(serde::Deserialize)]
        struct Wire {
            tenants: Vec<TenantEntry>,
        }
        let wire: Wire = self.get_json("/api/tenants").await?;
        Ok(wire.tenants)
    }

    /// PUT one Ownship Track segment (spec §5). `scope` is
    /// `{partition}/{recording}/{instant}`.
    pub async fn put_ownship_segment(
        &self,
        scope: &str,
        session_id: &str,
        minute: i64,
        gz: Vec<u8>,
        pilot: &str,
        object_id: &str,
    ) -> Result<PutOutcome, ApiError> {
        let session = Self::session()?;
        let path = format!("/api/ownship/{scope}/{session_id}/{minute}");
        let url = format!("{}{path}", self.base);
        let res = self
            .client
            .put(&url)
            .header("authorization", session.authorization())
            .header("content-type", "application/gzip")
            .header("x-ownship-pilot", sanitize_header(pilot))
            .header("x-ownship-object", object_id)
            .body(gz)
            .send()
            .await
            .map_err(|e| ApiError::Other(format!("PUT {path}: {e}")))?;
        let status = res.status();
        if let Some(error) = authorization_error(status, &path) {
            return Err(error);
        }
        let body = res.text().await.unwrap_or_default();
        match status.as_u16() {
            200 | 201 => {
                #[derive(serde::Deserialize)]
                struct Reply {
                    #[serde(default = "yes")]
                    stored: bool,
                }
                fn yes() -> bool {
                    true
                }
                let reply: Reply = serde_json::from_str(&body).unwrap_or(Reply { stored: true });
                Ok(if reply.stored {
                    PutOutcome::Stored
                } else {
                    PutOutcome::AlreadyStored
                })
            }
            409 if body.to_ascii_lowercase().contains("not live") => Ok(PutOutcome::NotLive),
            409 => Ok(PutOutcome::Conflict),
            404 | 410 => Ok(PutOutcome::Gone),
            400 | 413 | 422 => Ok(PutOutcome::Rejected(format!(
                "HTTP {} {}",
                status.as_u16(),
                body.chars().take(200).collect::<String>()
            ))),
            _ => Err(ApiError::Other(format!(
                "PUT {path}: HTTP {} {}",
                status.as_u16(),
                body.chars().take(200).collect::<String>()
            ))),
        }
    }
}

fn url_encode(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// A header value must be visible ASCII; a Discord name need not be. Anything
/// else becomes `?` — the authoritative label rides inside the segment.
fn sanitize_header(s: &str) -> String {
    s.chars()
        .map(|c| if (' '..='~').contains(&c) { c } else { '?' })
        .take(120)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_and_finalizing_accept_pushes() {
        let e = |s: Option<&str>| RecordingEntry {
            partition: "202610".into(),
            recording: "shadowlands".into(),
            instant: "20261028_192125".into(),
            started_ms: 0,
            status: s.map(String::from),
        };
        assert!(e(Some("live")).is_live());
        assert!(e(Some("live")).accepts_pushes());
        assert!(e(Some("finalizing")).accepts_pushes());
        assert!(!e(Some("finalizing")).is_live());
        assert!(!e(Some("complete")).accepts_pushes());
        assert!(!e(None).accepts_pushes());
        assert_eq!(e(None).scope(), "202610/shadowlands/20261028_192125");
    }

    #[test]
    fn header_values_stay_ascii() {
        assert_eq!(sanitize_header("Ægir | Wolf"), "?gir | Wolf");
        assert_eq!(url_encode("a b/c"), "a+b%2Fc");
    }
}
