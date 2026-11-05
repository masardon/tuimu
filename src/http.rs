//! Minimal HTTP client over the `curl` binary.
//!
//! Shelling out keeps the build free of TLS stack dependencies (important
//! for the Android cross-compile) and reuses the credentials/CA setup curl
//! already has. Bodies travel over stdin/stdout so binary payloads and long
//! URLs are safe.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};

pub fn curl_bin() -> String {
    std::env::var("TUIMU_CURL").unwrap_or_else(|_| "curl".into())
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// A single HTTP request, built with the builder methods and sent with
/// [`Request::send`].
#[derive(Debug, Default)]
pub struct Request<'a> {
    method: Option<&'a str>,
    url: &'a str,
    headers: Vec<(String, String)>,
    query: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    timeout_secs: u64,
    curl_bin: Option<String>,
}

impl<'a> Request<'a> {
    pub fn new(url: &'a str) -> Self {
        Self {
            url,
            timeout_secs: 15,
            ..Self::default()
        }
    }

    /// Request verb other than GET.
    pub fn method(mut self, method: &'a str) -> Self {
        self.method = Some(method);
        self
    }

    /// Override the curl binary (tests; defaults to `TUIMU_CURL`/`curl`).
    #[cfg(test)]
    fn curl_bin(mut self, bin: &str) -> Self {
        self.curl_bin = Some(bin.to_string());
        self
    }

    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }

    /// Bearer auth header.
    #[cfg_attr(test, allow(dead_code))]
    pub fn bearer(self, token: impl Into<String>) -> Self {
        self.header("Authorization", format!("Bearer {}", token.into()))
    }

    /// Add a URL-encoded query parameter.
    pub fn query(mut self, key: &str, value: &str) -> Self {
        self.query
            .push((key.to_string(), urlencoding::encode(value).into_owned()));
        self
    }

    /// Request body (stdin, binary-safe).
    pub fn body(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.body = Some(bytes.into());
        self
    }

    pub fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    fn full_url(&self) -> String {
        if self.query.is_empty() {
            return self.url.to_string();
        }
        let query = self
            .query
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("&");
        format!("{}?{}", self.url, query)
    }

    pub fn send(self) -> Result<Response> {
        // Field-proven on Android: this device's curl streams the response
        // body to stdout even when `-o` is given (the -o file stays empty),
        // while `-D` header dumps work fine. So: body from stdout, status
        // from the header dump, and no `-w` write-out at all — nothing to
        // glue together. The request body travels via a file, not stdin.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = format!(
            "tuimu-http-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let dir = std::env::temp_dir();
        let headers_path = dir.join(format!("{id}.headers"));
        let request_body_path = dir.join(format!("{id}.req"));
        let mut command = Command::new(self.curl_bin.clone().unwrap_or_else(curl_bin));
        command.args([
            "-sS",
            "--http1.1",
            "--max-time",
            &self.timeout_secs.to_string(),
        ]);
        command.arg("-D");
        command.arg(&headers_path);
        if let Some(method) = self.method {
            command.args(["-X", method]);
        }
        for (key, value) in &self.headers {
            command.arg("-H");
            command.arg(format!("{key}: {value}"));
        }
        if let Some(body) = &self.body {
            std::fs::write(&request_body_path, body).context("could not stage the request body")?;
            command.arg("--data-binary");
            command.arg(format!("@{}", request_body_path.display()));
        }
        command.arg(self.full_url());
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .context("could not start curl (is it installed and in PATH?)")?;
        drop(child.stdin.take());

        let output = child
            .wait_with_output()
            .context("could not wait for curl")?;
        let body = output.stdout;
        let headers = std::fs::read_to_string(&headers_path).unwrap_or_default();
        // TUIMU_HTTP_DEBUG keeps the raw artifacts of every request so
        // transport mysteries can be diagnosed from the files themselves.
        let debug = std::env::var_os("TUIMU_HTTP_DEBUG").is_some();
        if !debug {
            let _ = std::fs::remove_file(&headers_path);
        } else {
            let _ = std::fs::write(headers_path.with_extension("stderr"), &output.stderr);
            let _ = std::fs::write(headers_path.with_extension("body"), &body);
        }
        let _ = std::fs::remove_file(&request_body_path);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Truncate very long stderr so error messages stay readable.
            let stderr = &stderr[..stderr.len().min(400)];
            bail!(
                "curl failed ({}): {}\nheaders: {}",
                output.status,
                stderr.trim(),
                excerpt(&headers)
            );
        }

        // Authoritative status: the last `HTTP/x.y NNN` line of the header
        // dump.
        let mut status: Option<u16> = None;
        for line in headers.lines().filter(|line| line.starts_with("HTTP/")) {
            if let Some(code) = line.split_whitespace().nth(1).and_then(|c| c.parse().ok()) {
                status = Some(code);
            }
        }
        let status = status.with_context(|| {
            format!(
                "curl returned no HTTP status (headers: {:?}, body starts with: {})",
                excerpt(&headers),
                String::from_utf8_lossy(&body[..body.len().min(80)])
            )
        })?;
        Ok(Response { status, body })
    }
}

fn excerpt(text: &str) -> String {
    let trimmed: String = text.chars().take(160).collect();
    if trimmed.is_empty() {
        "<empty>".into()
    } else {
        format!("{trimmed:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real localhost request needs a server; instead assert the builder
    // produces the URL and payload shapes we hand to curl.
    #[test]
    fn full_url_appends_encoded_query() {
        let request = Request::new("https://example.com/x")
            .query("url", "https://open.spotify.com/track/1?x= y")
            .query("plain", "value");
        assert_eq!(
            request.full_url(),
            "https://example.com/x?url=https%3A%2F%2Fopen.spotify.com%2Ftrack%2F1%3Fx%3D%20y&plain=value"
        );
    }

    #[test]
    fn bare_url_has_no_query_suffix() {
        assert_eq!(
            Request::new("https://example.com").full_url(),
            "https://example.com"
        );
    }

    #[test]
    fn missing_curl_reports_error() {
        // A nonexistent curl binary must produce a clean error, not a panic.
        let result = Request::new("https://example.com")
            .curl_bin("/nonexistent/curl-for-test")
            .send();
        assert!(result.is_err());
    }

    // The supported contract: `-D` headers file, body on stdout.
    #[test]
    fn body_from_stdout_status_from_header_dump() {
        let script = std::env::temp_dir().join("tuimu-fake-curl-stdout");
        std::fs::write(
            &script,
            "#!/usr/bin/env bash\nset -e\nhdr=\"\"\nprev=\"\"\nfor arg in \"$@\"; do\n  if [ \"$prev\" = \"-D\" ]; then hdr=\"$arg\"; fi\n  prev=\"$arg\"\ndone\nprintf 'HTTP/1.1 200 OK\\r\\nContent-Length: 22\\r\\n' > \"$hdr\"\nprintf '{\"access_token\":\"x\"}'\n",
        )
        .expect("write script");
        make_executable(&script);

        let response = Request::new("https://example.com")
            .curl_bin(script.to_str().unwrap())
            .send()
            .expect("send");
        assert_eq!(response.status, 200);
        assert_eq!(response.text(), "{\"access_token\":\"x\"}");
        let _ = std::fs::remove_file(&script);
    }

    // Multiple HTTP status lines (redirect-style dumps): the last one wins.
    #[test]
    fn last_status_line_wins() {
        let script = std::env::temp_dir().join("tuimu-fake-curl-hops");
        std::fs::write(
            &script,
            "#!/usr/bin/env bash\nset -e\nhdr=\"\"\nprev=\"\"\nfor arg in \"$@\"; do\n  if [ \"$prev\" = \"-D\" ]; then hdr=\"$arg\"; fi\n  prev=\"$arg\"\ndone\nprintf 'HTTP/1.1 302 Found\\r\\nHTTP/1.1 201 Created\\r\\n' > \"$hdr\"\nprintf 'ok'\n",
        )
        .expect("write script");
        make_executable(&script);

        let response = Request::new("https://example.com")
            .curl_bin(script.to_str().unwrap())
            .send()
            .expect("send");
        assert_eq!(response.status, 201);
        assert_eq!(response.text(), "ok");
        let _ = std::fs::remove_file(&script);
    }

    #[cfg(unix)]
    fn make_executable(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &std::path::Path) {}
}
