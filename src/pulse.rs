//! PulseAudio daemon management for Termux (Android).
//!
//! On Termux there is no system audio server to rely on: the `pulseaudio`
//! package ships a per-user daemon that clients usually autospawn when
//! they connect. Autospawn can be disabled (`autospawn=no` in daemon.conf)
//! or racy when several processes start it at once, so before selecting
//! pulse output we make sure the daemon is up. `pulseaudio --check` is a
//! cheap no-op when a daemon already runs, so this normally costs one
//! short-lived process per mpv launch.

use std::env;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const START_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Running under Termux, where playback must flow through the per-user
/// pulseaudio daemon for the visualizer to see it.
pub fn is_termux() -> bool {
    env::var_os("TERMUX_VERSION").is_some()
}

/// The pulseaudio binary to talk to (`TUIMU_PULSEAUDIO` overrides).
pub fn bin() -> String {
    env::var("TUIMU_PULSEAUDIO").unwrap_or_else(|_| "pulseaudio".into())
}

/// Make sure a daemon is running, starting one when needed. Returns false
/// when pulseaudio is not installed or did not come up within the timeout;
/// callers should then avoid pulse output so playback keeps working.
pub fn ensure_daemon() -> bool {
    ensure_daemon_with(&bin())
}

fn silenced(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
}

fn daemon_running_with(bin: &str) -> bool {
    let mut command = Command::new(bin);
    silenced(command.arg("--check"));
    command.status().map(|s| s.success()).unwrap_or(false)
}

fn ensure_daemon_with(bin: &str) -> bool {
    if daemon_running_with(bin) {
        return true;
    }
    // Persistent daemon: it must survive while tuimu (and its mpv) runs.
    // Errors are ignored on purpose; the poll below is the source of truth.
    let mut command = Command::new(bin);
    silenced(command.args(["--start", "--exit-idle-time=-1"]));
    let _ = command.status();

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if daemon_running_with(bin) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // /system/bin binaries exist on every Android device and exit with a
    // fixed status, which makes them stand-ins for a working or broken
    // pulseaudio installation.
    #[test]
    fn missing_or_failing_binary_reports_no_daemon() {
        assert!(!daemon_running_with("/system/bin/false"));
    }

    #[test]
    fn ensure_with_failing_binary_gives_up() {
        assert!(!ensure_daemon_with("/system/bin/false"));
    }

    #[test]
    fn zero_exit_reports_daemon() {
        assert!(daemon_running_with("/system/bin/true"));
    }

    #[test]
    fn nonexistent_binary_reports_no_daemon() {
        assert!(!daemon_running_with("/nonexistent/pulseaudio"));
    }
}
