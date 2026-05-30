use anyhow::{Context, Result};
#[cfg(not(target_os = "windows"))]
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
    // On macOS/Linux we hold the cpal Stream to keep its callback thread alive.
    // On Windows we manage the WASAPI loopback thread ourselves; it self-exits
    // when the tokio mpsc Receiver is dropped (Sender::try_send returns Closed).
    #[cfg(not(target_os = "windows"))]
    _stream: cpal::Stream,
}

impl AudioCapture {
    /// Start capturing system / mic audio.
    ///
    /// Platform notes:
    /// - macOS 13+: cpal cannot capture system audio directly. scap's
    ///   ScreenCaptureKit backend can provide audio alongside video; for v1
    ///   we capture mic input here and document the SCKit-audio integration as
    ///   a follow-up.
    /// - Windows: uses WASAPI loopback via the `wasapi` crate, which sets
    ///   `AUDCLNT_STREAMFLAGS_LOOPBACK` for us when initializing a Render
    ///   device in Capture direction. The previous cpal-based path opened the
    ///   default output as an input but never set that flag, so callbacks
    ///   never fired and the audio broadcast closed immediately.
    /// - Linux: requires a PulseAudio/PipeWire loopback (`pactl load-module
    ///   module-loopback`) and selecting the monitor source as default input.
    pub fn start() -> Result<Self> {
        #[cfg(target_os = "windows")]
        {
            Self::start_wasapi_loopback()
        }
        #[cfg(not(target_os = "windows"))]
        {
            Self::start_cpal()
        }
    }

    #[cfg(not(target_os = "windows"))]
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

        // COM init. `initialize_mta()` returns Ok if MTA is already initialized
        // on this thread or sets it if not.
        initialize_mta()
            .ok()
            .map_err(|e| anyhow::anyhow!("wasapi MTA init: {e:?}"))?;

        let device = get_default_device(&Direction::Render)
            .map_err(|e| anyhow::anyhow!("default render device: {e:?}"))?;
        let mut audio_client = device
            .get_iaudioclient()
            .map_err(|e| anyhow::anyhow!("get iaudioclient: {e:?}"))?;

        // Ask WASAPI to deliver exactly what our Opus encoder wants. The
        // 5th arg (`convert = true`) sets AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        // + SRC_DEFAULT_QUALITY, so WASAPI will resample/format-convert from
        // whatever the device runs at — no manual resampler needed.
        let sample_rate: u32 = 48_000;
        let channels: u16 = 2;
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
        // Device direction Render + init direction Capture is the magic combo
        // wasapi uses to set AUDCLNT_STREAMFLAGS_LOOPBACK (see wasapi-0.15/src/api.rs).
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

        let (tx, rx) = mpsc::channel::<AudioFrame>(32);
        let start = std::time::Instant::now();
        let block_align = desired_format.get_blockalign() as usize; // bytes per frame (8 for stereo f32)
        // Hand off the encoded packet boundary roughly every 20ms (Opus frame).
        // 960 frames * 8 bytes = 7680 bytes per packet.
        let packet_bytes = 960 * block_align;

        std::thread::spawn(move || {
            // Keep the audio_client alive in this thread; dropping it tears
            // down the WASAPI stream.
            let _ac = audio_client;
            let mut byte_queue: VecDeque<u8> = VecDeque::with_capacity(packet_bytes * 8);
            let mut first_logged = false;
            loop {
                // 1000ms event timeout — long enough that idle playback (no
                // sound being rendered) doesn't burn CPU, short enough that
                // shutdown via Receiver-drop is detected within ~1s.
                if h_event.wait_for_event(1000).is_err() {
                    // Either timeout or stream stopped; keep waiting unless the
                    // mpsc has closed.
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
                            // Encoder is behind; drop this 20ms. Opus frames are
                            // independently decodable so a drop is just a glitch.
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
            }
            tracing::info!("wasapi loopback thread ending");
        });

        Ok(Self {
            rx,
            sample_rate,
            channels,
        })
    }
}
