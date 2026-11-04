use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

#[derive(Debug)]
pub enum MpvMessage {
    Event(Value),
    Closed(String),
}

/// Playback engine operations used by the app, so the UI logic can be tested
/// against a fake player instead of a real mpv process.
pub trait Player {
    fn try_recv(&self) -> Result<MpvMessage, TryRecvError>;
    fn load(&mut self, url: &str, headers: &[(String, String)]) -> Result<()>;
    fn toggle_pause(&mut self) -> Result<()>;
    fn seek_relative(&mut self, seconds: f64) -> Result<()>;
    fn set_volume(&mut self, volume: f64) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
}

impl Player for Mpv {
    fn try_recv(&self) -> Result<MpvMessage, TryRecvError> {
        Mpv::try_recv(self)
    }

    fn load(&mut self, url: &str, headers: &[(String, String)]) -> Result<()> {
        Mpv::load(self, url, headers)
    }

    fn toggle_pause(&mut self) -> Result<()> {
        Mpv::toggle_pause(self)
    }

    fn seek_relative(&mut self, seconds: f64) -> Result<()> {
        Mpv::seek_relative(self, seconds)
    }

    fn set_volume(&mut self, volume: f64) -> Result<()> {
        Mpv::set_volume(self, volume)
    }

    fn stop(&mut self) -> Result<()> {
        Mpv::stop(self)
    }
}

struct MpvProcess {
    child: Child,
    stream: UnixStream,
    rx: Receiver<MpvMessage>,
    socket_path: PathBuf,
}

impl MpvProcess {
    fn try_recv(&self) -> Result<MpvMessage, TryRecvError> {
        self.rx.try_recv()
    }

    fn send(&mut self, value: Value) -> Result<()> {
        serde_json::to_writer(&mut self.stream, &value).context("could not encode mpv command")?;
        self.stream
            .write_all(b"\n")
            .context("could not write to mpv IPC socket")?;
        self.stream.flush().context("could not flush mpv command")
    }
}

impl Drop for MpvProcess {
    fn drop(&mut self) {
        let _ = self.send(json!({"command": ["quit"]}));
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.socket_path);
    }
}

pub struct Mpv {
    process: MpvProcess,
    header_fields: Vec<String>,
    spawn_seq: u64,
}

impl Mpv {
    pub fn spawn() -> Result<Self> {
        Ok(Self {
            process: spawn_process(&[], 0)?,
            header_fields: Vec::new(),
            spawn_seq: 0,
        })
    }

    pub fn try_recv(&self) -> Result<MpvMessage, TryRecvError> {
        self.process.try_recv()
    }

    pub fn load(&mut self, url: &str, headers: &[(String, String)]) -> Result<()> {
        // mpv does not apply runtime changes to http-header-fields reliably to
        // the initial HTTP request (and loadfile options are applied too late),
        // so relaunch mpv with the headers on the command line when the source
        // changes. Only Referer/Origin are kept: other yt-dlp headers contain
        // commas, which mpv treats as separators in a header-field list.
        let fields = header_fields(headers);
        if fields != self.header_fields {
            let _ = self.process.send(json!({"command": ["stop"]}));
            self.spawn_seq = self.spawn_seq.wrapping_add(1);
            let process = spawn_process(&fields, self.spawn_seq)?;
            let old = std::mem::replace(&mut self.process, process);
            drop(old);
            self.header_fields = fields;
        }

        self.process
            .send(json!({"command": ["loadfile", url, "replace"]}))?;
        self.process
            .send(json!({"command": ["set_property", "pause", false]}))?;
        Ok(())
    }

    pub fn toggle_pause(&mut self) -> Result<()> {
        self.process.send(json!({"command": ["cycle", "pause"]}))
    }

    pub fn seek_relative(&mut self, seconds: f64) -> Result<()> {
        self.process
            .send(json!({"command": ["seek", seconds, "relative"]}))
    }

    pub fn set_volume(&mut self, volume: f64) -> Result<()> {
        self.process
            .send(json!({"command": ["set_property", "volume", volume.clamp(0.0, 100.0)]}))
    }

    pub fn stop(&mut self) -> Result<()> {
        self.process.send(json!({"command": ["stop"]}))
    }
}

/// The mpv audio output to force, if any.
fn select_ao() -> Option<String> {
    ao_choice(
        env::var("TUIMU_AO").ok().filter(|ao| !ao.is_empty()),
        crate::pulse::is_termux(),
        crate::pulse::ensure_daemon,
    )
}

/// Pure core of [`select_ao`] so the policy can be tested without a real
/// Termux or daemon: an explicit override always wins, Termux gets pulse
/// output only when a daemon can be provided, everyone else keeps mpv's
/// default.
fn ao_choice(
    explicit: Option<String>,
    termux: bool,
    daemon_up: impl FnOnce() -> bool,
) -> Option<String> {
    if let Some(ao) = explicit {
        return Some(ao);
    }
    if termux && daemon_up() {
        return Some("pulse".into());
    }
    None
}

fn header_fields(headers: &[(String, String)]) -> Vec<String> {
    let mut fields: Vec<String> = headers
        .iter()
        .filter(|(key, _)| {
            key.eq_ignore_ascii_case("referer") || key.eq_ignore_ascii_case("origin")
        })
        .map(|(key, value)| format!("{key}: {value}"))
        .collect();
    fields.sort();
    fields
}

fn spawn_process(header_fields: &[String], seq: u64) -> Result<MpvProcess> {
    let bin = env::var("TUIMU_MPV").unwrap_or_else(|_| "mpv".into());
    let socket_path = env::temp_dir().join(format!("tuimu-mpv-{}-{seq}.sock", std::process::id()));
    let _ = fs::remove_file(&socket_path);

    let mut command = Command::new(&bin);
    command.args([
        "--no-config",
        "--idle=yes",
        "--no-video",
        "--no-terminal",
        "--force-window=no",
        "--keep-open=no",
        "--really-quiet",
        // Audio is extracted by yt-dlp in a worker thread and handed to mpv as
        // a direct URL. mpv's own ytdl hook drops the headers that YouTube
        // requires, which results in HTTP 403.
        "--ytdl=no",
        "--audio-display=no",
        "--volume=70",
    ]);
    // Audio output selection. On Termux (Android) mpv defaults to the
    // OpenSL ES / AAudio outputs, which talk to Android's AudioFlinger
    // directly and bypass the PulseAudio server entirely — leaving the
    // visualizer's `@DEFAULT_MONITOR@` capture silent. Routing playback
    // through the Termux pulseaudio daemon (module-sles-sink) makes both
    // audible playback and the sink monitor work. When the daemon cannot
    // be started (no pulseaudio package installed), keep mpv's default
    // output so playback still works without the visualizer.
    if let Some(ao) = select_ao() {
        command.arg(format!("--ao={ao}"));
    }
    if !header_fields.is_empty() {
        command.arg(format!("--http-header-fields={}", header_fields.join(",")));
    }
    command
        .arg(format!("--input-ipc-server={}", socket_path.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = command.spawn().with_context(|| {
        format!(
            "could not start `{bin}`. Install mpv and make sure it is in PATH (or set TUIMU_MPV)."
        )
    })?;

    let started = Instant::now();
    let stream = loop {
        match UnixStream::connect(&socket_path) {
            Ok(stream) => break stream,
            Err(error) => {
                if let Some(status) = child.try_wait().context("failed to check mpv status")? {
                    let _ = fs::remove_file(&socket_path);
                    bail!("mpv exited during startup ({status})");
                }
                if started.elapsed() > Duration::from_secs(5) {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = fs::remove_file(&socket_path);
                    bail!("could not connect to mpv IPC socket: {error}");
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    };

    let reader_stream = stream
        .try_clone()
        .context("could not clone mpv IPC stream")?;
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("mpv-ipc".into())
        .spawn(move || {
            let mut reader = BufReader::new(reader_stream);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let _ = tx.send(MpvMessage::Closed("mpv closed the IPC socket".into()));
                        break;
                    }
                    Ok(_) => {
                        if let Ok(event) = serde_json::from_str::<Value>(&line)
                            && tx.send(MpvMessage::Event(event)).is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(MpvMessage::Closed(format!("mpv IPC error: {error}")));
                        break;
                    }
                }
            }
        })
        .context("could not start mpv IPC reader")?;

    let mut process = MpvProcess {
        child,
        stream,
        rx,
        socket_path,
    };

    for (id, name) in [
        (1, "time-pos"),
        (2, "duration"),
        (3, "pause"),
        (4, "volume"),
    ] {
        process
            .send(json!({"command": ["observe_property", id, name]}))
            .context("could not subscribe to mpv properties")?;
    }

    Ok(process)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_ao_override_wins_everywhere() {
        assert_eq!(
            ao_choice(Some("opensles".into()), true, || panic!("unused")),
            Some("opensles".into())
        );
        assert_eq!(
            ao_choice(Some("alsa".into()), false, || panic!("unused")),
            Some("alsa".into())
        );
    }

    #[test]
    fn termux_gets_pulse_only_with_a_daemon() {
        assert_eq!(ao_choice(None, true, || true), Some("pulse".into()));
        assert_eq!(ao_choice(None, true, || false), None);
    }

    #[test]
    fn desktop_keeps_mpv_default_without_asking_the_daemon() {
        assert_eq!(ao_choice(None, false, || panic!("unused")), None);
    }

    #[test]
    fn keeps_only_safe_header_fields() {
        let headers = vec![
            ("User-Agent".into(), "a browser".into()),
            ("Accept".into(), "text/html,application/xhtml".into()),
            ("Referer".into(), "https://www.youtube.com/".into()),
        ];
        assert_eq!(
            header_fields(&headers),
            vec!["Referer: https://www.youtube.com/".to_string()]
        );
    }
}
