# Agent notes

Practical notes for AI agents working on this repo. Read alongside `README.md`.

## Architecture in one paragraph

Rust client (`client/`) + browser/native viewer + Cloudflare Worker backend
(`backend/src/index.ts`) with a D1 DB for SDP signalling and a `RelayRoom`
Durable Object (`backend/src/relay_room.ts`). Primary path is WebRTC P2P
(`webrtc-rs`). If WebRTC fails or times out (**4s**, dropped from the original
15s), both sender and viewer fall back to a WebSocket relay through the DO. The
sender sends a JSON `RelayConfig` once at WS open, then VP8 + Opus binary
frames; the browser viewer decodes via WebCodecs. The relay path on the sender
runs an AIMD bandwidth estimator that adapts the encoder's bitrate to observed
TCP backpressure and frame age — see `client/src/fallback.rs` for the
controller.

Key client files:
- `client/src/webrtc_client/sender.rs` — share flow; `start_video_pump` /
  `start_audio_pump` run encoders on OS threads and fan out to (a) the WebRTC
  track and (b) a `tokio::sync::broadcast` channel used by the relay. The video
  encoder reads `target_bitrate_kbps: Arc<AtomicU32>` each iteration and
  re-inits libvpx when it changes (the same trick we use for keyframes since
  vpx-encode 0.5.0 exposes neither knob). After `Encoder::new` we reach
  through the pinned struct layout and call `vpx_codec_control_(VP8E_SET_CPUUSED,
  8)` to put libvpx in realtime mode.
- `client/src/fallback.rs` — `run_sender` / `run_viewer` for the WS relay,
  including: TCP `SO_SNDBUF` cap to 64KB, send-latency probe,
  frame-age dropping (100ms threshold), goodput meter, and AIMD bitrate
  controller. Also parses `{"v":1,"cmd":"keyframe"}` text messages from the DO
  and flips `force_keyframe` accordingly.
- `client/src/capture/video.rs` — scap reader + emitter, plus in-process BGRA
  downscale to the requested `Resolution` because scap's Windows backend
  silently ignores its own `output_resolution` option.
- `client/src/capture/audio.rs` — cpal mic on macOS/Linux, WASAPI loopback on
  Windows (via the `wasapi` crate). All Windows COM init happens on the worker
  thread because the WASAPI handle types are `!Send`.
- `client/src/capture/mod.rs` — the `Quality` enum (Low/Medium/High), and the
  `lower_thread_priority_for_background_work()` helper used by the capture
  reader/emitter and the VP8 encoder threads on Windows to keep games at
  scheduler priority.

Key backend files:
- `backend/src/relay_room.ts` — the DO. Buffers `RelayConfig`, replays it to
  late viewers, sends `{"v":1,"cmd":"keyframe"}` to attached senders when a new
  viewer joins (so the viewer's VideoDecoder gets a usable keyframe instead of
  erroring on the first delta).
- `backend/src/viewer_html.ts` — the inline browser viewer. WebCodecs decoder
  with a `seenKeyframe` gate that silently drops pre-keyframe deltas.

## How to reproduce relay/pipeline bugs locally (no second machine, no browser)

The sender only starts capture *after* a viewer posts an SDP answer, and only
enters the relay after WebRTC fails — which makes the relay path awkward to
trigger. Two tools make it deterministic and agent-runnable:

1. **Drive the sender into fallback without a browser.** Write a throwaway
   `client/examples/` harness that: fetches the offer, builds a real answer with
   `webrtc-rs`, posts it, then immediately `pc.close()`s so ICE can never
   connect. The sender times out after 15s and switches to the relay. The same
   harness can then connect to the relay as a raw WS viewer (`role=viewer`) and
   count frames / flag keyframes. (Deleted after use — recreate as needed.)

2. **Synthetic capture.** Capture needs screen-recording permission, which is
   painful to keep across rebuilds (see below). When you need frames flowing but
   don't care about real pixels, temporarily add an env-gated synthetic frame
   generator in `VideoCapture::start` (returns animated BGRA at target fps).
   Lets you exercise encode → broadcast → relay with zero permission. (Also
   throwaway.)

3. **Watch the DO side:** `cd backend && npx wrangler tail --format pretty`.
   The `RelayRoom` has `console.log` diagnostics on accept/close/alarm. A WS
   `close ... code=1006 ... clean=false` means the client dropped the
   connection abnormally (often a *consequence* of `run_sender` exiting, not the
   cause — check the client first).

4. **`screenshare probe`** (real subcommand, kept) — runs the actual
   `VideoCapture` path and grabs one frame, so you can verify capture +
   permission without a viewer.

Instrument before fixing. Two hypotheses in this area were wrong on first guess
(`write_sample` stalling; keyframe not produced) and only tagged debug logs at
the encode→broadcast→relay boundaries showed the truth. Tag temporary logs with
a unique prefix and grep them out before committing.

## Bugs resolved (most recent first)

### Browser viewer black screen on mid-stream join — keyframe never arrived (`9d0cfdc`)
Symptom: `wsf` counter climbs into the thousands while the canvas stays black;
DevTools console shows `decode err: a key frame is required after configure()
or flush()`. Root cause: libvpx with our config emits keyframes only on init /
force, so a viewer joining mid-stream got only P-frames. Fix has three parts:
DO sends `{"v":1,"cmd":"keyframe"}` to the sender on every viewer-attach;
sender's relay loop parses it and flips `force_keyframe`; browser viewer gates
`vdec.decode` behind a `seenKeyframe` flag (drops pre-keyframe deltas instead
of feeding them to WebCodecs).

### 20-second viewer lag despite AIMD controller (`ef284b5`)
Symptom: AIMD bitrate controller appeared healthy in logs but the viewer was
20+ seconds behind realtime. Root cause: the kernel's TCP send buffer absorbed
many megabytes of frames silently — `sink.send().await` returned instantly,
the latency probe never fired, and our "fresh keyframe" responses to lag were
themselves stale by the time they reached the wire. Fix: cap `SO_SNDBUF` to
64KB on the relay WS via `socket2::SockRef` immediately after connect (forces
backpressure to surface application-side within ~500ms), and tighten the
age-drop to discard *all* stale frames including keyframes (a "fresh" keyframe
that took 2s to push through isn't fresh).

### Windows game framerate halved while sharing (`79e0e68`)
Symptom: user's game drops from 60+ to 30 fps once a share starts. Two root
causes: (1) capture/emitter/encoder threads all ran at `NORMAL` OS priority,
sharing scheduler quanta with the game's render thread; (2) vpx-encode 0.5.0
leaves libvpx VP8 at the default `cpu_used=0` (best quality, ~30-50ms per
1080p frame). Fix: drop those three threads to `THREAD_PRIORITY_BELOW_NORMAL`
on Windows via `windows-sys`, and reach through vpx-encode's pinned struct
layout to call `vpx_codec_control_(VP8E_SET_CPUUSED, 8)` for realtime mode
(~5-10ms per 1080p frame). The struct-poking is safe because `Cargo.lock`
pins the version.

### Sender crash on Windows after a few minutes of share (`3aa1faf`)
Symptom: process disappears with no panic message, stdout truncated mid-line
(classic OOM-kill signature). Root cause: `enc_tx` mpsc between encoder
thread and async forwarder was `unbounded_channel`. When the forwarder
stalled (e.g. on `track.write_sample` against a Failed WebRTC track), the
encoder kept producing into the unbounded channel until memory exhaustion.
The lag/keyframe-thrash spiral made this exponentially worse — every drop
triggered an encoder re-init with a 100KB+ keyframe. Fix: bound the mpsc
(`channel(16)`); on `TrySendError::Full`, drop the frame and set
`force_keyframe` so the next survivor is independently decodable.

### Windows lag/keyframe thrash spiral (`3aa1faf`)
Symptom: persistent `video relay lagged n=...` logs every ~1s, viewer
frozen. Root cause: scap's `windows-capture` backend silently ignores
`output_resolution`, so a 2560×1600 display was being captured natively
and encoded at our 4 Mbps preset bitrate — starvation-level (~1 bit per
pixel). Every keyframe was 50-100KB, the WS uplink couldn't keep up,
broadcast lagged, force_keyframe fired, encoder re-init produced an even
bigger keyframe, cycle repeats. Fix: downscale BGRA in the scap reader
thread to the requested `Resolution` before handing it down. No-op on
platforms where scap honored the request.

### Windows audio silent — cpal default-output-as-input doesn't loopback (`3aa1faf`, `10bed15`)
Symptom: sender logs `audio capture ended — continuing without audio`
within seconds of start; relay viewer never gets a single audio frame.
Root cause: cpal's `default_output_device() + build_input_stream` on
Windows doesn't pass `AUDCLNT_STREAMFLAGS_LOOPBACK`, so the WASAPI stream
opens but no callbacks fire. Fix: replace the Windows path with the
`wasapi` crate, which sets the flag when initialising a Render-direction
device in Capture direction. All WASAPI/COM types are `!Send`, so all
init happens on the worker thread with a `std::sync::mpsc::sync_channel`
oneshot signalling success/failure back.

### Mid-stream WebRTC failure left pumps writing to a dead track (`3aa1faf`)
Symptom: viewer freezes after some period of healthy WebRTC; no recovery.
Root cause: after `connected.notified()` fires the connection-state watcher
exited; later `Failed`/`Disconnected` states were ignored, so the pumps
just kept writing samples into a dead transport. Fix: post-connect
`tokio::select!` races a fresh `failed.notified()` against pump termination,
and on failure escalates to the relay via the same path as the initial
failure.

### Initial relay engagement took 15s (`057015a`, `3aa1faf`)
Symptom: viewer stares at black for ~16s before switching to relay.
Root cause: 15s WebRTC connect timeout in sender, viewer, and the browser
viewer. A real DTLS handshake is sub-2s; on a stalled DTLS (Windows) the
timer is the only signal. Fix: drop all three to 4s.

### Relay delivered no video — audio failure killed the whole relay (`cd7ba81`)
Symptom: viewer shows black screen, debug overlay `v/a 0/0`, "relay closed".
Root cause: in `fallback::run_sender`, the `audio.recv()` arm treated
`broadcast::error::RecvError::Closed` as fatal (`break`). When audio capture
never started (no input device, system-audio not available by default on macOS,
permission denied), the audio encoder thread exited → audio broadcast closed →
`run_sender` broke its entire select loop → sender's relay WS closed (1006) →
viewer got "sender disconnected". **Video was fine the whole time.** Fix: audio
is best-effort — on `Closed`, disable that select branch (`audio_open = false`)
and keep relaying video. Verified on Windows by the user.

### macOS: `scap` panic-aborts the sender process (in `cd7ba81`)
`scap`'s `create_capturer` does `.find(main_display).unwrap()` on
`SCShareableContent::current().displays`. When Screen Recording permission is
not effectively granted, that list is empty and it panics — and because
`[profile.release] panic = "abort"`, the *whole process* dies, the relay WS
closes, and it looks like a relay bug. Fix: `VideoCapture::start` pre-flights
`scap::get_all_targets()` and returns an actionable error when no displays are
capturable, instead of letting scap abort.

## Gotchas

- **macOS TCC is per-binary.** Screen Recording permission is keyed to the
  executable's code hash. **Every `cargo build` invalidates the grant** for
  `target/release/screenshare`, so capture silently breaks after a rebuild and
  `scap::has_permission()` can still report a stale `true`. If a user "granted
  permission" but capture fails, suspect a rebuild since. Use `screenshare
  probe` to confirm. Long-term fix would be a stable code-signing identity.
- **`panic = "abort"`** (release profile) means any panic on any thread kills
  the process. Prefer pre-flight checks that return `Result` over trusting
  third-party crates not to panic.
- **bin-only crate.** `client/` has no `lib.rs`, so `run_sender`,
  `start_video_pump`, `VideoCapture`, etc. are not importable from `tests/` or
  `examples/`. This has repeatedly blocked clean regression tests. Consider
  splitting into `lib.rs` + thin `main.rs` to get testable seams.
- **CI** (`.github/workflows/build.yml`) builds `macos-arm64` and `windows-x64`
  release binaries on every push to `main` and uploads them as artifacts
  (`screenshare-windows-x64`, etc., 14-day retention). Use this to hand the user
  a Windows build to verify.

## Known open issues (not yet fixed)

- **WebRTC P2P fails against modern browsers.** `webrtc-rs` 0.11's DTLS stack
  doesn't understand DTLS 1.3 extensions browsers offer (`Unsupported Extension
  Type 51/43/45`, then `invalid named curve`), so P2P always falls back to the
  relay for those clients. Fix needs a `webrtc-rs` upgrade that speaks DTLS 1.3,
  or constraining the offer to DTLS 1.2 / a known curve.
- **Audio over the relay** is effectively unused on machines without a capture
  source; the relay now tolerates this. Real system-audio capture on macOS
  needs a loopback device or ScreenCaptureKit audio wiring (follow-up).
- **Backend relay diagnostics.** `relay_room.ts` still has `console.log` lines
  from debugging; decide whether to keep or strip when you next touch it.
