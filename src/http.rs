//! Minimal HTTP client over the `curl` binary.
//!
//! Shelling out keeps the build free of TLS stack dependencies (important
//! for the Android cross-compile) and reuses the credentials/CA setup curl
//! already has. Bodies travel over stdin/stdout so binary payloads and long
//! URLs are safe.

use std::io::Write;
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
        // The body goes to a temp file and stdout carries only the `-w`
        // write-out: some curl builds retry a transfer internally (e.g. when
        // TLS 1.3 early data is rejected — OAuth endpoints always refuse it)
        // and then emit the write-out TWICE, once as "000" for the aborted
        // attempt. Writing the body through stdout would glue that garbage
        // onto the payload; a file keeps them isolated, and the LAST status
        // line is the authoritative one for the retry.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let body_path = std::env::temp_dir().join(format!(
            "tuimu-http-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let mut command = Command::new(self.curl_bin.clone().unwrap_or_else(curl_bin));
        command.args(["-sS", "--max-time", &self.timeout_secs.to_string(), "-o"]);
        command.arg(&body_path);
        command.arg("-w");
        command.arg("\n%{http_code}");
        if let Some(method) = self.method {
            command.args(["-X", method]);
        }
        for (key, value) in &self.headers {
            command.arg(format!("{key}: {value}"));
        }
        if self.body.is_some() {
            command.arg("--data-binary");
            command.arg("@-");
        }
        command.arg(self.full_url());
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .context("could not start curl (is it installed and in PATH?)")?;
        if let Some(body) = &self.body {
            let stdin = child.stdin.as_mut().expect("piped stdin");
            stdin
                .write_all(body)
                .and_then(|_| stdin.flush())
                .context("could not send request body through curl")?;
        }
        drop(child.stdin.take());

        let output = child
            .wait_with_output()
            .context("could not wait for curl")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Truncate very long stderr so error messages stay readable.
            let stderr = &stderr[..stderr.len().min(400)];
            bail!("curl failed ({}): {}", output.status, stderr.trim());
        }

        let stdout = output.stdout;
        let body = std::fs::read(&body_path).unwrap_or_default();
        let _ = std::fs::remove_file(&body_path);

        // The write-out stream is "\n<code>" per transfer; the last line is
        // the final transfer's status. "000" means curl never got a
        // response at all.
        let status_line = String::from_utf8_lossy(&stdout);
        let lines: Vec<&str> = status_line
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let status = lines
            .last()
            .and_then(|line| line.trim().parse::<u16>().ok())
            .with_context(|| {
                format!(
                    "curl returned no HTTP status (write-out: {status_line:?}, body starts with: {})",
                    String::from_utf8_lossy(&body[..body.len().min(80)])
                )
            })?;
        Ok(Response { status, body })
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

    // Regression: some curl builds retry a transfer after rejected TLS
    // early data and emit the `-w` write-out twice ("000" for the aborted
    // attempt, then the real status). The body must stay untouched and the
    // LAST status must win.
    #[test]
    fn doubled_writeout_keeps_body_and_takes_last_status() {
        let script = std::env::temp_dir().join("tuimu-fake-curl-retry");
        std::fs::write(
            &script,
            "#!/usr/bin/env bash\nset -e\nout=\"\"\nprev=\"\"\nfor arg in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then out=\"$arg\"; fi\n  prev=\"$arg\"\ndone\nprintf '{\"access_token\":\"x\"}' > \"$out\"\nprintf '\\n000\\n200'\n",
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

    #[cfg(unix)]
    fn make_executable(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &std::path::Path) {}
}
