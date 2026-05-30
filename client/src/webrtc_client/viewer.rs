use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Notify;
use tokio::time::sleep;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

use crate::fallback;
use crate::render;
use crate::signaling::SignalingClient;
use crate::webrtc_client::STUN_SERVERS;

pub async fn run_native(backend: &str, code: &str, pin: &str) -> Result<()> {
    let code = code.to_uppercase();
    if !pin.chars().all(|c| c.is_ascii_digit()) || pin.len() != 6 {
        anyhow::bail!("PIN must be exactly 6 digits");
    }
    let signaling = SignalingClient::new(backend);

    // 1. Fetch offer (with patience for sender still gathering)
    let mut offer = None;
    for _ in 0..60 {
        if let Some(o) = signaling.get_offer(&code, pin).await? {
            offer = Some(o);
            break;
        }
        sleep(Duration::from_secs(1)).await;
    }
    let offer = offer.context("offer never appeared")?;

    // 2. Build PC
    let mut media = MediaEngine::default();
    media.register_default_codecs()?;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media)?;
    let api = APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .build();

    let config = RTCConfiguration {
        ice_servers: STUN_SERVERS
            .iter()
            .map(|u| RTCIceServer { urls: vec![(*u).into()], ..Default::default() })
            .collect(),
        ..Default::default()
    };
    let pc = Arc::new(api.new_peer_connection(config).await?);

    pc.add_transceiver_from_kind(
        RTPCodecType::Video,
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            send_encodings: vec![],
        }),
    )
    .await?;
    pc.add_transceiver_from_kind(
        RTPCodecType::Audio,
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            send_encodings: vec![],
        }),
    )
    .await?;

    let frame_sink = render::start_native_window()?;

    pc.on_track(Box::new({
        let sink = frame_sink.clone();
        move |track, _receiver, _transceiver| {
            let sink = sink.clone();
            Box::pin(async move {
                let mime = track.codec().capability.mime_type.clone();
                tracing::info!(%mime, "remote track");
                if mime.eq_ignore_ascii_case("video/vp8") {
                    render::pump_vp8_track(track, sink).await;
                }
                // audio rendering with cpal output is left for follow-up;
                // for v1 the native viewer focuses on video display.
            })
        }
    }));

    let connected = Arc::new(Notify::new());
    let failed = Arc::new(Notify::new());
    {
        let c = connected.clone();
        let f = failed.clone();
        pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            tracing::info!(?s, "pc state");
            let c = c.clone();
            let f = f.clone();
            Box::pin(async move {
                match s {
                    RTCPeerConnectionState::Connected => c.notify_waiters(),
                    RTCPeerConnectionState::Failed
                    | RTCPeerConnectionState::Disconnected
                    | RTCPeerConnectionState::Closed => f.notify_waiters(),
                    _ => {}
                }
            })
        }));
    }

    pc.set_remote_description(RTCSessionDescription::offer(offer.sdp)?)
        .await?;
    let answer = pc.create_answer(None).await?;
    let mut gather = pc.gathering_complete_promise().await;
    pc.set_local_description(answer).await?;
    gather.recv().await;
    let local = pc.local_description().await.context("no local desc")?;
    signaling.put_answer(&code, pin, &local.sdp).await?;

    // Match the sender's 4s budget (see sender.rs). A successful P2P handshake
    // is sub-2s; anything longer is the sender stalling on DTLS and we should
    // get to the relay quickly.
    let connect_timeout = sleep(Duration::from_secs(4));
    tokio::pin!(connect_timeout);
    tokio::select! {
        _ = connected.notified() => {
            tracing::info!("viewer connected via WebRTC");
            // Block forever — frames flow via on_track until pc closes.
            failed.notified().await;
            Ok(())
        }
        _ = failed.notified() => {
            tracing::warn!("viewer WebRTC failed, polling for fallback");
            wait_and_relay(backend, &code, pin, frame_sink).await
        }
        _ = &mut connect_timeout => {
            tracing::warn!("viewer WebRTC timeout, polling for fallback");
            wait_and_relay(backend, &code, pin, frame_sink).await
        }
    }
}

async fn wait_and_relay(backend: &str, code: &str, pin: &str, sink: render::FrameSink) -> Result<()> {
    let signaling = SignalingClient::new(backend);
    for _ in 0..30 {
        if signaling.get_fallback(code).await.unwrap_or(false) {
            return fallback::run_viewer(backend, code, pin, sink).await;
        }
        sleep(Duration::from_secs(2)).await;
    }
    anyhow::bail!("no fallback became available")
}
