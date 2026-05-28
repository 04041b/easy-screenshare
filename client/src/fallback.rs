use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::render::FrameSink;
use crate::signaling::ws_url;
use crate::webrtc_client::sender::EncodedFrame;

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
) -> Result<()> {
    let url = ws_url(backend, id, "sender", Some(token));
    tracing::info!(%url, "opening sender relay ws");
    let (ws, _) = connect_async(&url).await.context("ws connect")?;
    let (mut sink, _stream) = ws.split();

    let cfg = RelayConfig {
        v: 1,
        codec: "vp8",
        audio: AudioConfig { codec: "opus", rate: 48_000, channels: 2 },
    };
    sink.send(Message::Text(serde_json::to_string(&cfg)?)).await?;

    loop {
        tokio::select! {
            v = video.recv() => {
                match v {
                    Ok(f) => {
                        sink.send(Message::Binary(frame_to_bytes(&f))).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(n, "video relay lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            a = audio.recv() => {
                match a {
                    Ok(f) => {
                        sink.send(Message::Binary(frame_to_bytes(&f))).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(n, "audio relay lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    Ok(())
}

pub async fn run_viewer(backend: &str, id: &str, sink: FrameSink) -> Result<()> {
    let url = ws_url(backend, id, "viewer", None);
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
