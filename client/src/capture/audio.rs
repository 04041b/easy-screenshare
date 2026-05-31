#[cfg(not(target_os = "macos"))]
use anyhow::Result;
#[cfg(target_os = "linux")]
use anyhow::Context;
#[cfg(target_os = "linux")]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc;

/// One captured audio frame: interleaved f32 samples at `sample_rate`, `channels` channels.
pub struct AudioFrame {
    pub samples: Vec<f32>,
    pub channels: u16,
    pub sample_rate: u32,
    pub timestamp_us: u64,
}

pub struct AudioCapture {
    pub rx: mpsc::Receiver<AudioFrame>,
    pub sample_rate: u32,
    pub channels: u16,
    // On Linux we hold the cpal Stream to keep its callback thread alive.
    // On Windows we manage the WASAPI loopback thread ourselves; it self-exits
    // when the mpsc Receiver is dropped (Sender::try_send returns Closed).
    // On macOS the audio frames come from the shared SCStream owned by
    // `MacAvSession`; we hold a clone of that Arc here so the stream lives
    // until both this capture and the paired VideoCapture drop.
    #[cfg(target_os = "linux")]
    _stream: cpal::Stream,
    #[cfg(target_os = "macos")]
    pub(crate) _session: std::sync::Arc<super::video::macos_impl::MacAvSession>,
}

impl AudioCapture {
    /// Start capturing system / mic audio.
    ///
    /// Platform notes:
    /// - macOS: there is **no** standalone audio constructor — system
    ///   audio is captured by the same `SCStream` as video. Use
    ///   [`crate::capture::start_av`] instead, which returns the audio
    ///   half alongside the video half.
    /// - Windows: WASAPI loopback via the `wasapi` crate, which sets
    ///   `AUDCLNT_STREAMFLAGS_LOOPBACK` when initialising a Render
    ///   device in Capture direction. cpal opens the default output as
    ///   an input but never sets that flag, so callbacks never fire.
    /// - Linux: requires a PulseAudio/PipeWire loopback (`pactl load-module
    ///   module-loopback`) and selecting the monitor source as default input.
    #[cfg(not(target_os = "macos"))]
    pub fn start() -> Result<Self> {
        #[cfg(target_os = "windows")]
        {
            Self::start_wasapi_loopback()
        }
        #[cfg(target_os = "linux")]
        {
            Self::start_cpal()
        }
    }

    #[cfg(target_os = "linux")]
    fn start_cpal() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("no default input device")?;

        let config = device
            .default_input_config()
            .or_else(|_| device.default_output_config())
            .context("no usable audio config")?;
        let sample_rate = config.sample_rate().0;
        let channels = config.channels();
        let stream_config: cpal::StreamConfig = config.clone().into();

        let (tx, rx) = mpsc::channel::<AudioFrame>(32);
        let start = std::time::Instant::now();

        let err_fn = |e| tracing::warn!("audio stream error: {e}");
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device.build_input_stream(
                &stream_config,
                move |data: &[f32], _| {
                    let ts = start.elapsed().as_micros() as u64;
                    let _ = tx.try_send(AudioFrame {
                        samples: data.to_vec(),
                        channels,
                        sample_rate,
                        timestamp_us: ts,
                    });
                },
                err_fn,
                None,
            )?,
            cpal::SampleFormat::I16 => device.build_input_stream(
                &stream_config,
                move |data: &[i16], _| {
                    let ts = start.elapsed().as_micros() as u64;
                    let f: Vec<f32> = data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                    let _ = tx.try_send(AudioFrame {
                        samples: f,
                        channels,
                        sample_rate,
                        timestamp_us: ts,
                    });
                },
                err_fn,
                None,
            )?,
            other => anyhow::bail!("unsupported sample format: {other:?}"),
        };

        stream.play()?;

        Ok(Self {
            rx,
            sample_rate,
            channels,
            _stream: stream,
        })
    }

    #[cfg(target_os = "windows")]
    fn start_wasapi_loopback() -> Result<Self> {
        use std::collections::VecDeque;
        use wasapi::{
            get_default_device, initialize_mta, Direction, SampleType, ShareMode, WaveFormat,
        };

        // WASAPI / COM types from `windows-core` hold `NonNull<c_void>` and
        // are `!Send`, so they cannot be moved into a worker thread after
        // being constructed on the caller. Instead, do *all* of COM init,
        // device acquisition, IAudioClient setup, and the capture loop on
        // the worker thread, and signal init success/failure back through a
        // synchronous oneshot.
        let sample_rate: u32 = 48_000;
        let channels: u16 = 2;
        let (tx, rx) = mpsc::channel::<AudioFrame>(32);
        let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
        let start = std::time::Instant::now();

        std::thread::spawn(move || {
            let setup = || -> Result<(_, _, _, usize)> {
                initialize_mta()
                    .ok()
                    .map_err(|e| anyhow::anyhow!("wasapi MTA init: {e:?}"))?;
                let device = get_default_device(&Direction::Render)
                    .map_err(|e| anyhow::anyhow!("default render device: {e:?}"))?;
                let mut audio_client = device
                    .get_iaudioclient()
                    .map_err(|e| anyhow::anyhow!("get iaudioclient: {e:?}"))?;
                // Ask WASAPI to deliver what our Opus encoder wants. The
                // 5th arg (`convert = true`) sets AUTOCONVERTPCM +
                // SRC_DEFAULT_QUALITY, so WASAPI resamples/format-converts
                // from whatever the device runs at — no manual resampler.
                let desired_format = WaveFormat::new(
                    32,
                    32,
                    &SampleType::Float,
                    sample_rate as usize,
                    channels as usize,
                    None,
                );
                let (_def_time, min_time) = audio_client
                    .get_periods()
                    .map_err(|e| anyhow::anyhow!("get_periods: {e:?}"))?;
                // Render-device direction + Capture init direction is the
                // magic combo wasapi-0.15 uses to set
                // AUDCLNT_STREAMFLAGS_LOOPBACK (see wasapi-0.15/src/api.rs:777).
                audio_client
                    .initialize_client(
                        &desired_format,
                        min_time,
                        &Direction::Capture,
                        &ShareMode::Shared,
                        true,
                    )
                    .map_err(|e| anyhow::anyhow!("initialize_client: {e:?}"))?;
                let h_event = audio_client
                    .set_get_eventhandle()
                    .map_err(|e| anyhow::anyhow!("set_get_eventhandle: {e:?}"))?;
                let capture_client = audio_client
                    .get_audiocaptureclient()
                    .map_err(|e| anyhow::anyhow!("get_audiocaptureclient: {e:?}"))?;
                audio_client
                    .start_stream()
                    .map_err(|e| anyhow::anyhow!("start_stream: {e:?}"))?;
                let block_align = desired_format.get_blockalign() as usize; // 8 bytes/frame for stereo f32
                Ok((audio_client, h_event, capture_client, block_align))
            };

            let (_audio_client, h_event, capture_client, block_align) = match setup() {
                Ok(v) => {
                    let _ = init_tx.send(Ok(()));
                    v
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            // 20ms packets to match the Opus frame size downstream.
            let packet_bytes = 960 * block_align;
            let mut byte_queue: VecDeque<u8> = VecDeque::with_capacity(packet_bytes * 8);
            let mut first_logged = false;
            loop {
                // 1000ms event timeout — long enough that idle playback
                // (no sound being rendered) doesn't burn CPU; short enough
                // that shutdown via Receiver-drop is detected within ~1s.
                if h_event.wait_for_event(1000).is_err() {
                    if tx.is_closed() {
                        break;
                    }
                    continue;
                }
                if let Err(e) = capture_client.read_from_device_to_deque(&mut byte_queue) {
                    tracing::warn!("wasapi read error: {e:?}");
                    break;
                }
                while byte_queue.len() >= packet_bytes {
                    let mut bytes = Vec::with_capacity(packet_bytes);
                    for _ in 0..packet_bytes {
                        bytes.push(byte_queue.pop_front().unwrap());
                    }
                    let samples: Vec<f32> = bytes
                        .chunks_exact(4)
                        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect();
                    if !first_logged {
                        tracing::info!(
                            samples = samples.len(),
                            sample_rate,
                            channels,
                            "first WASAPI loopback packet"
                        );
                        first_logged = true;
                    }
                    let ts = start.elapsed().as_micros() as u64;
                    match tx.try_send(AudioFrame {
                        samples,
                        channels,
                        sample_rate,
                        timestamp_us: ts,
                    }) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            // Encoder is behind; drop this 20ms. Opus frames
                            // are independently decodable so a drop is just
                            // a glitch.
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
            }
            tracing::info!("wasapi loopback thread ending");
        });

        // Block briefly for the init result. The thread sends Ok/Err once
        // it's either ready to capture or has hit a fatal setup error.
        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                rx,
                sample_rate,
                channels,
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => anyhow::bail!("wasapi loopback thread died before init"),
        }
    }
}
