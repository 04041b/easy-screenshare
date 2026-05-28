use anyhow::{Context, Result};
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
    _stream: cpal::Stream,
}

impl AudioCapture {
    /// Start capturing system audio.
    ///
    /// Platform notes:
    /// - macOS 13+: cpal cannot capture system audio directly. scap's
    ///   ScreenCaptureKit backend can provide audio alongside video; for v1
    ///   we capture mic input here and document the SCKit-audio integration as
    ///   a follow-up.
    /// - Windows: uses WASAPI loopback by opening the default output device.
    /// - Linux: requires a PulseAudio/PipeWire loopback (`pactl load-module
    ///   module-loopback`) and selecting the monitor source as default input.
    pub fn start() -> Result<Self> {
        let host = cpal::default_host();

        #[cfg(target_os = "windows")]
        let device = host
            .default_output_device()
            .context("no default output device")?;

        #[cfg(not(target_os = "windows"))]
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
}
