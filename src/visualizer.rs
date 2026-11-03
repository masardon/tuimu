//! Audio visualizer: captures the default output monitor via `parec`
//! (PulseAudio or PipeWire-pulse) and reduces it to 24 smoothed spectrum
//! bands for the TUI panel.
//!
//! mpv's IPC does not expose PCM data, so the system sink monitor is the
//! only dependency-free tap. The monitor carries all desktop audio, not
//! just tuimu's own playback.

use std::env;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

pub const BANDS: usize = 24;
const SAMPLE_RATE: u32 = 22_050;
const FFT_SIZE: usize = 512;
const FRAME_BYTES: usize = FFT_SIZE * 2; // s16le mono
const MIN_INTERVAL: Duration = Duration::from_millis(80);
const MIN_HZ: f32 = 50.0;
const MAX_HZ: f32 = 10_000.0;
/// Magnitude of the peak bin for a full-scale sine (1.0) through the Hann
/// window; used as the 0 dB reference for band levels.
const FULL_SCALE_MAGNITUDE: f32 = (FFT_SIZE as f32) / 4.0;
const FLOOR_DB: f32 = -48.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualizerStatus {
    /// Capture has not been started (or was stopped).
    Stopped,
    /// The capture thread is running.
    Running,
    /// `parec` is unavailable or exited; starting again retries.
    Unavailable,
}

struct CaptureProcess {
    child: Child,
}

impl Drop for CaptureProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Visualizer {
    status: VisualizerStatus,
    receiver: Option<Receiver<Vec<f32>>>,
    process: Option<CaptureProcess>,
}

impl Default for Visualizer {
    fn default() -> Self {
        Self {
            status: VisualizerStatus::Stopped,
            receiver: None,
            process: None,
        }
    }
}

impl Visualizer {
    pub fn status(&self) -> VisualizerStatus {
        self.status
    }

    /// Start capturing. No-op when already running; a fresh attempt when a
    /// previous one failed, so plugging in a pulse server later works.
    pub fn start(&mut self) {
        if self.status == VisualizerStatus::Running {
            return;
        }
        self.stop();

        let (tx, rx) = mpsc::channel();
        match spawn_capture(tx) {
            Some(process) => {
                self.receiver = Some(rx);
                self.process = Some(process);
                self.status = VisualizerStatus::Running;
            }
            None => self.status = VisualizerStatus::Unavailable,
        }
    }

    pub fn stop(&mut self) {
        self.process.take(); // Drop kills the child
        self.receiver.take();
        self.status = VisualizerStatus::Stopped;
    }

    /// Take the newest band frame, if any. A closed channel flips the state
    /// to `Unavailable` so the panel can explain instead of freezing.
    pub fn poll(&mut self) -> Option<Vec<f32>> {
        let mut latest = None;
        while let Some(rx) = &self.receiver {
            match rx.try_recv() {
                Ok(bands) => latest = Some(bands),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.receiver = None;
                    self.process.take();
                    self.status = VisualizerStatus::Unavailable;
                    break;
                }
            }
        }
        latest
    }
}

fn spawn_capture(tx: Sender<Vec<f32>>) -> Option<CaptureProcess> {
    let bin = env::var("TUIMU_PAREC").unwrap_or_else(|_| "parec".into());
    let mut command = Command::new(bin);
    command.args([
        "--raw",
        "--format=s16le",
        "--rate",
        &SAMPLE_RATE.to_string(),
        "--channels",
        "1",
        "--device",
        "@DEFAULT_MONITOR@",
    ]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let spawned = thread::Builder::new()
        .name("visualizer".into())
        .spawn(move || capture_loop(stdout, tx));
    if spawned.is_err() {
        return None; // dropping `child` kills the process
    }
    Some(CaptureProcess { child })
}

fn capture_loop(mut stdout: impl Read, tx: Sender<Vec<f32>>) {
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let window = hann_window(FFT_SIZE);
    let band_edges = band_bin_edges();

    let mut pending: Vec<u8> = Vec::with_capacity(FRAME_BYTES * 2);
    let mut chunk = [0u8; 2048];
    let mut last_emit = Instant::now()
        .checked_sub(MIN_INTERVAL)
        .expect("instant before now exists");

    loop {
        match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                pending.extend_from_slice(&chunk[..read]);
                // Keep at most a couple of windows so a slow consumer cannot
                // grow the buffer without bound.
                if pending.len() > FRAME_BYTES * 4 {
                    let excess = pending.len() - FRAME_BYTES * 4;
                    pending.drain(..excess);
                }
                if pending.len() >= FRAME_BYTES && last_emit.elapsed() >= MIN_INTERVAL {
                    let start = pending.len() - FRAME_BYTES;
                    let samples = decode_s16le(&pending[start..]);
                    pending.clear();
                    let bands = analyze(&samples, &window, fft.as_ref(), &band_edges);
                    if tx.send(bands).is_err() {
                        break; // receiver gone; app dropped us
                    }
                    last_emit = Instant::now();
                }
            }
        }
    }
}

fn decode_s16le(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let raw = i16::from_le_bytes(*pair);
            raw as f32 / 32768.0
        })
        .collect()
}

fn hann_window(size: usize) -> Vec<f32> {
    (0..size)
        .map(|i| {
            let x = i as f32 / (size - 1) as f32;
            0.5 * (1.0 - (std::f32::consts::TAU * x).cos())
        })
        .collect()
}

/// For each band, the half-open bin range [start, end) it aggregates.
fn band_bin_edges() -> [(usize, usize); BANDS] {
    let mut edges = [(0usize, 0usize); BANDS];
    let ratio = (MAX_HZ / MIN_HZ).powf(1.0 / BANDS as f32);
    let mut low_hz = MIN_HZ;
    let mut previous_bin = 0usize;
    for (index, band) in edges.iter_mut().enumerate() {
        let high_hz = low_hz * ratio;
        // The first band starts at DC so sub-bass bins are included.
        let low_bin = if index == 0 {
            0
        } else {
            bin_of(low_hz).max(previous_bin)
        };
        let high_bin = (bin_of(high_hz).max(low_bin + 1)).min(FFT_SIZE / 2);
        *band = (low_bin, high_bin);
        previous_bin = high_bin;
        low_hz = high_hz;
    }
    edges
}

fn bin_of(hz: f32) -> usize {
    (hz * FFT_SIZE as f32 / SAMPLE_RATE as f32).round() as usize
}

fn analyze(
    samples: &[f32],
    window: &[f32],
    fft: &dyn rustfft::Fft<f32>,
    band_edges: &[(usize, usize); BANDS],
) -> Vec<f32> {
    let mut buffer: Vec<Complex<f32>> = samples
        .iter()
        .zip(window)
        .map(|(sample, gain)| Complex::new(sample * gain, 0.0))
        .collect();
    fft.process(&mut buffer);

    let mut bands = vec![0.0f32; BANDS];
    for (index, (low, high)) in band_edges.iter().enumerate() {
        let mut best = 0.0f32;
        for bin in buffer[*low..*high].iter() {
            best = best.max(bin.norm());
        }
        bands[index] = level_of(best);
    }
    bands
}

/// Map a bin magnitude to a 0..1 band level on a fixed dB scale, where
/// 0 dB (full scale) is 1.0 and `FLOOR_DB` is 0.0.
fn level_of(magnitude: f32) -> f32 {
    let db = 20.0 * (magnitude / FULL_SCALE_MAGNITUDE + 1e-4).log10();
    (1.0 + db / -FLOOR_DB).clamp(0.0, 1.0)
}

/// Attack fast, release slow: a rising band jumps immediately, a falling
/// band decays ~12% per frame.
pub fn smooth(prev: &[f32], next: &[f32]) -> Vec<f32> {
    next.iter()
        .zip(prev)
        .map(|(&new, &old)| if new >= old { new } else { old * 0.88 })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f32, amplitude: f32) -> Vec<f32> {
        (0..FFT_SIZE)
            .map(|i| amplitude * (std::f32::consts::TAU * hz * i as f32 / SAMPLE_RATE as f32).sin())
            .collect()
    }

    fn analyze_samples(samples: &[f32]) -> Vec<f32> {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        analyze(
            samples,
            &hann_window(FFT_SIZE),
            fft.as_ref(),
            &band_bin_edges(),
        )
    }

    fn band_of_hz(hz: f32) -> usize {
        let edges = band_bin_edges();
        edges
            .iter()
            .position(|(low, high)| {
                let low_hz = *low as f32 * SAMPLE_RATE as f32 / FFT_SIZE as f32;
                let high_hz = *high as f32 * SAMPLE_RATE as f32 / FFT_SIZE as f32;
                hz >= low_hz && hz < high_hz
            })
            .unwrap_or(BANDS - 1)
    }

    #[test]
    fn decodes_s16le() {
        let samples = decode_s16le(&[0x00, 0x00, 0x00, 0x40, 0x00, 0xC0]);
        assert_eq!(samples[0], 0.0);
        assert!((samples[1] - 16384.0 / 32768.0).abs() < 1e-6);
        assert!((samples[2] + 16384.0 / 32768.0).abs() < 1e-6);
    }

    #[test]
    fn silence_produces_zero_bands() {
        let bands = analyze_samples(&vec![0.0; FFT_SIZE]);
        assert!(bands.iter().all(|band| *band < 0.01));
    }

    #[test]
    fn sine_peaks_its_own_band() {
        let bands = analyze_samples(&sine(1000.0, 0.5));
        let peak = bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        let expected = band_of_hz(1000.0);
        assert!(
            (peak as i32 - expected as i32).abs() <= 1,
            "peak band {peak}, expected near {expected} (bands: {bands:?})"
        );
        assert!(bands[peak] > 0.5);
    }

    #[test]
    fn bass_and_treble_land_apart() {
        let bass = analyze_samples(&sine(100.0, 0.5));
        let treble = analyze_samples(&sine(8000.0, 0.5));
        let bass_peak = bass
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        let treble_peak = treble
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        assert!(bass_peak < 5, "bass peak {bass_peak}");
        assert!(treble_peak > BANDS - 8, "treble peak {treble_peak}");
    }

    #[test]
    fn band_edges_are_monotonic() {
        let edges = band_bin_edges();
        assert_eq!(edges[0].0, 0);
        for window in edges.windows(2) {
            assert!(window[0].1 <= window[1].0, "edges overlap: {edges:?}");
            assert!(window[0].0 < window[0].1, "empty band: {edges:?}");
        }
        assert!(edges[BANDS - 1].1 <= FFT_SIZE / 2);
    }

    #[test]
    fn level_mapping_is_bounded() {
        assert_eq!(level_of(FULL_SCALE_MAGNITUDE), 1.0);
        assert_eq!(level_of(0.0), 0.0);
        assert!(level_of(FULL_SCALE_MAGNITUDE / 100.0) > 0.0);
        assert!(level_of(FULL_SCALE_MAGNITUDE / 100.0) < 0.5);
    }

    #[test]
    fn smoothing_attacks_fast_and_releases_slowly() {
        let prev = vec![0.0, 0.9, 0.5];
        let next = vec![0.8, 0.1, 0.5];
        let smoothed = smooth(&prev, &next);
        assert_eq!(smoothed[0], 0.8); // rising: immediate
        assert!((smoothed[1] - 0.9 * 0.88).abs() < 1e-6); // falling: decay
        assert_eq!(smoothed[2], 0.5); // steady: unchanged
    }

    #[test]
    fn stopped_visualizer_has_no_bands() {
        let mut visualizer = Visualizer::default();
        assert_eq!(visualizer.status(), VisualizerStatus::Stopped);
        assert!(visualizer.poll().is_none());
        visualizer.start();
        // On machines without parec this is Unavailable; with parec it runs.
        // Either way stop() must return to Stopped without panicking.
        visualizer.stop();
        assert_eq!(visualizer.status(), VisualizerStatus::Stopped);
    }
}
