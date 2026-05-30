use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Notify;
use tokio::time::sleep;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_OPUS, MIME_TYPE_VP8};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice::network_type::NetworkType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

use crate::capture::{AudioCapture, VideoCapture};
use crate::fallback;
use crate::signaling::SignalingClient;
use crate::webrtc_client::{codec, STUN_SERVERS};

/// Headless entrypoint: starts sharing and prints the URL + PIN.
pub async fn run_headless(backend: &str) -> Result<()> {
    run_with_callbacks(backend, |info| {
        println!("Share URL: {}", info.viewer_url);
        println!("PIN: {}", info.pin);
    })
    .await
}

#[derive(Clone)]
pub struct ShareInfo {
    pub viewer_url: String,
    pub pin: String,
}

/// Library entrypoint used by both the GUI and headless modes.
pub async fn run_with_callbacks<F>(backend: &str, on_url: F) -> Result<()>
where
    F: FnOnce(ShareInfo) + Send + 'static,
{
    let signaling = SignalingClient::new(backend);

    // 1. Create session
    let session = signaling.create_session().await.context("create session")?;
    on_url(ShareInfo {
        viewer_url: session.viewer_url.clone(),
        pin: session.pin.clone(),
    });
    tracing::info!(id = %session.id, "session created");

    // 2. Build PeerConnection
    let mut media = MediaEngine::default();
    media.register_default_codecs()?;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media)?;
    let mut settings = SettingEngine::default();
    // Restrict to IPv4 only — webrtc-rs's IPv6 gather is noisy on macOS (link-local
    // candidates fail to bind, STUN resolver requires an IPv6 default route) and
    // adds no useful connectivity for typical home/office networks.
    settings.set_network_types(vec![NetworkType::Udp4]);
    let api = APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_setting_engine(settings)
        .build();

    let config = RTCConfiguration {
        ice_servers: STUN_SERVERS
            .iter()
            .map(|u| RTCIceServer { urls: vec![(*u).into()], ..Default::default() })
            .collect(),
        ..Default::default()
    };
    let pc = Arc::new(api.new_peer_connection(config).await?);

    // 3. Tracks
    let video_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_VP8.to_string(),
            ..Default::default()
        },
        "video".to_string(),
        "screenshare".to_string(),
    ));
    pc.add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    let audio_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_OPUS.to_string(),
            clock_rate: 48000,
            channels: 2,
            ..Default::default()
        },
        "audio".to_string(),
        "screenshare".to_string(),
    ));
    pc.add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    // 4. Connection-state watch
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

    // 5. Offer + non-trickle ICE
    let offer = pc.create_offer(None).await?;
    let mut gather_complete = pc.gathering_complete_promise().await;
    pc.set_local_description(offer).await?;
    gather_complete.recv().await;

    let local = pc.local_description().await.context("no local desc after gather")?;
    signaling
        .put_offer(&session.id, &session.sender_token, &local.sdp)
        .await
        .context("put offer")?;
    tracing::info!("offer posted, polling for answer");

    // 6. Poll for answer (up to 60s)
    let mut answer = None;
    for _ in 0..60 {
        if let Some(a) = signaling
            .get_answer(&session.id, &session.sender_token)
            .await?
        {
            answer = Some(a);
            break;
        }
        sleep(Duration::from_secs(1)).await;
    }
    let answer = answer.context("viewer never answered within 60s")?;
    pc.set_remote_description(RTCSessionDescription::answer(answer.sdp)?)
        .await?;

    // 7. Start media pumps now — encoder threads write to tracks regardless of P2P state;
    //    fallback path reuses the same encoded frames over WS if WebRTC fails.
    let (video_pump_handle, video_tap, force_keyframe) = start_video_pump(video_track.clone())?;
    let (audio_pump_handle, audio_tap) = start_audio_pump(audio_track.clone())?;

    // 8. Watch for connection result; on failure, escalate to WS relay
    let connect_timeout = sleep(Duration::from_secs(15));
    tokio::pin!(connect_timeout);

    tokio::select! {
        _ = connected.notified() => {
            tracing::info!("WebRTC connected");
        }
        _ = failed.notified() => {
            tracing::warn!("WebRTC failed, escalating to fallback relay");
            signaling.put_fallback(&session.id, &session.sender_token).await?;
            // The encoder has been running since step 7 with a tiny 16-slot
            // broadcast buffer; the fallback receiver will almost certainly
            // come up Lagged past the only keyframe scap produced at start.
            // Force a fresh keyframe so the late subscriber can decode.
            force_keyframe.store(true, Ordering::Relaxed);
            fallback::run_sender(backend, &session.id, &session.sender_token, video_tap, audio_tap, force_keyframe).await?;
        }
        _ = &mut connect_timeout => {
            tracing::warn!("WebRTC connect timeout, escalating to fallback relay");
            signaling.put_fallback(&session.id, &session.sender_token).await?;
            force_keyframe.store(true, Ordering::Relaxed);
            fallback::run_sender(backend, &session.id, &session.sender_token, video_tap, audio_tap, force_keyframe).await?;
        }
    }

    let _ = video_pump_handle.await;
    let _ = audio_pump_handle.await;
    Ok(())
}

/// Spawns screen capture + VP8 encode on a dedicated OS thread (libvpx's
/// `Encoder` is `!Send` so we can't hold it across `.await`), then drains the
/// encoded packets in an async task to call `track.write_sample()` and to
/// broadcast for the fallback path.
fn start_video_pump(
    track: Arc<TrackLocalStaticSample>,
) -> Result<(
    tokio::task::JoinHandle<()>,
    tokio::sync::broadcast::Receiver<EncodedFrame>,
    Arc<AtomicBool>,
)> {
    let mut capture = VideoCapture::start(30)?;
    let (bcast_tx, bcast_rx) = tokio::sync::broadcast::channel::<EncodedFrame>(16);
    let (enc_tx, mut enc_rx) = tokio::sync::mpsc::unbounded_channel::<EncodedFrame>();
    // Flag the encoder thread polls before each encode. vpx-encode 0.5.0
    // doesn't expose VPX_EFLAG_FORCE_KF, but a freshly constructed Encoder
    // always emits a keyframe as its first packet — so to "force a keyframe"
    // we drop and recreate the encoder. Used by the fallback path because a
    // late WS subscriber will have missed the encoder's only natural keyframe.
    let force_keyframe = Arc::new(AtomicBool::new(false));
    let force_keyframe_thread = Arc::clone(&force_keyframe);

    std::thread::spawn(move || {
        let mut encoder: Option<vpx_encode::Encoder> = None;
        let mut enc_w = 0u32;
        let mut enc_h = 0u32;
        let mut t0_us: Option<u64> = None;
        let mut frames_seen = 0u64;

        while let Some(frame) = capture.rx.blocking_recv() {
            frames_seen += 1;
            // VP8 needs even dimensions and non-zero size. scap can deliver
            // garbage frames before permission is fully granted on macOS —
            // skip those rather than failing the encoder.
            if frame.width < 16 || frame.height < 16 || frame.width % 2 != 0 || frame.height % 2 != 0 {
                if frames_seen < 5 || frames_seen % 30 == 0 {
                    tracing::warn!(
                        w = frame.width, h = frame.height, stride = frame.stride,
                        bytes = frame.data.len(),
                        "dropping unusable capture frame"
                    );
                }
                continue;
            }
            if force_keyframe_thread.swap(false, Ordering::Relaxed) {
                tracing::info!("force_keyframe set — reinitialising encoder");
                encoder = None;
            }
            if encoder.is_none() || enc_w != frame.width || enc_h != frame.height {
                tracing::info!(w = frame.width, h = frame.height, "initializing VP8 encoder");
                let cfg = vpx_encode::Config {
                    width: frame.width,
                    height: frame.height,
                    timebase: [1, 1000],
                    bitrate: 4_000,
                    codec: vpx_encode::VideoCodecId::VP8,
                };
                match vpx_encode::Encoder::new(cfg) {
                    Ok(e) => {
                        encoder = Some(e);
                        enc_w = frame.width;
                        enc_h = frame.height;
                    }
                    Err(e) => {
                        tracing::error!(w = frame.width, h = frame.height, "vpx encoder init failed: {e}");
                        return;
                    }
                }
            }
            let (y, u, v) = match codec::bgra_to_i420(&frame.data, frame.width, frame.height, frame.stride) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("color conv failed: {e}");
                    continue;
                }
            };
            let i420 = codec::pack_i420(&y, &u, &v);
            let base = *t0_us.get_or_insert(frame.timestamp_us);
            let ts_ms = ((frame.timestamp_us - base) / 1000) as i64;
            let enc = encoder.as_mut().unwrap();
            let packets = match enc.encode(ts_ms, &i420) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("encode err: {e}");
                    continue;
                }
            };
            for pkt in packets {
                let ef = EncodedFrame {
                    stream: 0,
                    keyframe: pkt.key,
                    timestamp_us: frame.timestamp_us,
                    data: pkt.data.to_vec(),
                };
                if enc_tx.send(ef).is_err() {
                    return;
                }
            }
        }
    });

    let handle = tokio::spawn(async move {
        while let Some(ef) = enc_rx.recv().await {
            let sample = webrtc::media::Sample {
                data: ef.data.clone().into(),
                duration: Duration::from_micros(33_333),
                ..Default::default()
            };
            if let Err(e) = track.write_sample(&sample).await {
                tracing::warn!("video track write failed: {e}");
            }
            let _ = bcast_tx.send(ef);
        }
    });

    Ok((handle, bcast_rx, force_keyframe))
}

/// Same pattern for audio: opus::Encoder is `!Send`, so the encode loop runs
/// on an OS thread and forwards EncodedFrames to an async forwarder.
fn start_audio_pump(
    track: Arc<TrackLocalStaticSample>,
) -> Result<(tokio::task::JoinHandle<()>, tokio::sync::broadcast::Receiver<EncodedFrame>)> {
    let mut capture = AudioCapture::start()?;
    let (bcast_tx, bcast_rx) = tokio::sync::broadcast::channel::<EncodedFrame>(64);
    let (enc_tx, mut enc_rx) = tokio::sync::mpsc::unbounded_channel::<EncodedFrame>();
    let channels = capture.channels as usize;
    let sample_rate = capture.sample_rate;

    std::thread::spawn(move || {
        let mut encoder = match opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("opus encoder init: {e}");
                return;
            }
        };
        const FRAME_SAMPLES: usize = 960; // 20ms @ 48kHz per channel
        let mut buf: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 2 * 4);
        let mut out = vec![0u8; 4000];
        let mut warned = false;

        while let Some(frame) = capture.rx.blocking_recv() {
            if (sample_rate != 48_000 || channels != 2) && !warned {
                tracing::warn!(sample_rate, channels, "audio not 48kHz stereo; quality may suffer");
                warned = true;
            }
            let stereo: Vec<f32> = if channels == 1 {
                let mut v = Vec::with_capacity(frame.samples.len() * 2);
                for s in frame.samples {
                    v.push(s);
                    v.push(s);
                }
                v
            } else {
                frame.samples
            };
            buf.extend_from_slice(&stereo);

            while buf.len() >= FRAME_SAMPLES * 2 {
                let chunk: Vec<f32> = buf.drain(..FRAME_SAMPLES * 2).collect();
                let n = match encoder.encode_float(&chunk, &mut out) {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!("opus encode: {e}");
                        continue;
                    }
                };
                let ef = EncodedFrame {
                    stream: 1,
                    keyframe: false,
                    timestamp_us: frame.timestamp_us,
                    data: out[..n].to_vec(),
                };
                if enc_tx.send(ef).is_err() {
                    return;
                }
            }
        }
        // Audio capture stopped delivering (no input device, permission denied,
        // or the stream ended). The relay treats audio as best-effort, so just
        // let this thread finish; the broadcast closing is handled downstream.
        tracing::warn!("audio capture ended — continuing without audio");
    });

    let handle = tokio::spawn(async move {
        while let Some(ef) = enc_rx.recv().await {
            let sample = webrtc::media::Sample {
                data: ef.data.clone().into(),
                duration: Duration::from_millis(20),
                ..Default::default()
            };
            if let Err(e) = track.write_sample(&sample).await {
                tracing::warn!("audio track write failed: {e}");
            }
            let _ = bcast_tx.send(ef);
        }
    });

    Ok((handle, bcast_rx))
}

#[derive(Clone)]
pub struct EncodedFrame {
    pub stream: u8, // 0 = video (VP8), 1 = audio (Opus)
    pub keyframe: bool,
    pub timestamp_us: u64,
    pub data: Vec<u8>,
}
