//! Captures an independently installed Soloist receiver's PipeWire output.
//!
//! Keep one handle while Soloist owns the visualiser's [`AudioTap`]. The caller
//! supplies the receiver's PID and must replace the handle after a process
//! restart. Nodes belonging to that PID are rediscovered as tracks change.
//! This observes Soloist's output; it does not route playback through Spotifast's
//! equalizer. Any EQ already applied to that output is included in the capture.

use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use librespot_playback::{NUM_CHANNELS, SAMPLE_RATE};
use serde_json::Value;

use crate::vis::AudioTap;

/// Additional history to read with `AudioTap::window` for this capture path.
/// `pw-record` requests 10 ms buffers and already delivers samples behind the
/// output stream. There is no librespot sink queue to compensate, so use the
/// newest samples, not `vis::LAG`. Device-specific output latency is unmeasured.
pub const LAG: usize = 0;

const POLL: Duration = Duration::from_millis(5);
const RESCAN: Duration = Duration::from_millis(500);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const STALE: Duration = Duration::from_millis(250);
const CHANNELS: usize = NUM_CHANNELS as usize;
const FRAME_BYTES: usize = CHANNELS * size_of::<f32>();
const MAX_DUMP: usize = 8 * 1024 * 1024;

/// A capture worker. Starting and dropping it never waits for PipeWire or disk.
/// Drop signals shutdown; the worker kills and reaps its children off the UI
/// thread. The caller must stop the previous audio producer before sharing its
/// tap with this one, and select [`LAG`] for the analyser while capture is active.
pub struct SoloistTap {
    stop: mpsc::Sender<()>,
}

impl SoloistTap {
    /// Starts discovery on a dedicated thread. Missing PipeWire tools or a
    /// missing stream are retried; only failure to spawn the thread is returned.
    pub fn start(tap: Arc<AudioTap>, soloist_pid: u32) -> io::Result<Self> {
        let (stop, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("soloist-tap".into())
            .spawn(move || capture(tap, soloist_pid, receiver))?;
        Ok(Self { stop })
    }

    /// Requests asynchronous shutdown, the same as dropping the handle.
    pub fn stop(self) {}
}

impl Drop for SoloistTap {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Node {
    // Serial numbers are not reused when a node disappears, unlike object IDs.
    serial: u64,
    gains: [f64; CHANNELS],
}

fn number(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn find_node(bytes: &[u8], pid: u32) -> serde_json::Result<Option<Node>> {
    let objects: Vec<Value> = serde_json::from_slice(bytes)?;
    Ok(objects.iter().find_map(|object| {
        if object["type"] != "PipeWire:Interface:Node" {
            return None;
        }
        let props = &object["info"]["props"];
        if props["media.class"] != "Stream/Output/Audio" {
            return None;
        }
        let process = number(&props["application.process.id"]).or_else(|| {
            let client_id = number(&props["client.id"])?;
            let client = objects.iter().find(|candidate| {
                candidate["type"] == "PipeWire:Interface:Client"
                    && number(&candidate["id"]) == Some(client_id)
            })?;
            number(&client["info"]["props"]["application.process.id"])
        })?;
        if process != u64::from(pid) {
            return None;
        }
        Some(Node {
            serial: number(&props["object.serial"])?,
            gains: channel_gains(&object["info"]["params"]["Props"][0]),
        })
    }))
}

fn channel_gains(props: &Value) -> [f64; CHANNELS] {
    let volume = props["volume"].as_f64().unwrap_or(1.0);
    let muted = props["mute"].as_bool().unwrap_or(false);
    std::array::from_fn(|channel| {
        if muted {
            0.0
        } else {
            volume * props["channelVolumes"][channel].as_f64().unwrap_or(1.0)
        }
    })
}

fn restore(sample: f32, gain: f64) -> f64 {
    // pw-record targets the stream's normal output ports, after stream/channel
    // volume, not a pre-volume monitor. Verified with an isolated 0.25-gain
    // PipeWire stream. Undo the published linear gain, never the Spotify volume
    // percentage (whose curve is unspecified). At mute/zero gain the information
    // is gone: silence cannot be reconstructed and zero volume cannot dance.
    // Soloist may also attenuate internally without publishing a PipeWire gain;
    // that attenuation cannot be removed here. Snapshot polling can lag volume
    // changes by RESCAN, so brief changes in amplitude remain possible.
    if sample.is_finite() && gain.is_finite() && gain > 0.0 {
        let restored = f64::from(sample) / gain;
        if restored.is_finite() {
            return restored;
        }
    }
    0.0
}

#[derive(Default)]
struct Samples {
    pending: Vec<u8>,
    interleaved: Vec<f64>,
}

impl Samples {
    fn push(&mut self, bytes: &[u8], gains: [f64; CHANNELS], tap: &AudioTap) {
        self.pending.extend_from_slice(bytes);
        let complete = self.pending.len() / FRAME_BYTES * FRAME_BYTES;
        self.interleaved.clear();
        for (index, bytes) in self.pending[..complete]
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
        {
            // pw-record's f32 raw format uses native byte order. Retain partial
            // stereo frames across reads, including reads split inside a float.
            let sample = f32::from_ne_bytes(*bytes);
            self.interleaved
                .push(restore(sample, gains[index % CHANNELS]));
        }
        tap.push(&self.interleaved, 1.0);
        self.pending.drain(..complete);
    }
}

/// A nonblocking stdout socket avoids both an indefinitely blocked read and a
/// second reader thread. The child keeps a blocking writer on the other end.
struct Output {
    child: Child,
    stdout: UnixStream,
}

impl Output {
    fn start(command: &mut Command) -> io::Result<Self> {
        let (stdout, writer) = UnixStream::pair()?;
        stdout.set_nonblocking(true)?;
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("{}: {error}", command.get_program().to_string_lossy()),
                )
            })?;
        Ok(Self { child, stdout })
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Dump {
    output: Output,
    bytes: Vec<u8>,
    started: Instant,
}

impl Dump {
    fn start() -> io::Result<Self> {
        Ok(Self {
            output: Output::start(&mut Command::new("pw-dump"))?,
            bytes: Vec::new(),
            started: Instant::now(),
        })
    }

    fn poll(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut buffer = [0; 16 * 1024];
        let mut eof = false;
        for _ in 0..16 {
            match self.output.stdout.read(&mut buffer) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(count) => self.bytes.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
            if self.bytes.len() > MAX_DUMP {
                return Err(io::Error::other("PipeWire graph exceeds capture limit"));
            }
        }
        if let Some(status) = self.output.child.try_wait()? {
            if !status.success() {
                return Err(io::Error::other("pw-dump failed"));
            }
            // The child can exit between the last read and try_wait. Wait for
            // EOF too, so the final stdout bytes are not lost in that race.
            if eof {
                return Ok(Some(std::mem::take(&mut self.bytes)));
            }
        }
        if self.started.elapsed() >= COMMAND_TIMEOUT {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "pw-dump timed out"));
        }
        Ok(None)
    }
}

struct Recording {
    output: Output,
    node: Node,
    samples: Samples,
    last_audio: Instant,
    cleared: bool,
}

impl Recording {
    fn start(node: Node) -> io::Result<Self> {
        let output = Output::start(Command::new("pw-record").args([
            "--target",
            &node.serial.to_string(),
            "--raw",
            "--format",
            "f32",
            "--rate",
            &SAMPLE_RATE.to_string(),
            "--channels",
            &CHANNELS.to_string(),
            "--latency",
            "10ms",
            "--properties",
            // Never fall back to the microphone/default sink if Soloist's node
            // disappears between discovery and linking. Never move on restart.
            // Saved recorder properties must not attenuate or reroute capture.
            "{ node.dont-fallback=true node.dont-reconnect=true node.dont-move=true \
              stream.monitor=true state.restore-props=false state.restore-target=false }",
            "-",
        ]))?;
        Ok(Self {
            output,
            node,
            samples: Samples::default(),
            last_audio: Instant::now(),
            cleared: false,
        })
    }

    fn poll(&mut self, tap: &AudioTap) -> io::Result<bool> {
        let mut buffer = [0; 16 * 1024];
        let mut received = false;
        // Bound each drain so discovery and shutdown cannot starve.
        for _ in 0..16 {
            match self.output.stdout.read(&mut buffer) {
                Ok(0) => return Err(io::Error::other("pw-record closed its output")),
                Ok(count) => {
                    self.samples.push(&buffer[..count], self.node.gains, tap);
                    self.last_audio = Instant::now();
                    self.cleared = false;
                    received = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        if self.last_audio.elapsed() >= STALE && !self.cleared {
            tap.clear();
            self.cleared = true;
        }
        Ok(received)
    }
}

fn capture(tap: Arc<AudioTap>, pid: u32, stop: mpsc::Receiver<()>) {
    let mut recording: Option<Recording> = None;
    let mut dump: Option<Dump> = None;
    let mut next_scan = Instant::now();
    let mut warned = false;
    while let Err(mpsc::RecvTimeoutError::Timeout) = stop.recv_timeout(POLL) {
        if dump.is_none() && Instant::now() >= next_scan {
            match Dump::start() {
                Ok(started) => dump = Some(started),
                Err(error) => warn_once(&mut warned, &error),
            }
            next_scan = Instant::now() + RESCAN;
        }
        if let Some(active) = &mut dump {
            let result = active.poll().and_then(|bytes| {
                bytes
                    .map(|bytes| find_node(&bytes, pid).map_err(io::Error::other))
                    .transpose()
            });
            match result {
                Ok(None) => (),
                Ok(Some(node)) => {
                    dump = None;
                    if recording.as_ref().map(|r| r.node.serial) != node.as_ref().map(|n| n.serial)
                    {
                        recording = None;
                        tap.clear();
                    }
                    if let Some(node) = node {
                        if let Some(active) = &mut recording {
                            active.node = node;
                        } else {
                            match Recording::start(node) {
                                Ok(started) => {
                                    recording = Some(started);
                                }
                                Err(error) => warn_once(&mut warned, &error),
                            }
                        }
                    }
                }
                Err(error) => {
                    dump = None;
                    recording = None;
                    tap.clear();
                    warn_once(&mut warned, &error);
                }
            }
        }
        if let Some(active) = &mut recording {
            match active.poll(&tap) {
                Ok(true) => warned = false,
                Ok(false) => (),
                Err(error) => {
                    recording = None;
                    tap.clear();
                    warn_once(&mut warned, &error);
                }
            }
        }
    }
    // Child kill/wait happens here, on this worker, never in SoloistTap::drop.
    // Do not clear on exit: a replacement producer may already own the tap.
}

fn warn_once(warned: &mut bool, error: &io::Error) {
    if !*warned {
        log::warn!("Soloist visualiser capture unavailable: {error}");
        *warned = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn discovery_uses_pid_including_client_inheritance_not_spotify_names() {
        let graph = json!([
            {"id": 94, "type": "PipeWire:Interface:Client", "info": {"props": {
                "application.name": "Spotify", "application.process.id": 80003,
                "media.class": "Stream/Output/Audio", "node.name": "spotify", "object.serial": 293
            }}},
            {"id": 95, "type": "PipeWire:Interface:Node", "info": {"props": {
                "application.name": "Spotify", "application.process.id": "12345",
                "media.class": "Stream/Output/Audio", "node.name": "spotify", "object.serial": 290
            }}},
            {"id": 98, "type": "PipeWire:Interface:Node", "info": {
                "props": {"client.id": "94", "application.name": "Spotify",
                    "media.class": "Stream/Output/Audio", "node.name": "spotify", "object.serial": "294"},
                "params": {"Props": [{"volume": 0.5, "channelVolumes": [0.25, 0.75]}]}
            }}
        ]);
        let bytes = serde_json::to_vec(&graph).unwrap();
        assert_eq!(
            find_node(&bytes, 80003).unwrap(),
            Some(Node {
                serial: 294,
                gains: [0.125, 0.375],
            })
        );
        assert_eq!(find_node(&bytes, 12345).unwrap().unwrap().serial, 290);
        assert_eq!(find_node(&bytes, 99999).unwrap(), None);
    }

    #[test]
    fn discovery_rejects_wrong_classes_missing_serials_and_invalid_json() {
        for props in [
            json!({"media.class": "Audio/Source", "application.process.id": 7, "object.serial": 42}),
            json!({"media.class": "Stream/Output/Audio", "application.process.id": 7}),
            json!({"media.class": "Stream/Output/Audio", "client.id": 42, "object.serial": 43}),
        ] {
            let bytes = serde_json::to_vec(&json!([
                {"id": 1, "type": "PipeWire:Interface:Node", "info": {"props": props}}
            ]))
            .unwrap();
            assert_eq!(find_node(&bytes, 7).unwrap(), None);
        }
        assert!(find_node(b"not JSON", 7).is_err());
        assert!(find_node(b"{}", 7).is_err());
    }

    #[test]
    fn float_bytes_keep_stereo_frames_across_every_read_boundary() {
        let source = [0.25_f32, 0.5, -0.5, -1.0, 0.125, 0.25];
        let bytes: Vec<_> = source.into_iter().flat_map(f32::to_ne_bytes).collect();
        for split in 0..=bytes.len() {
            let tap = AudioTap::new();
            let mut samples = Samples::default();
            samples.push(&bytes[..split], [0.5, 1.0], &tap);
            samples.push(&bytes[split..], [0.5, 1.0], &tap);
            assert_eq!(tap.window(3, LAG), [0.5, -1.0, 0.25], "split {split}");
            assert!(samples.pending.is_empty());
            assert_eq!(samples.interleaved.len() % CHANNELS, 0);
        }
    }

    #[test]
    fn volume_restoration_preserves_source_amplitude_and_handles_silence() {
        for gain in [1.0, 0.5, 0.25, 0.125, 2.0] {
            assert_eq!(restore((0.25 * gain) as f32, gain), 0.25);
        }
        for gain in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(restore(0.5, gain), 0.0);
        }
        for sample in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(restore(sample, 1.0), 0.0);
        }
        assert_eq!(channel_gains(&json!({})), [1.0, 1.0]);
        assert_eq!(channel_gains(&json!({"mute": true})), [0.0, 0.0]);
        assert_eq!(channel_gains(&json!({"volume": 0.0})), [0.0, 0.0]);
    }

    #[test]
    fn dropping_output_kills_and_reaps_a_child_with_open_stdout() {
        let mut output =
            Output::start(Command::new("sh").args(["-c", "printf ready; exec sleep 30"])).unwrap();
        let pid = output.child.id();
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let mut bytes = Vec::new();
        while bytes.len() < 5 && Instant::now() < deadline {
            let _ = output.stdout.read_to_end(&mut bytes);
            std::thread::sleep(POLL);
        }
        assert_eq!(bytes, b"ready");
        assert!(output.child.try_wait().unwrap().is_none());
        drop(output);
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    #[ignore = "requires a running PipeWire session, pw-dump, pw-play and pw-record"]
    fn pipewire_capture_restores_stream_volume_without_speaker_output() {
        use std::io::Write;

        struct Playback(Child);
        impl Drop for Playback {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let mut source = Playback(
            Command::new("pw-play")
                .args([
                    "--target",
                    "0",
                    "--raw",
                    "--format",
                    "f32",
                    "--rate",
                    "44100",
                    "--channels",
                    "2",
                    "--volume",
                    "0.25",
                    "--properties",
                    "{ node.name=spotifast-tap-test node.always-process=true state.restore-props=false }",
                    "-",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut input = source.0.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            let tone: Vec<u8> = (0..SAMPLE_RATE)
                .flat_map(|frame| {
                    let phase = frame as f32 * std::f32::consts::TAU * 440.0 / SAMPLE_RATE as f32;
                    [phase.sin() * 0.25; CHANNELS]
                })
                .flat_map(f32::to_ne_bytes)
                .collect();
            while input.write_all(&tone).is_ok() {}
        });
        let tap = AudioTap::new();
        let capture = SoloistTap::start(Arc::clone(&tap), source.0.id()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut rms = 0.0;
        while Instant::now() < deadline {
            let window = tap.window(4096, LAG);
            rms = (window.iter().map(|v| v * v).sum::<f32>() / window.len() as f32).sqrt();
            if (rms - 0.25 / 2.0_f32.sqrt()).abs() < 0.005 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        capture.stop();
        drop(source);
        writer.join().unwrap();
        println!("restored capture RMS: {rms:.6}");
        assert!((rms - 0.25 / 2.0_f32.sqrt()).abs() < 0.005);
    }
}
