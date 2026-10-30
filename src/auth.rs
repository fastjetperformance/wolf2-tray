//! Discord sign-in — a copy of Wolf2 Desktop's `desktop/crates/core/src/auth.rs`
//! (avstream commit d64f6f1f671dee560549d5015d281ba03932f571), minus the BYO-AI
//! credential section the tray has no use for. Kept in step with that file and
//! with the browser flow it mirrors (`web-client/src/lib/auth/discord.ts`);
//! the Credential Manager entry is the SAME one Desktop uses, so a member who
//! runs both signs in once.
//!
//! Discord sign-in — the same implicit grant the site runs, as the same
//! public client. `web-client/src/lib/auth/discord.ts` is the browser-flow
//! reference; keep this native port in step with it.
//!
//! Why this works without a client secret: the site's own flow is
//! `response_type=token` (`web-client/src/lib/auth/discord.ts:86-101`) — a
//! public client, no secret anywhere in the repo. So a desktop app can run the
//! identical dance and hold the identical credential. Nothing server-side
//! changes.
//!
//! **The fragment trap.** The implicit grant returns the token in the URL
//! *fragment*, which a browser never sends to a server — a loopback listener
//! would see `GET /oauth2/callback` with nothing in it. So the callback serves
//! a page whose script reads `location.hash` and POSTs it back to `/token`.
//! That is not a workaround; it is how native OAuth clients read a fragment.
//!
//! Wolf2 Desktop protects the Discord session in Windows Credential Manager,
//! under an entry distinct from the future AI Runtime credential. The loopback
//! callback still carries the RFC 8252 caveat that a local process binding
//! :8917 first could intercept it while the browser dance is in flight.

// Kept as close to the Desktop original as possible, so clippy's style
// suggestions are declined here rather than applied in one copy only.
#![allow(clippy::collapsible_if, clippy::inconsistent_digit_grouping)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The deployed site: control plane and OAuth client. Same default as the MCP
/// server (`web-client/src/mcp/config.ts:11`).
pub fn base_url() -> String {
    std::env::var("AVSTREAM_BASE_URL").unwrap_or_else(|_| "https://wolf2.net".to_string())
}

/// The site's own public client id — not a secret, it rides in the authorize
/// URL (`web-client/src/lib/auth/discord.ts:13-15`).
fn client_id() -> String {
    std::env::var("AVSTREAM_DISCORD_CLIENT_ID")
        .unwrap_or_else(|_| "1181549538377486347".to_string())
}

const AUTHORIZE_URL: &str = "https://discord.com/api/oauth2/authorize";
const SCOPE: &str = "identify";

/// Fixed, and registered in the Discord dev portal (LOCAL_MCP.md §1). Discord
/// matches redirect URIs exactly, so this cannot be an ephemeral port.
pub const CALLBACK_PORT: u16 = 8917;

const DANCE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Legacy standalone-MCP token path. Windows Desktop imports a live value once
/// into Credential Manager and removes this plaintext migration source.
pub fn token_file() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".avstream").join("mcp.json"))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub access_token: String,
    #[serde(default = "bearer")]
    pub token_type: String,
    /// epoch ms
    pub expires_at: i64,
    #[serde(default)]
    pub scope: String,
}

fn bearer() -> String {
    "Bearer".to_string()
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Session {
    pub fn is_live(&self) -> bool {
        !self.access_token.is_empty() && now_ms() < self.expires_at
    }
    pub fn authorization(&self) -> String {
        format!("{} {}", self.token_type, self.access_token)
    }
}

/// Headless fallback, same contract as the MCP server's `AVSTREAM_SESSION`:
/// either the raw `discord_session` JSON from a signed-in tab, or a bare token.
fn env_session() -> Option<Session> {
    let raw = std::env::var("AVSTREAM_SESSION").ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(s) = serde_json::from_str::<Session>(raw) {
        if !s.access_token.is_empty() {
            return Some(s);
        }
    }
    Some(Session {
        access_token: raw.to_string(),
        token_type: bearer(),
        expires_at: now_ms() + 24 * 3600_000,
        scope: SCOPE.to_string(),
    })
}

pub const CREDENTIAL_SERVICE: &str = "net.wolf2.desktop";
pub const DISCORD_CREDENTIAL_ACCOUNT: &str = "discord-session";

trait SessionStore {
    fn load(&self) -> Result<Option<String>, String>;
    fn save(&self, value: &str) -> Result<(), String>;
    fn delete(&self) -> Result<(), String>;
}

struct ProtectedSessionStore;

#[cfg(target_os = "windows")]
impl ProtectedSessionStore {
    fn entry() -> Result<keyring::Entry, String> {
        keyring::Entry::new(CREDENTIAL_SERVICE, DISCORD_CREDENTIAL_ACCOUNT)
            .map_err(|error| format!("open Windows Credential Manager entry: {error}"))
    }
}

#[cfg(target_os = "windows")]
impl SessionStore for ProtectedSessionStore {
    fn load(&self) -> Result<Option<String>, String> {
        match Self::entry()?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(format!(
                "read Discord session from Windows Credential Manager: {error}"
            )),
        }
    }

    fn save(&self, value: &str) -> Result<(), String> {
        Self::entry()?.set_password(value).map_err(|error| {
            format!("protect Discord session in Windows Credential Manager: {error}")
        })
    }

    fn delete(&self) -> Result<(), String> {
        match Self::entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(format!(
                "remove Discord session from Windows Credential Manager: {error}"
            )),
        }
    }
}

#[cfg(not(target_os = "windows"))]
impl SessionStore for ProtectedSessionStore {
    fn load(&self) -> Result<Option<String>, String> {
        Ok(read_legacy_text())
    }

    fn save(&self, value: &str) -> Result<(), String> {
        let path = token_file().ok_or("no home directory")?;
        let dir = path.parent().ok_or("no parent directory")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        file.write_all(value.as_bytes()).map_err(|e| e.to_string())
    }

    fn delete(&self) -> Result<(), String> {
        delete_legacy_file()
    }
}

fn read_legacy_text() -> Option<String> {
    std::fs::read_to_string(token_file()?).ok()
}

fn read_legacy_session() -> Option<Session> {
    serde_json::from_str::<Session>(&read_legacy_text()?).ok()
}

fn delete_legacy_file() -> Result<(), String> {
    let Some(path) = token_file() else {
        return Ok(());
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

fn current_session_from(store: &dyn SessionStore) -> Result<Option<Session>, String> {
    let Some(raw) = store.load()? else {
        return Ok(None);
    };
    let session = serde_json::from_str::<Session>(&raw).ok();
    if session.as_ref().is_some_and(Session::is_live) {
        Ok(session)
    } else {
        store.delete()?;
        Ok(None)
    }
}

fn save_session_to(store: &dyn SessionStore, session: &Session) -> Result<(), String> {
    let body = serde_json::to_string(session).map_err(|error| error.to_string())?;
    store.save(&body)
}

/// The live session, if any — environment override first, then protected
/// storage. A stored-but-expired token is deleted and never refreshed: the
/// implicit grant issues no refresh token, so re-auth is the only path.
pub fn current_session() -> Option<Session> {
    if let Some(session) = env_session().filter(Session::is_live) {
        return Some(session);
    }
    let store = ProtectedSessionStore;
    match current_session_from(&store) {
        Ok(Some(session)) => return Some(session),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!("[auth] {error}");
            return None;
        }
    }
    #[cfg(target_os = "windows")]
    if let Some(session) = read_legacy_session().filter(Session::is_live) {
        match save_session_to(&store, &session) {
            Ok(()) => {
                let _ = delete_legacy_file();
                return Some(session);
            }
            Err(error) => {
                tracing::warn!(
                    "[auth] refusing plaintext session because migration failed: {error}"
                );
                return None;
            }
        }
    }
    None
}

pub fn save_session(session: &Session) -> Result<(), String> {
    save_session_to(&ProtectedSessionStore, session)?;
    #[cfg(target_os = "windows")]
    delete_legacy_file()?;
    Ok(())
}

pub fn sign_out() -> Result<(), String> {
    ProtectedSessionStore.delete()?;
    #[cfg(target_os = "windows")]
    delete_legacy_file()?;
    Ok(())
}

fn random_state() -> String {
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("cmd")
        .args(["/C", "start", "", &url.replace('&', "^&")])
        .spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = r {
        tracing::warn!("could not open a browser ({e}); sign in manually at: {url}");
    }
}

/// Served at /oauth2/callback: forwards the fragment to the listener. Verbatim
/// from mcp/auth.ts:100-107 — the fragment is unreachable any other way.
const CALLBACK_HTML: &str = r#"<!doctype html><meta charset="utf-8"><title>avstream sign-in</title>
<body style="font-family:system-ui;padding:2rem"><p id="m">Completing sign-in…</p>
<script>
fetch("/token", { method: "POST", headers: { "content-type": "text/plain" }, body: location.hash })
  .then(r => { document.getElementById("m").textContent = r.ok
    ? "Signed in — you can close this tab." : "Sign-in failed: " + r.status; })
  .catch(() => { document.getElementById("m").textContent = "Sign-in failed (listener gone?)"; });
</script></body>"#;

pub fn authorize_url(state: &str) -> String {
    let params = [
        ("client_id", client_id()),
        (
            "redirect_uri",
            format!("http://localhost:{CALLBACK_PORT}/oauth2/callback"),
        ),
        ("response_type", "token".to_string()),
        ("scope", SCOPE.to_string()),
        ("state", state.to_string()),
    ];
    let q: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect();
    format!("{AUTHORIZE_URL}?{}", q.join("&"))
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Parse the `#access_token=…&state=…` fragment the callback page posts back.
pub fn parse_fragment(body: &str) -> std::collections::BTreeMap<String, String> {
    let raw = body.trim().trim_start_matches('#');
    form_urlencoded::parse(raw.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// Build a session from the callback's fragment params, verifying CSRF state.
pub fn session_from_fragment(
    params: &std::collections::BTreeMap<String, String>,
    expect_state: &str,
) -> Result<Session, String> {
    if let Some(err) = params.get("error") {
        let desc = params.get("error_description").unwrap_or(err);
        return Err(format!("Discord refused sign-in: {desc}"));
    }
    let access_token = params
        .get("access_token")
        .ok_or("callback carried no access_token")?;
    // Constant-ish comparison is overkill for a CSRF nonce, but a mismatch must
    // fail closed rather than sign the user in as someone else's dance.
    if params.get("state").map(String::as_str) != Some(expect_state) {
        return Err("OAuth callback state mismatch — sign-in rejected".to_string());
    }
    let expires_in: i64 = params
        .get("expires_in")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok(Session {
        access_token: access_token.clone(),
        token_type: params.get("token_type").cloned().unwrap_or_else(bearer),
        expires_at: now_ms() + expires_in * 1000,
        scope: params
            .get("scope")
            .cloned()
            .unwrap_or_else(|| SCOPE.to_string()),
    })
}

// ── The loopback listener ────────────────────────────────────────────────────
// Hand-rolled rather than pulling an HTTP server crate: two routes, loopback
// only, single-shot, and every byte is inspected here rather than in a
// dependency. Anything richer would be more surface, not less.

struct Request {
    method: String,
    path: String,
    body: String,
}

fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        let n = reader.read_line(&mut header).map_err(|e| e.to_string())?;
        if n == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }

    // Cap the body: a fragment is a few hundred bytes. Anything larger is not
    // our callback, and this listener must not be a memory sink.
    const MAX_BODY: usize = 16 * 1024;
    let mut body = Vec::new();
    if content_length > 0 {
        let take = content_length.min(MAX_BODY);
        body.resize(take, 0);
        reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    }
    Ok(Request {
        method,
        path,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let res = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(res.as_bytes());
    let _ = stream.flush();
}

/// Open the browser at Discord's authorize URL, listen on :8917, verify the
/// CSRF state, persist and return the session. Blocking — call it off the UI
/// thread.
pub fn run_oauth_dance() -> Result<Session, String> {
    let state = random_state();
    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).map_err(|e| {
        format!(
            "cannot listen on 127.0.0.1:{CALLBACK_PORT} ({e}). \
             The MCP server uses the same port during its own sign-in — if it is \
             mid-dance, wait and retry."
        )
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("listener: {e}"))?;

    open_browser(&authorize_url(&state));

    let deadline = Instant::now() + DANCE_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            return Err("Sign-in timed out — no OAuth callback within 5 minutes".to_string());
        }
        let (mut stream, _) = match listener.accept() {
            Ok(pair) => pair,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => return Err(format!("accept: {e}")),
        };
        stream.set_nonblocking(false).ok();
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();

        let req = match read_request(&mut stream) {
            Ok(r) => r,
            Err(_) => continue, // a malformed probe is not our callback
        };

        if req.method == "GET" && req.path.starts_with("/oauth2/callback") {
            respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                CALLBACK_HTML,
            );
            continue;
        }
        if req.method == "POST" && req.path == "/token" {
            let params = parse_fragment(&req.body);
            match session_from_fragment(&params, &state) {
                Ok(session) => {
                    save_session(&session)?;
                    respond(&mut stream, "200 OK", "text/plain", "ok");
                    return Ok(session);
                }
                Err(e) => {
                    respond(&mut stream, "400 Bad Request", "text/plain", &e);
                    return Err(e);
                }
            }
        }
        respond(&mut stream, "404 Not Found", "text/plain", "");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct MemorySessionStore {
        value: RefCell<Option<String>>,
        deletes: RefCell<usize>,
    }

    impl SessionStore for MemorySessionStore {
        fn load(&self) -> Result<Option<String>, String> {
            Ok(self.value.borrow().clone())
        }

        fn save(&self, value: &str) -> Result<(), String> {
            *self.value.borrow_mut() = Some(value.to_string());
            Ok(())
        }

        fn delete(&self) -> Result<(), String> {
            *self.value.borrow_mut() = None;
            *self.deletes.borrow_mut() += 1;
            Ok(())
        }
    }

    fn frag(s: &str) -> std::collections::BTreeMap<String, String> {
        parse_fragment(s)
    }

    #[test]
    fn parses_the_implicit_grant_fragment() {
        let p = frag(
            "#access_token=abc123&token_type=Bearer&expires_in=604800&scope=identify&state=deadbeef",
        );
        assert_eq!(p.get("access_token").unwrap(), "abc123");
        assert_eq!(p.get("state").unwrap(), "deadbeef");
        let s = session_from_fragment(&p, "deadbeef").unwrap();
        assert_eq!(s.access_token, "abc123");
        assert_eq!(s.token_type, "Bearer");
        assert_eq!(s.scope, "identify");
        assert!(s.expires_at > now_ms(), "expiry must be in the future");
    }

    #[test]
    fn rejects_a_state_mismatch() {
        // CSRF: someone else's dance must never sign us in.
        let p = frag("#access_token=abc&state=attacker");
        assert!(session_from_fragment(&p, "ours").is_err());
    }

    #[test]
    fn rejects_a_missing_state_even_with_a_token() {
        let p = frag("#access_token=abc&token_type=Bearer");
        assert!(session_from_fragment(&p, "ours").is_err());
    }

    #[test]
    fn surfaces_discords_error_response() {
        let p = frag("#error=access_denied&error_description=The+user+denied+the+request");
        let e = session_from_fragment(&p, "ours").unwrap_err();
        assert!(e.contains("denied"), "got {e}");
    }

    #[test]
    fn authorize_url_is_the_sites_own_public_client_flow() {
        // response_type=token (implicit) and no secret — this is what makes a
        // desktop client possible at all (lib/auth/discord.ts:86-101).
        let url = authorize_url("s1");
        assert!(url.starts_with("https://discord.com/api/oauth2/authorize?"));
        assert!(url.contains("response_type=token"));
        assert!(url.contains("scope=identify"));
        assert!(url.contains("state=s1"));
        assert!(url.contains(&urlencode(&format!(
            "http://localhost:{CALLBACK_PORT}/oauth2/callback"
        ))));
        assert!(!url.contains("client_secret"));
    }

    #[test]
    fn an_expired_stored_token_is_not_live() {
        let s = Session {
            access_token: "x".into(),
            token_type: bearer(),
            expires_at: now_ms() - 1,
            scope: SCOPE.into(),
        };
        assert!(!s.is_live());
    }

    #[test]
    fn reads_the_mcp_servers_session_shape() {
        // The legacy ~/.avstream/mcp.json shape is camelCase. Desktop can
        // import it once without inventing a second session wire format.
        let json = r#"{"accessToken":"tok","tokenType":"Bearer","expiresAt":99999999999999,"scope":"identify"}"#;
        let s: Session = serde_json::from_str(json).unwrap();
        assert_eq!(s.access_token, "tok");
        assert_eq!(s.authorization(), "Bearer tok");
        assert!(s.is_live());
    }

    #[test]
    fn persists_the_discord_session_through_the_protected_store() {
        let store = MemorySessionStore::default();
        let session = Session {
            access_token: "desktop-token".into(),
            token_type: bearer(),
            expires_at: now_ms() + 60_000,
            scope: SCOPE.into(),
        };

        save_session_to(&store, &session).unwrap();
        let loaded = current_session_from(&store).unwrap().unwrap();

        assert_eq!(loaded.access_token, "desktop-token");
        assert_eq!(loaded.expires_at, session.expires_at);
    }

    #[test]
    fn an_expired_protected_session_is_deleted() {
        let store = MemorySessionStore::default();
        let expired = Session {
            access_token: "expired".into(),
            token_type: bearer(),
            expires_at: now_ms() - 1,
            scope: SCOPE.into(),
        };
        store
            .save(&serde_json::to_string(&expired).unwrap())
            .unwrap();

        assert!(current_session_from(&store).unwrap().is_none());
        assert_eq!(*store.deletes.borrow(), 1);
    }

    #[test]
    fn the_credential_entry_is_shared_with_wolf2_desktop() {
        // Spec §6: one sign-in serves Desktop and the tray.
        assert_eq!(CREDENTIAL_SERVICE, "net.wolf2.desktop");
        assert_eq!(DISCORD_CREDENTIAL_ACCOUNT, "discord-session");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_credential_manager_round_trips_a_desktop_session() {
        let account = format!("test-{}-{}", std::process::id(), now_ms());
        let entry = keyring::Entry::new("net.wolf2.desktop.test", &account).unwrap();
        entry.set_password("ephemeral-session").unwrap();
        assert_eq!(entry.get_password().unwrap(), "ephemeral-session");
        entry.delete_credential().unwrap();
        assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry)));
    }
}
