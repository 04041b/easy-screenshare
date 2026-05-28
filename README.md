# screenshare

Install-free screen sharing. Native sender (Rust + egui, single binary), zero-install browser viewer, signaling via a Cloudflare Worker with D1 + a single Durable Object used only when WebRTC needs a relay.

## Architecture

- **Sender** (`client/`): Rust binary. Captures screen via `scap` + audio via `cpal`, encodes to VP8/Opus, publishes through `webrtc-rs` to one or more viewers.
- **Backend** (`backend/`): Cloudflare Worker (TypeScript). D1 stores SDP offer/answer for non-trickle ICE signaling. A `RelayRoom` Durable Object is only instantiated when WebRTC fails between peers; it relays encoded frames as WebSocket binary messages.
- **Viewer**: either a browser (HTML page served by the Worker at `/viewer/:id`) or the same Rust binary in `view` mode.

```
sender ──HTTPS──▶ Worker ◀──HTTPS── viewer (browser/native)
   │                │                    ▲
   │                └── D1 (offer/answer)
   │                                     │
   └────────── WebRTC P2P (STUN) ────────┘
                       │ (if P2P fails)
                       ▼
                Durable Object WS relay
```

## Backend

```bash
cd backend
npm install
wrangler d1 create screenshare-signals  # paste returned id into wrangler.toml
npm run db:migrate:local
npm run dev
```

Hit `POST http://localhost:8787/api/sessions` to verify.

## Client

```bash
cd client
SCREENSHARE_BACKEND=http://localhost:8787 cargo run --release
# or headless:
SCREENSHARE_BACKEND=http://localhost:8787 cargo run --release -- share
# viewer mode:
cargo run --release -- view ABCD1234
```

On macOS the first run will prompt for Screen Recording permission.

## Status

| Component | State |
|---|---|
| Worker signaling (D1) | complete |
| RelayRoom DO (WS fallback) | complete |
| Browser viewer (WebRTC + WebCodecs fallback) | complete |
| Rust sender capture → VP8 → WebRTC | complete |
| Rust sender Opus audio → WebRTC | complete |
| Rust sender → fallback WS push | complete |
| Rust native viewer window | windowing complete; **VP8 decode is stubbed** — wire `vpx-decode` or `ffmpeg-next` to enable. The browser viewer is the recommended viewer experience for v1. |
| macOS system audio (vs. mic) | scap supports it; v1 currently uses cpal mic input on macOS — follow-up to switch to SCKit audio. |

See `client/src/render.rs` `mod vpx_decode_shim` for the native-viewer decode extension point.
