use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Notify;
use tokio::time::sleep;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_OPUS, MIME_TYPE_VP8};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTPCodecType};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

use crate::capture::{AudioCapture, VideoCapture};
use crate::fallback;
use crate::signaling::SignalingClient;
use crate::webrtc_client::{codec, STUN_SERVERS};

/// Headless entrypoint: starts sharing and prints the URL.
pub async fn run_headless(backend: &str) -> Result<()> {
    run_with_callbacks(backend, |url| {
        println!("Share URL: {url}");
    })
    .await
}

/// Library entrypoint used by both the GUI and headless modes.
pub async fn run_with_callbacks<F>(backend: &str, on_url: F) -> Result<()>
where
    F: FnOnce(String) + Send + 'static,
{
    let signaling = SignalingClient::new(backend);

    // 1. Create session
    let session = signaling.create_session().await.context("create session")?;
    on_url(session.viewer_url.clone());
    tracing::info!(id = %session.id, "session created");

    // 2. Build PeerConnection
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
    let (video_pump_handle, video_tap) = start_video_pump(video_track.clone())?;
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
            fallback::run_sender(backend, &session.id, &session.sender_token, video_tap, audio_tap).await?;
        }
        _ = &mut connect_timeout => {
            tracing::warn!("WebRTC connect timeout, escalating to fallback relay");
            signaling.put_fallback(&session.id, &session.sender_token).await?;
            fallback::run_sender(backend, &session.id, &session.sender_token, video_tap, audio_tap).await?;
        }
    }

    // Keep alive until pumps exit or pc closes
    let _ = video_pump_handle.await;
    let _ = audio_pump_handle.await;
    Ok(())
}

/// Spawns the screen capture + VP8 encoder. Returns a join handle and a tap that
/// also receives every encoded frame for the fallback WS relay.
fn start_video_pump(
    track: Arc<TrackLocalStaticSample>,
) -> Result<(tokio::task::JoinHandle<()>, tokio::sync::broadcast::Receiver<EncodedFrame>)> {
    let mut capture = VideoCapture::start(30)?;
    let (bcast_tx, bcast_rx) = tokio::sync::broadcast::channel::<EncodedFrame>(16);

    let handle = tokio::spawn(async move {
        // Lazily build the encoder once we know the actual capture dimensions
        // (scap may downscale based on Resolution hint).
        let mut encoder: Option<vpx_encode::Encoder> = None;
        let mut enc_w = 0u32;
        let mut enc_h = 0u32;
        let mut t0_us: Option<u64> = None;

        while let Some(frame) = capture.rx.recv().await {
            // (Re)build encoder on resolution change
            if encoder.is_none() || enc_w != frame.width || enc_h != frame.height {
                let cfg = vpx_encode::Config {
                    width: frame.width,
                    height: frame.height,
                    timebase: [1, 1000], // ms timebase
                    bitrate: 4_000,       // kbps
                    codec: vpx_encode::VideoCodecId::VP8,
                };
                match vpx_encode::Encoder::new(cfg) {
                    Ok(e) => {
                        encoder = Some(e);
                        enc_w = frame.width;
                        enc_h = frame.height;
                    }
                    Err(e) => {
                        tracing::error!("vpx encoder init failed: {e}");
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
                let data = pkt.data.to_vec();
                let keyframe = pkt.key;
                let duration = Duration::from_micros(33_333);
                let sample = webrtc::media::Sample {
                    data: data.clone().into(),
                    duration,
                    ..Default::default()
                };
                if let Err(e) = track.write_sample(&sample).await {
                    tracing::warn!("track write failed: {e}");
                }
                let _ = bcast_tx.send(EncodedFrame {
                    stream: 0,
                    keyframe,
                    timestamp_us: frame.timestamp_us,
                    data,
                });
            }
        }
    });

    Ok((handle, bcast_rx))
}

fn start_audio_pump(
    track: Arc<TrackLocalStaticSample>,
) -> Result<(tokio::task::JoinHandle<()>, tokio::sync::broadcast::Receiver<EncodedFrame>)> {
    let mut capture = AudioCapture::start()?;
    let (bcast_tx, bcast_rx) = tokio::sync::broadcast::channel::<EncodedFrame>(64);
    let channels = capture.channels as usize;
    let sample_rate = capture.sample_rate;

    let handle = tokio::spawn(async move {
        // Opus encoder: enforce 48 kHz stereo for compatibility with browser default.
        let mut encoder = match opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("opus encoder init: {e}");
                return;
            }
        };
        let mut buf: Vec<f32> = Vec::with_capacity(48_000); // 1s of stereo
        // Frame size for 20ms @ 48kHz stereo = 960 samples/ch * 2 = 1920 interleaved
        const FRAME_SAMPLES: usize = 960;
        let mut out = vec![0u8; 4000];

        while let Some(frame) = capture.rx.recv().await {
            // Resample/upmix as needed (very simple: assume rate==48000 stereo; otherwise warn)
            if sample_rate != 48_000 || channels != 2 {
                if buf.is_empty() {
                    tracing::warn!(
                        sample_rate, channels,
                        "audio not 48kHz stereo — frames will be passed through without resampling; quality may suffer"
                    );
                }
                // Simple stereo upmix from mono if needed
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
            } else {
                buf.extend_from_slice(&frame.samples);
            }

            // Drain in 20ms stereo chunks
            while buf.len() >= FRAME_SAMPLES * 2 {
                let chunk: Vec<f32> = buf.drain(..FRAME_SAMPLES * 2).collect();
                let n = match encoder.encode_float(&chunk, &mut out) {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!("opus encode: {e}");
                        continue;
                    }
                };
                let data = out[..n].to_vec();
                let sample = webrtc::media::Sample {
                    data: data.clone().into(),
                    duration: Duration::from_millis(20),
                    ..Default::default()
                };
                if let Err(e) = track.write_sample(&sample).await {
                    tracing::warn!("audio track write failed: {e}");
                }
                let _ = bcast_tx.send(EncodedFrame {
                    stream: 1,
                    keyframe: false,
                    timestamp_us: frame.timestamp_us,
                    data,
                });
            }
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
