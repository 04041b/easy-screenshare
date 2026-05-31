use clap::{Parser, Subcommand};
use screenshare::{app, capture, webrtc_client};

const DEFAULT_BACKEND: &str = "https://screenshare-backend.04041b.workers.dev";

#[derive(Parser, Debug)]
#[command(name = "screenshare", version, about = "Install-free screen sharing")]
struct Cli {
    /// Override the backend Worker URL.
    #[arg(long, env = "SCREENSHARE_BACKEND", default_value = DEFAULT_BACKEND)]
    backend: String,

    #[command(subcommand)]
    cmd: Option<Mode>,
}

#[derive(Subcommand, Debug)]
enum Mode {
    /// Start sharing your screen immediately (headless, prints the share URL).
    Share,
    /// Join a share as a viewer in a native window.
    View {
        code: String,
        /// 6-digit PIN shown by the sender.
        pin: String,
    },
    /// Diagnose screen capture: report permission/display state and grab one frame.
    Probe,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("screenshare=info,warn")),
        )
        .init();

    let cli = Cli::parse();
    let backend = cli.backend.trim_end_matches('/').to_string();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    match cli.cmd {
        Some(Mode::Share) => rt.block_on(webrtc_client::sender::run_headless(&backend)),
        Some(Mode::View { code, pin }) => rt.block_on(webrtc_client::viewer::run_native(&backend, &code, &pin)),
        Some(Mode::Probe) => rt.block_on(probe_capture()),
        None => app::run_gui(rt, backend),
    }
}

/// Exercises the real capture path so screen-recording permission can be
/// verified without standing up a viewer. Capture in `share` only starts after
/// a viewer answers, which makes permission problems hard to diagnose.
async fn probe_capture() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        println!("capture backend     = scap (Linux)");
        println!("scap is_supported   = {}", scap::is_supported());
        println!("scap has_permission = {}", scap::has_permission());
        let displays = scap::get_all_targets()
            .into_iter()
            .filter(|t| matches!(t, scap::Target::Display(_)))
            .count();
        println!("capturable displays = {displays}");
    }
    #[cfg(target_os = "macos")]
    {
        use screencapturekit::prelude::SCShareableContent;
        println!("capture backend     = screencapturekit (direct, no scap)");
        match SCShareableContent::get() {
            Ok(content) => {
                let displays = content.displays();
                println!("capturable displays = {}", displays.len());
            }
            Err(e) => {
                println!("SCShareableContent::get failed: {e}");
                println!("(open System Settings ▸ Privacy & Security ▸ Screen Recording, enable this binary, relaunch)");
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        println!("capture backend     = windows-capture (direct, no scap)");
    }

    // start_av builds the full capture session the share path uses, so
    // any permission or device error surfaces here. Drop the audio rx —
    // the probe is video-only — but keep the AudioCapture handle alive so
    // the macOS SCStream isn't torn down by Arc-drop before the first
    // video frame lands.
    let (mut video, _audio_keep_alive) = capture::start_av(capture::Quality::default())?;
    println!("capture::start_av ok — waiting up to 5s for the first frame...");
    match tokio::time::timeout(std::time::Duration::from_secs(5), video.rx.recv()).await {
        Ok(Some(f)) => {
            println!("OK: first frame {}x{} stride={} ({} bytes)", f.width, f.height, f.stride, f.data.len());
            Ok(())
        }
        Ok(None) => anyhow::bail!("capture channel closed before delivering a frame"),
        Err(_) => anyhow::bail!("timed out after 5s with no frame — capture is not producing data"),
    }
}
