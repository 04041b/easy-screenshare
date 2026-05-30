# Agent notes

Practical notes for AI agents working on this repo. Read alongside `README.md`.

## Architecture in one paragraph

Rust client (`client/`) + browser/native viewer + Cloudflare Worker backend
(`backend/src/index.ts`) with a D1 DB for SDP signalling and a `RelayRoom`
Durable Object (`backend/src/relay_room.ts`). Primary path is WebRTC P2P
(`webrtc-rs`). If WebRTC fails or times out (15s), both sender and viewer fall
back to a WebSocket relay through the DO. The sender sends a JSON `RelayConfig`
once at WS open, then VP8 + Opus binary frames; the browser viewer decodes via
WebCodecs.

Key client files:
- `client/src/webrtc_client/sender.rs` — share flow; `start_video_pump` /
  `start_audio_pump` run encoders on OS threads and fan out to (a) the WebRTC
  track and (b) a `tokio::sync::broadcast` channel used by the relay.
- `client/src/fallback.rs` — `run_sender` / `run_viewer` for the WS relay.
- `client/src/capture/video.rs`, `audio.rs` — scap (screen) + cpal (audio).

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
