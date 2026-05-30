use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::render::FrameSink;
use crate::signaling::ws_url;
use crate::webrtc_client::sender::EncodedFrame;

/// Lower bound the AIMD controller never drops below — at lower bitrates the
/// VP8 encoder produces frames so blocky the share is unusable, and the
/// keyframe spike penalty starts to dominate the budget.
const MIN_BITRATE_KBPS: u32 = 250;

/// A `sink.send()` `.await` that takes longer than this is the TCP socket
/// telling us its send buffer is full. Treated as a congestion signal.
const BACKPRESSURE_LATENCY_MS: u64 = 100;

/// Drop P-frames older than this when we're behind. 150ms ≈ 4.5 frames at
/// 30fps; beyond that the viewer is watching the past, not the present.
/// Keyframes are kept regardless because they unblock the decoder.
const FRAME_AGE_DROP_MS: u64 = 150;

/// AIMD multiplier on backpressure / drop signals. 0.75 = cut by a quarter.
const AIMD_DECREASE: f32 = 0.75;

/// AIMD additive increase per healthy second, in kbps.
const AIMD_INCREASE_KBPS: u32 = 200;

/// How long the link must be quiet (no backpressure, no drops) before the
/// controller starts climbing the bitrate back up.
const HEALTHY_HOLD_MS: u64 = 1_500;

#[derive(Serialize, Deserialize)]
struct RelayConfig {
    v: u32,
    codec: &'static str,
    audio: AudioConfig,
}

#[derive(Serialize, Deserialize)]
struct AudioConfig {
    codec: &'static str,
    rate: u32,
    channels: u16,
}

pub async fn run_sender(
    backend: &str,
    id: &str,
    token: &str,
    mut video: broadcast::Receiver<EncodedFrame>,
    mut audio: broadcast::Receiver<EncodedFrame>,
    force_keyframe: Arc<AtomicBool>,
    target_bitrate_kbps: Arc<AtomicU32>,
    max_bitrate_kbps: u32,
) -> Result<()> {
    let url = ws_url(backend, id, "sender", Some(token), None);
    tracing::info!(%url, max_bitrate_kbps, "opening sender relay ws");
    let (ws, _) = connect_async(&url).await.context("ws connect")?;
    let (mut sink, mut stream) = ws.split();

    let cfg = RelayConfig {
        v: 1,
        codec: "vp8",
        audio: AudioConfig { codec: "opus", rate: 48_000, channels: 2 },
    };
    sink.send(Message::Text(serde_json::to_string(&cfg)?)).await?;

    // Video frames from the broadcast may arrive lagged (consumer started
    // after the WS handshake). VP8 deltas can't be decoded without a
    // preceding keyframe, so suppress video sends until we see a keyframe.
    let mut seen_keyframe = false;

    // Audio is best-effort and may never start (no input device, system-audio
    // capture unavailable, permission denied). When that happens the audio
    // capture thread ends, its broadcast sender drops, and `audio.recv()`
    // returns `Closed`. That must NOT tear down the video relay — otherwise a
    // missing microphone makes the viewer see a permanently black screen. Gate
    // the audio branch so it is disabled (not fatal) once audio ends.
    let mut audio_open = true;

    // -------- Congestion control state --------
    // Wall-clock anchor for the encoder's monotonic frame.timestamp_us so we
    // can compute each frame's actual age before sending. Stamped on the
    // first frame we see and unchanged afterward.
    let mut clock_anchor: Option<(Instant, u64)> = None;

    // Shared counters published by the send path, consumed by the controller.
    // bytes_sent accumulates payload bytes per controller window; drops
    // counts P-frames discarded due to backpressure or staleness; the bool
    // `backpressure_seen` flips whenever the latest sink.send took longer
    // than BACKPRESSURE_LATENCY_MS.
    let bytes_sent = Arc::new(AtomicU64::new(0));
    let drops = Arc::new(AtomicU32::new(0));
    let backpressure_seen = Arc::new(AtomicBool::new(false));

    // AIMD controller. Runs every 500ms, reads the counters, updates the
    // shared target_bitrate_kbps that the encoder reads on each iteration.
    // Spawned so the main relay loop never blocks on its math.
    let controller = {
        let bytes_sent = Arc::clone(&bytes_sent);
        let drops = Arc::clone(&drops);
        let backpressure_seen = Arc::clone(&backpressure_seen);
        let target_bitrate_kbps = Arc::clone(&target_bitrate_kbps);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            let mut last_unhealthy = Instant::now();
            loop {
                interval.tick().await;
                let window_bytes = bytes_sent.swap(0, Ordering::Relaxed);
                let window_drops = drops.swap(0, Ordering::Relaxed);
                let bp = backpressure_seen.swap(false, Ordering::Relaxed);
                let goodput_kbps = (window_bytes as f64 * 8.0 / 0.5 / 1000.0) as u32;
                let current = target_bitrate_kbps.load(Ordering::Relaxed);

                let unhealthy = bp || window_drops > 0;
                if unhealthy {
                    last_unhealthy = Instant::now();
                }

                let new_target = if unhealthy {
                    // Multiplicative decrease, anchored to whichever is lower:
                    // (a) `current * 0.75` — the textbook AIMD response, or
                    // (b) `goodput * 0.9` — what we measured the link can
                    // actually carry. Picking the min prevents flapping when
                    // the link's real capacity is well below our last guess.
                    let from_aimd = (current as f32 * AIMD_DECREASE) as u32;
                    let from_meter = (goodput_kbps as f32 * 0.9) as u32;
                    from_aimd.min(from_meter.max(MIN_BITRATE_KBPS)).max(MIN_BITRATE_KBPS)
                } else if last_unhealthy.elapsed() >= Duration::from_millis(HEALTHY_HOLD_MS) {
                    // Additive increase after a quiet period.
                    current.saturating_add(AIMD_INCREASE_KBPS).min(max_bitrate_kbps)
                } else {
                    current
                };

                if new_target != current {
                    target_bitrate_kbps.store(new_target, Ordering::Relaxed);
                    tracing::info!(
                        prev_kbps = current,
                        new_kbps = new_target,
                        goodput_kbps,
                        drops = window_drops,
                        backpressure = bp,
                        "abr: bitrate decision"
                    );
                }
            }
        })
    };
    // Cancel the controller on relay exit. tokio tasks are detached when
    // their JoinHandle drops, so we need to abort explicitly.
    struct AbortOnDrop(tokio::task::JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _controller_guard = AbortOnDrop(controller);

    loop {
        tokio::select! {
            // Poll the read side too. tokio-tungstenite only auto-responds to
            // server pings while the stream is being polled, so dropping it
            // here would let Cloudflare close the WS for missed pongs after
            // its idle window — which would then cascade to the DO closing
            // all viewers with code 1000 ("idle timeout"). Surface any read
            // errors as a clean break.
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Close(frame))) => {
                        tracing::warn!(?frame, "relay closed by server");
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::warn!("relay read err: {e}");
                        break;
                    }
                    None => break,
                    _ => {}
                }
            }
            v = video.recv() => {
                match v {
                    Ok(f) => {
                        if !seen_keyframe {
                            if f.keyframe {
                                seen_keyframe = true;
                                tracing::info!("relay: first keyframe forwarded");
                            } else {
                                continue;
                            }
                        }

                        // Age check. Compute wall-clock equivalent of the
                        // encoder's monotonic timestamp via the anchor, and
                        // drop stale P-frames so the viewer stays close to
                        // realtime. Keyframes are always forwarded — even a
                        // stale keyframe is better than no keyframe (the
                        // decoder needs it to resync).
                        let (anchor_inst, anchor_ts) =
                            *clock_anchor.get_or_insert((Instant::now(), f.timestamp_us));
                        let frame_inst = anchor_inst
                            + Duration::from_micros(f.timestamp_us.saturating_sub(anchor_ts));
                        let age = Instant::now().saturating_duration_since(frame_inst);
                        if !f.keyframe && age >= Duration::from_millis(FRAME_AGE_DROP_MS) {
                            drops.fetch_add(1, Ordering::Relaxed);
                            // Stale P-frame implies the next one would be
                            // stale too. Request a fresh keyframe and skip
                            // until we see it so the receiver doesn't try to
                            // decode against a missing reference.
                            seen_keyframe = false;
                            force_keyframe.store(true, Ordering::Relaxed);
                            continue;
                        }

                        let bytes = frame_to_bytes(&f);
                        let payload_len = bytes.len();
                        let send_start = Instant::now();
                        if let Err(e) = sink.send(Message::Binary(bytes)).await {
                            tracing::warn!("video relay send err: {e}");
                            break;
                        }
                        let send_elapsed = send_start.elapsed();
                        bytes_sent.fetch_add(payload_len as u64, Ordering::Relaxed);
                        if send_elapsed >= Duration::from_millis(BACKPRESSURE_LATENCY_MS) {
                            backpressure_seen.store(true, Ordering::Relaxed);
                            tracing::debug!(
                                send_ms = send_elapsed.as_millis() as u64,
                                bytes = payload_len,
                                "relay: send latency over threshold (backpressure)"
                            );
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(n, "video relay lagged");
                        drops.fetch_add(n.try_into().unwrap_or(u32::MAX), Ordering::Relaxed);
                        // After a lag we may have skipped past a keyframe;
                        // force the next decision back through the gate, and
                        // ask the encoder to emit a fresh keyframe so we can
                        // resync — without this the loop sits here forever
                        // because VP8 deltas can't decode standalone.
                        seen_keyframe = false;
                        force_keyframe.store(true, Ordering::Relaxed);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            a = audio.recv(), if audio_open => {
                match a {
                    Ok(f) => {
                        let bytes = frame_to_bytes(&f);
                        let payload_len = bytes.len();
                        let send_start = Instant::now();
                        if let Err(e) = sink.send(Message::Binary(bytes)).await {
                            tracing::warn!("audio relay send err: {e}");
                            break;
                        }
                        bytes_sent.fetch_add(payload_len as u64, Ordering::Relaxed);
                        if send_start.elapsed() >= Duration::from_millis(BACKPRESSURE_LATENCY_MS) {
                            backpressure_seen.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(n, "audio relay lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Audio ended; keep relaying video only.
                        tracing::warn!("audio capture unavailable — relaying video without audio");
                        audio_open = false;
                    }
                }
            }
        }
    }
    Ok(())
}

pub async fn run_viewer(backend: &str, id: &str, pin: &str, sink: FrameSink) -> Result<()> {
    let url = ws_url(backend, id, "viewer", None, Some(pin));
    tracing::info!(%url, "opening viewer relay ws");
    let (ws, _) = connect_async(&url).await.context("ws connect")?;
    let (_w, mut read) = ws.split();
    while let Some(msg) = read.next().await {
        match msg? {
            Message::Binary(b) => {
                if let Some(f) = parse_frame(&b) {
                    if f.stream == 0 {
                        sink.push_encoded_vp8(f.data, f.keyframe);
                    }
                    // native audio decode is a follow-up
                }
            }
            Message::Text(_) => { /* RelayConfig, ignore in v1 native viewer */ }
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}

fn frame_to_bytes(f: &EncodedFrame) -> Vec<u8> {
    let mut out = Vec::with_capacity(10 + f.data.len());
    out.push(f.stream);
    out.push(if f.keyframe { 1 } else { 0 });
    out.extend_from_slice(&f.timestamp_us.to_le_bytes());
    out.extend_from_slice(&f.data);
    out
}

fn parse_frame(b: &[u8]) -> Option<ParsedFrame> {
    if b.len() < 10 { return None; }
    let stream = b[0];
    let keyframe = b[1] & 1 == 1;
    let ts = u64::from_le_bytes(b[2..10].try_into().ok()?);
    Some(ParsedFrame {
        stream,
        keyframe,
        timestamp_us: ts,
        data: b[10..].to_vec(),
    })
}

struct ParsedFrame {
    stream: u8,
    keyframe: bool,
    #[allow(dead_code)]
    timestamp_us: u64,
    data: Vec<u8>,
}
