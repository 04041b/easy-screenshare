# screenshare

Install-free screen sharing. Native sender (Rust + egui, single binary), zero-install browser viewer, signaling and relay via a Cloudflare Worker with D1 + a single Durable Object.

The sender tries WebRTC P2P first; if it can't establish in 4 seconds (which is common against browsers because `webrtc-rs` 0.11's DTLS stack doesn't understand DTLS 1.3) it escalates to a WebSocket relay through the Durable Object. The relay path runs an AIMD bandwidth controller so it adapts to the actual uplink instead of pushing blind.

## Architecture

```
sender ──HTTPS──▶ Worker ◀──HTTPS── viewer (browser/native)
   │                │                    ▲
   │                ├── D1 (offer/answer + fallback flag)
   │                │
   │                └── RelayRoom DO (WS relay, when WebRTC fails)
   │                                     │
   └────────── WebRTC P2P (STUN) ────────┘
```

- **Sender** (`client/`): Rust binary. Captures screen via `scap` and audio via `cpal` (macOS/Linux) or `wasapi` loopback (Windows). VP8 video via libvpx in realtime mode (`cpu_used=8`); Opus audio. Encoded frames fan out to both a WebRTC track and a `tokio::sync::broadcast` channel that the relay path reads from.
- **Backend** (`backend/`): Cloudflare Worker (TypeScript). D1 holds the SDP offer/answer for non-trickle ICE signaling and a "fallback engaged" flag. A `RelayRoom` Durable Object accepts the sender + viewers as WebSockets and forwards encoded frames as binary messages.
- **Viewer**: a browser tab served at `/viewer/:id` (WebCodecs decoder) or the same Rust binary in `view` mode (still useful for debugging; native VP8 decode is wire-up only).

## Quality presets

The sender's GUI picker (or `Quality::default()` for headless) selects an encoding ceiling. The relay path may auto-cut bitrate below this ceiling based on observed congestion.

| Preset | Resolution | FPS | VP8 bitrate ceiling |
|---|---|---|---|
| Low    | 720p  | 15 | 1.2 Mbps |
| Medium | 1080p | 24 | 2.5 Mbps |
| High   | 1080p | 30 | 4 Mbps (default) |

Pick **Low** if you're on a slow uplink or sharing while gaming. Pick **High** for screen detail on a healthy link.

## Backend

```bash
cd backend
npm install
wrangler d1 create screenshare-signals   # paste returned id into wrangler.toml
npm run db:migrate:local
npm run dev                              # local dev server
npx wrangler deploy                      # production
npx wrangler tail --format pretty        # live logs
```

Hit `POST http://localhost:8787/api/sessions` to verify.

## Client

```bash
cd client
SCREENSHARE_BACKEND=http://localhost:8787 cargo run --release             # GUI
SCREENSHARE_BACKEND=http://localhost:8787 cargo run --release -- share    # headless sender
cargo run --release -- view ABCD1234 123456                                # native viewer
cargo run --release -- probe                                               # capture + permission probe
```

The compiled-in backend default is production (`screenshare-backend.04041b.workers.dev`); override with `SCREENSHARE_BACKEND` or `--backend`.

### macOS permission gotcha

Screen Recording permission is keyed to the executable's code hash. Every `cargo build` invalidates the grant for `target/release/screenshare` and `scap::has_permission()` keeps reporting a stale `true`. If capture suddenly fails after a rebuild, re-add the binary in System Settings ▸ Privacy & Security ▸ Screen Recording. `screenshare probe` is the fastest way to confirm.

## What works on what platform

| Feature | macOS | Windows | Linux |
|---|---|---|---|
| Screen capture | ✓ (ScreenCaptureKit via scap) | ✓ (windows-capture via scap, native-res — we downscale in client) | ✓ (X11/Wayland via scap) |
| System audio capture | ✗ (cpal mic only — SCKit audio is a follow-up) | ✓ (WASAPI loopback via the `wasapi` crate) | ✗ (needs PulseAudio loopback) |
| WebRTC P2P to browser | ✓ (DTLS 1.2 ok) | ✗ (DTLS 1.3 mismatch — falls back to relay) | ✓ |
| WS relay path | ✓ | ✓ | ✓ |

## Status

| Component | State |
|---|---|
| Worker signaling (D1) | ✓ |
| RelayRoom DO with keyframe-on-join, alarm gating, config replay | ✓ |
| Browser viewer (WebCodecs, debug overlay via `?debug=1` or pressing D) | ✓ |
| Sender → VP8 (realtime preset) → WebRTC + relay broadcast | ✓ |
| Sender → Opus → WebRTC + relay broadcast | ✓ |
| Relay-path AIMD congestion control (bitrate + frame pacing) | ✓ |
| Mid-stream WebRTC `Failed` → relay escalation | ✓ |
| Quality presets in GUI | ✓ |
| Native viewer VP8 decode | stubbed (browser viewer is the recommended path) |
| macOS system audio (vs. mic) | open (needs SCKit audio wiring) |

See `AGENTS.md` for architecture notes, debugging recipes, gotchas, and bug history. See `CLAUDE.md` for Claude Code session conventions.
