//! Dropbox RemoteStore: RPC-style API v2 over the shared curl wrapper.
//!
//! The token is for an *app folder* app, so every path is already relative
//! to `/Apps/TuiMu/` — the store physically cannot see the rest of the
//! account. Uploads use content revisions (`mode: update`) for the engine's
//! optimistic concurrency; a 409 with a conflict reason maps to
//! [`StoreError::Conflict`].

// The transport methods are only exercised against real Dropbox in
// production; tests cover the pure helpers.
#![cfg_attr(test, allow(dead_code))]

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::{RemoteFile, RemoteStore, StoreError};
use crate::http;
use crate::sync::auth;

const RPC_ROOT: &str = "https://api.dropboxapi.com";
const CONTENT_ROOT: &str = "https://content.dropboxapi.com";

pub struct DropboxStore {
    access: String,
    token_path: PathBuf,
}

impl DropboxStore {
    pub fn new() -> Result<Self> {
        Self::at(&auth::token_path())
    }

    pub fn at(token_path: &Path) -> Result<Self> {
        Ok(Self {
            access: auth::current_access_token_at(token_path)?,
            token_path: token_path.to_path_buf(),
        })
    }

    fn refresh(&mut self) -> Result<()> {
        self.access = auth::current_access_token_at(&self.token_path)?;
        Ok(())
    }

    fn rpc(&mut self, endpoint: &str, body: &serde_json::Value) -> Result<http::Response> {
        let send = |token: &str| {
            http::Request::new(&format!("{RPC_ROOT}{endpoint}"))
                .method("POST")
                .bearer(token)
                .header("Content-Type", "application/json")
                .body(body.to_string().into_bytes())
                .send()
        };
        let mut response = send(&self.access).context("could not reach Dropbox")?;
        if response.status == 401 {
            self.refresh()?;
            response = send(&self.access).context("could not reach Dropbox")?;
        }
        Ok(response)
    }

    fn content(
        &mut self,
        endpoint: &str,
        api_arg: &serde_json::Value,
        bytes: Option<&[u8]>,
    ) -> Result<http::Response> {
        let send = |token: &str| {
            let url = format!("{CONTENT_ROOT}{endpoint}");
            let mut request = http::Request::new(&url)
                .method("POST")
                .bearer(token)
                .header("Dropbox-API-Arg", api_arg.to_string())
                .header("Content-Type", "application/octet-stream");
            request = match bytes {
                Some(bytes) => request.body(bytes.to_vec()),
                None => request,
            };
            request.send()
        };
        let mut response = send(&self.access).context("could not reach Dropbox")?;
        if response.status == 401 {
            self.refresh()?;
            response = send(&self.access).context("could not reach Dropbox")?;
        }
        Ok(response)
    }
}

impl Default for DropboxStore {
    fn default() -> Self {
        Self::new().unwrap_or_else(|_| Self {
            access: String::new(),
            token_path: auth::token_path(),
        })
    }
}

/// Dropbox paths start with `/` (or are empty for the app folder root).
fn dropbox_path(path: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!("/{path}")
    }
}

fn upload_arg(path: &str, base_rev: Option<&str>) -> serde_json::Value {
    match base_rev {
        Some(rev) => serde_json::json!({
            "path": dropbox_path(path),
            "mode": {".tag": "update", "update": rev},
            "autorename": false,
            "mute": true,
        }),
        None => serde_json::json!({
            "path": dropbox_path(path),
            "mode": {".tag": "add"},
            "autorename": false,
            "mute": true,
        }),
    }
}

/// Map a failed Dropbox response (status + parsed error tag) to our error
/// shape. `not_found` means the file is simply gone.
fn map_error(context: &str, status: u16, body: &[u8]) -> StoreError {
    let tag = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("tag"))
                .and_then(|tag| tag.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    match (status, tag.as_str()) {
        (409, "conflict") => StoreError::Conflict,
        (409, "not_found") | (404, _) => StoreError::Other(anyhow::anyhow!("{context}: not found")),
        (401, _) => StoreError::Other(anyhow::anyhow!(
            "{context}: not authorized (re-pair with Y)"
        )),
        (429, _) => StoreError::Other(anyhow::anyhow!("{context}: rate limited, retry later")),
        _ => {
            let excerpt = String::from_utf8_lossy(body);
            let excerpt: String = excerpt.chars().take(160).collect();
            StoreError::Other(anyhow::anyhow!("{context}: HTTP {status}: {excerpt:?}"))
        }
    }
}

impl RemoteStore for DropboxStore {
    fn list(&mut self) -> Result<Vec<RemoteFile>, StoreError> {
        let mut files = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let (endpoint, body) = match &cursor {
                None => (
                    "/2/files/list_folder",
                    serde_json::json!({"path": "", "recursive": true, "limit": 500}),
                ),
                Some(cursor) => (
                    "/2/files/list_folder/continue",
                    serde_json::json!({"cursor": cursor}),
                ),
            };
            let response = self.rpc(endpoint, &body).map_err(StoreError::from)?;
            if !response.is_success() {
                return Err(map_error(
                    "could not list files",
                    response.status,
                    &response.body,
                ));
            }
            let value: serde_json::Value =
                serde_json::from_slice(&response.body).map_err(|error| {
                    StoreError::Other(anyhow::anyhow!("invalid list response: {error}"))
                })?;
            for entry in value
                .get("entries")
                .and_then(|e| e.as_array())
                .unwrap_or(&Vec::new())
            {
                if entry.get(".tag").and_then(|t| t.as_str()) != Some("file") {
                    continue;
                }
                let (Some(path), Some(rev)) = (
                    entry.get("path_display").and_then(|p| p.as_str()),
                    entry.get("rev").and_then(|r| r.as_str()),
                ) else {
                    continue;
                };
                files.push(RemoteFile {
                    // Engine paths are relative, without the leading slash.
                    path: path.trim_start_matches('/').to_string(),
                    rev: rev.to_string(),
                });
            }
            cursor = value
                .get("has_more")
                .and_then(|m| m.as_bool())
                .unwrap_or(false)
                .then(|| {
                    value
                        .get("cursor")
                        .and_then(|c| c.as_str())
                        .unwrap_or_default()
                        .to_string()
                });
            if cursor.is_none() {
                break;
            }
        }
        Ok(files)
    }

    fn download(&mut self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let arg = serde_json::json!({"path": dropbox_path(path)});
        let response = self
            .content("/2/files/download", &arg, None)
            .map_err(StoreError::from)?;
        match response.status {
            200..=299 => Ok(Some(response.body)),
            409 | 404 if is_not_found(&response.body) => Ok(None),
            _ => Err(map_error(
                "could not download",
                response.status,
                &response.body,
            )),
        }
    }

    fn upload(
        &mut self,
        path: &str,
        bytes: &[u8],
        base_rev: Option<&str>,
    ) -> Result<String, StoreError> {
        let arg = upload_arg(path, base_rev);
        let response = self
            .content("/2/files/upload", &arg, Some(bytes))
            .map_err(StoreError::from)?;
        if !response.is_success() {
            return Err(map_error(
                "could not upload",
                response.status,
                &response.body,
            ));
        }
        let value: serde_json::Value = serde_json::from_slice(&response.body).map_err(|error| {
            StoreError::Other(anyhow::anyhow!("invalid upload response: {error}"))
        })?;
        value
            .get("rev")
            .and_then(|r| r.as_str())
            .map(str::to_string)
            .ok_or_else(|| StoreError::Other(anyhow::anyhow!("upload response has no rev")))
    }

    fn delete(&mut self, path: &str) -> Result<(), StoreError> {
        let body = serde_json::json!({"path": dropbox_path(path)});
        let response = self
            .rpc("/2/files/delete_v2", &body)
            .map_err(StoreError::from)?;
        match response.status {
            200..=299 => Ok(()),
            409 | 404 if is_not_found(&response.body) => Ok(()),
            _ => Err(map_error(
                "could not delete",
                response.status,
                &response.body,
            )),
        }
    }
}

fn is_not_found(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("tag"))
                .and_then(|tag| tag.as_str())
                .map(|tag| tag == "not_found" || tag == "path_lookup/not_found")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live diagnostic against the real Dropbox API using this device's
    /// pairing. Run explicitly with:
    /// `cargo test --release live_dropbox_list -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the real Dropbox API"]
    fn live_dropbox_list() {
        let mut store = DropboxStore::at(&crate::sync::auth::token_path()).expect("device paired");
        match store.list() {
            Ok(files) => println!("list OK: {} files", files.len()),
            Err(error) => panic!("list failed: {error}"),
        }
    }

    /// Live proof that refresh carries over the existing refresh token:
    /// `cargo test --release live_refresh -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the real Dropbox API"]
    fn live_refresh() {
        let tokens = crate::sync::auth::load_tokens_at(&crate::sync::auth::token_path())
            .expect("device paired");
        let refreshed =
            crate::sync::auth::refresh_tokens(&tokens.refresh).expect("refresh succeeds");
        assert_eq!(refreshed.refresh, tokens.refresh);
        println!(
            "refresh OK: access rotated, refresh token stable ({} chars)",
            refreshed.refresh.len()
        );
    }

    #[test]
    fn paths_gain_leading_slash() {
        assert_eq!(dropbox_path(""), String::new());
        assert_eq!(dropbox_path("playlists/mix.json"), "/playlists/mix.json");
    }

    #[test]
    fn upload_args_select_update_or_add() {
        let update = upload_arg("playlists/a.json", Some("rev9"));
        assert_eq!(update["mode"][".tag"], "update");
        assert_eq!(update["mode"]["update"], "rev9");
        assert_eq!(update["path"], "/playlists/a.json");
        assert_eq!(update["autorename"], false);

        let add = upload_arg("playlists/a.json", None);
        assert_eq!(add["mode"][".tag"], "add");
    }

    #[test]
    fn conflict_and_not_found_map_cleanly() {
        let conflict = serde_json::json!({"error": {"tag": "conflict"}}).to_string();
        assert!(matches!(
            map_error("x", 409, conflict.as_bytes()),
            StoreError::Conflict
        ));

        let missing = serde_json::json!({"error": {"tag": "not_found"}}).to_string();
        assert!(is_not_found(missing.as_bytes()));
        assert!(!is_not_found(conflict.as_bytes()));

        let opaque = b"gateway nonsense";
        assert!(matches!(map_error("x", 502, opaque), StoreError::Other(_)));
    }
}
