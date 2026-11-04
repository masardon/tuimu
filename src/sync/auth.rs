//! Dropbox OAuth 2.0 pairing with PKCE (no client secret, RFC 8252).
//!
//! Flow: the app opens the browser (`termux-open-url` on Termux,
//! `xdg-open` elsewhere) at the authorize URL with a loopback redirect;
//! the provider lands on `http://127.0.0.1:8377/cb`, which we catch with a
//! single-use listener. A manual mode (paste the code) covers headless
//! setups such as SSH sessions into Termux.
//!
//! Android caveat: loopback is shared by every app on the device, so the
//! listener is single-use, bound to 127.0.0.1 only, guarded by a random
//! `state`, and shut down immediately — and PKCE makes an intercepted code
//! useless without the in-memory verifier.

// The pairing/browser chain is compiled out in tests (no real browser,
// listener, or network during cargo test).
#![cfg_attr(test, allow(dead_code))]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::http;

/// Dropbox app key registered for TuiMu (app-folder scoped, PKCE enabled).
/// `TUIMU_DROPBOX_CLIENT_ID` overrides it during development.
pub fn dropbox_client_id() -> String {
    std::env::var("TUIMU_DROPBOX_CLIENT_ID")
        .ok()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "hs3jv1yr7b628s8".into())
}

/// The redirect must match the app registration exactly (loopback, fixed
/// port; Dropbox rejects random ports).
pub const REDIRECT_PORT: u16 = 8377;
pub const REDIRECT_URI: &str = "http://127.0.0.1:8377/cb";
const AUTH_ENDPOINT: &str = "https://www.dropbox.com/oauth2/authorize";
const TOKEN_ENDPOINT: &str = "https://api.dropboxapi.com/oauth2/token";
const LISTEN_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    /// Unix epoch seconds when the access token expires.
    pub expires_at: u64,
}

impl Tokens {
    pub fn needs_refresh(&self, now: u64) -> bool {
        self.expires_at.saturating_sub(now) < 60
    }
}

/// A pairing attempt in progress: holds the PKCE verifier until the code is
/// exchanged, then is dropped.
pub struct PendingAuth {
    verifier: String,
    state: String,
}

#[derive(Debug)]
pub enum AuthOutcome {
    Success(Tokens),
    /// User closed the wizard / the browser never came back.
    Cancelled,
    Failed(String),
}

pub fn begin() -> PendingAuth {
    PendingAuth {
        verifier: random_token(64),
        state: random_token(24),
    }
}

impl PendingAuth {
    /// PKCE verifier (kept by the wizard for the manual fallback).
    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    pub fn authorize_url(&self) -> String {
        format!(
            "{AUTH_ENDPOINT}?client_id={}&response_type=code&code_challenge={}&code_challenge_method=S256&redirect_uri={}&state={}&token_access_type=offline",
            dropbox_client_id(),
            self.challenge(),
            urlencoding::encode(REDIRECT_URI),
            self.state,
        )
    }

    fn challenge(&self) -> String {
        pkce_challenge(&self.verifier)
    }

    /// Open the authorize URL in the system browser. Returns false when no
    /// browser opener exists (the wizard falls back to manual mode).
    pub fn open_browser(&self) -> bool {
        let url = self.authorize_url();
        let opener = if std::env::var_os("TERMUX_VERSION").is_some() {
            "termux-open-url"
        } else {
            "xdg-open"
        };
        Command::new(opener)
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Wait for the loopback redirect. Single connection, then the listener
    /// is gone. The state must match; anything else is refused.
    pub fn wait_for_callback(&self) -> AuthOutcome {
        let Some(listener) = bind_loopback() else {
            return AuthOutcome::Failed(format!("port {REDIRECT_PORT} is busy"));
        };
        let deadline = Instant::now() + LISTEN_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                return AuthOutcome::Cancelled;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    let Some(request) = read_request_line(&stream) else {
                        continue;
                    };
                    let _ = respond_ok(&stream);
                    return match parse_code_and_state(&request) {
                        Some((code, state)) if state == self.state => self.exchange(&code),
                        _ => AuthOutcome::Failed("unexpected redirect from the browser".into()),
                    };
                }
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }

    fn exchange(&self, code: &str) -> AuthOutcome {
        match exchange_code_for_tokens(code, &self.verifier) {
            Ok(tokens) => AuthOutcome::Success(tokens),
            Err(error) => AuthOutcome::Failed(format!("{error:#}")),
        }
    }
}

fn bind_loopback() -> Option<TcpListener> {
    TcpListener::bind(("127.0.0.1", REDIRECT_PORT)).ok()
}

fn read_request_line(stream: &TcpStream) -> Option<String> {
    let mut reader = stream.try_clone().ok()?;
    reader.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buffer = [0u8; 2048];
    let read = reader.read(&mut buffer).ok()?;
    String::from_utf8_lossy(&buffer[..read]).into_owned().into()
}

fn respond_ok(stream: &TcpStream) -> std::io::Result<()> {
    let body = "<html><body><h2>Paired with TuiMu.</h2>You can close this tab and return to the app.</body></html>";
    let mut writer = stream.try_clone()?;
    write!(
        writer,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

/// Extract `code` and `state` from `GET /cb?code=..&state=..`.
fn parse_code_and_state(request: &str) -> Option<(String, String)> {
    let path = request.split_whitespace().nth(1)?;
    let query = path.split_once('?')?.1;
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        match key {
            "code" => code = Some(value.to_string()),
            "state" => state = Some(value.to_string()),
            _ => {}
        }
    }
    Some((code?, state?))
}

pub fn exchange_code_for_tokens(code: &str, verifier: &str) -> Result<Tokens> {
    let body = form_urlencoded(&[
        ("code", code),
        ("grant_type", "authorization_code"),
        ("code_verifier", verifier),
        ("client_id", &dropbox_client_id()),
        ("redirect_uri", REDIRECT_URI),
    ]);
    request_tokens(&body)
}

/// Refresh the access token with the stored refresh token.
pub fn refresh_tokens(refresh: &str) -> Result<Tokens> {
    let body = form_urlencoded(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
        ("client_id", &dropbox_client_id()),
    ]);
    request_tokens(&body)
}

fn request_tokens(body: &str) -> Result<Tokens> {
    let response = http::Request::new(TOKEN_ENDPOINT)
        .method("POST")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body.as_bytes().to_vec())
        .send()
        .context("could not reach the Dropbox token endpoint")?;
    if !response.is_success() {
        bail!(
            "Dropbox rejected the token request (HTTP {})",
            response.status
        );
    }
    let value: serde_json::Value =
        serde_json::from_slice(&response.body).context("invalid token response")?;
    let access = value
        .get("access_token")
        .and_then(|v| v.as_str())
        .context("token response has no access_token")?
        .to_string();
    let refresh = value
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .context("token response has no refresh_token")?
        .to_string();
    let expires_in = value
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(14400);
    Ok(Tokens {
        access,
        refresh,
        expires_at: crate::model::now_epoch() + expires_in,
    })
}

pub fn form_urlencoded(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", key, urlencoding::encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Random base64url token from `/dev/urandom` (fallback: time + pid hash,
/// still unguessable enough for `state`, never used for keys).
fn random_token(chars: usize) -> String {
    let wanted = chars * 3 / 4 + 1;
    if let Ok(mut file) = fs::File::open("/dev/urandom") {
        let mut bytes = vec![0u8; wanted];
        if file.read_exact(&mut bytes).is_ok() {
            let token = URL_SAFE_NO_PAD.encode(&bytes);
            return token.chars().take(chars).collect();
        }
    }
    let entropy = format!("{}-{}", crate::model::now_epoch(), std::process::id());
    let digest = Sha256::digest(entropy.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

pub fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

// ---- token persistence ----------------------------------------------------

pub fn token_path() -> PathBuf {
    crate::playlist::data_dir()
        .join("tokens")
        .join("dropbox.json")
}

pub fn save_tokens_at(path: &std::path::Path, tokens: &Tokens) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("could not create the tokens directory")?;
    }
    let json = serde_json::to_string(tokens).context("could not encode tokens")?;
    fs::write(path, json).context("could not write the token file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn load_tokens_at(path: &std::path::Path) -> Option<Tokens> {
    let json = fs::read_to_string(path).ok()?;
    serde_json::from_str(&json).ok()
}

pub fn clear_tokens_at(path: &std::path::Path) {
    let _ = fs::remove_file(path);
}

pub fn current_access_token_at(path: &std::path::Path) -> Result<String> {
    let mut tokens = load_tokens_at(path).context("this device is not paired for sync")?;
    if tokens.needs_refresh(crate::model::now_epoch()) {
        tokens = refresh_tokens(&tokens.refresh).context("could not refresh the sync token")?;
        save_tokens_at(path, &tokens)?;
    }
    Ok(tokens.access)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_contains_flow_parameters() {
        let pending = begin();
        let url = pending.authorize_url();
        assert!(url.starts_with("https://www.dropbox.com/oauth2/authorize?"));
        assert!(url.contains(&format!("client_id={}", dropbox_client_id())));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!(
            "code_challenge={}",
            pkce_challenge(&pending.verifier)
        )));
        assert!(url.contains(&format!("state={}", pending.state)));
        assert!(url.contains("token_access_type=offline"));
        assert!(url.contains(urlencoding::encode(REDIRECT_URI).as_ref()));
    }

    #[test]
    fn challenge_is_s256_of_verifier() {
        let pending = begin();
        assert_eq!(pending.challenge().len(), 43); // sha256 base64url, unpadded
        assert_ne!(pending.challenge(), pending.verifier);
        assert_eq!(pending.challenge(), pkce_challenge(&pending.verifier));
    }

    #[test]
    fn parses_code_and_state_from_callback() {
        let parsed = parse_code_and_state("GET /cb?code=ABC&state=xyz HTTP/1.1\r\nHost: x");
        assert_eq!(parsed, Some(("ABC".into(), "xyz".into())));
        assert_eq!(parse_code_and_state("GET /cb HTTP/1.1"), None);
        assert_eq!(parse_code_and_state("GET /cb?code=ABC HTTP/1.1"), None);
    }

    #[test]
    fn form_body_is_url_encoded() {
        let body = form_urlencoded(&[("a", "1 2"), ("b", "x&y")]);
        assert_eq!(body, "a=1%202&b=x%26y");
    }

    #[test]
    fn tokens_roundtrip_and_report_expiry() {
        let path = std::env::temp_dir().join(format!("tuimu-auth-tokens-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let tokens = Tokens {
            access: "at".into(),
            refresh: "rt".into(),
            expires_at: crate::model::now_epoch() + 100,
        };
        save_tokens_at(&path, &tokens).expect("save");
        assert_eq!(load_tokens_at(&path).expect("load").access, "at");
        assert!(!tokens.needs_refresh(crate::model::now_epoch()));
        assert!(tokens.needs_refresh(tokens.expires_at));
        clear_tokens_at(&path);
        assert!(load_tokens_at(&path).is_none());
    }

    #[test]
    fn loopback_catches_the_real_callback() {
        let pending = begin();
        // Exchange would hit the network; instead check the listener side
        // only by validating state handling with a raw connection.
        let listener = bind_loopback().expect("port free");
        let state = pending.state.clone();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let request = read_request_line(&stream).expect("read");
            let _ = respond_ok(&stream);
            request
        });
        let mut client = TcpStream::connect(("127.0.0.1", REDIRECT_PORT)).expect("connect");
        client
            .write_all(format!("GET /cb?code=CODE&state={state} HTTP/1.1\r\n\r\n").as_bytes())
            .expect("write");
        let request = handle.join().expect("thread");
        assert_eq!(parse_code_and_state(&request), Some(("CODE".into(), state)));
    }
}
