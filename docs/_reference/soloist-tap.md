---
title: Soloist visualiser capture
nav_exclude: true
---

This personal fork provides a Linux-only `soloist_tap` module for visualisers
when an independently installed Soloist receiver plays through PipeWire. The
module does not install Soloist, change playback routing, or connect itself to
the device menu. It needs `pw-dump` and `pw-record` on `PATH`.

The integration owner calls `SoloistTap::start(Arc::clone(&tap), soloist_pid)`,
which returns `std::io::Result<SoloistTap>`. The handle owns capture while
Soloist owns the tap. The caller stops the previous producer before starting
capture. Dropping the handle or calling `stop(self)` signals shutdown without
waiting on the UI thread. The worker kills and reaps its children. A receiver
restart requires a new handle with the new PID. The caller clears the tap when
ownership transfers between audio producers. The PID file and receiver
lifecycle belong to the caller.

Discovery scans `pw-dump` every 500 ms. It matches an audio output node's
`application.process.id`, including a PID inherited through `client.id`, rather
than the Spotify application or node name. Capture uses the node's unique
`object.serial`. It never falls back to a microphone or another device when the
node disappears. A stalled graph query times out after two seconds. Missing
tools and unavailable nodes are retried. Capture warnings use the normal app
log and do not include the graph, command output, or receiver credentials.

`pw-record` supplies native-endian float32 stereo at 44.1 kHz, with requested
10 ms buffers. The tap converts to float64, restores published PipeWire linear
stream and channel gain per channel, and calls `AudioTap::push` with gain 1.0.
The samples reach the same mono buffer and MilkDrop stereo ring as local
playback. The analyser uses `soloist_tap::LAG` (zero samples) instead of
`vis::LAG`. Capture already buffers audio. Adding librespot's 200 ms queue
compensation would delay it again. Actual speaker latency depends on the output
device and was not measured. MilkDrop currently reads `vis::LAG` through
`src/milkdrop/child.rs`. Selecting the capture lag for that consumer also belongs
to the integration.

Capturing the stream's normal output ports includes stream volume. An isolated
PipeWire test with a 0.25 linear stream gain reduced capture RMS from about
0.177 to 0.044. Published `volume * channelVolumes` can be reversed at nonzero
gain. Volume snapshots can lag changes by 500 ms. Soloist's reported 0–100
volume is not assumed to be linear, and any internal attenuation that is absent
from PipeWire's published gain cannot be reversed. Muted or zero-gain output
contains no recoverable music, so this path cannot promise animated visuals at
zero volume. Custom PipeWire software-volume or channel-mixing policies can
also prevent exact restoration. No output volume or mute is changed to work
around that limit.

Capture includes any EQ already applied to Soloist's stream. It does not pass
through Spotifast's equalizer, so changing Spotifast's EQ does not affect
either Soloist's playback or this picture. This integration therefore cannot
provide the local playback path's complete post-EQ, pre-volume guarantee.

The opt-in test exercises capture and gain restoration with an isolated
PipeWire stream that has no speaker output:

```sh
cargo test --locked --lib soloist_tap::tests::pipewire_capture_restores_stream_volume_without_speaker_output -- --ignored --exact --nocapture
```
