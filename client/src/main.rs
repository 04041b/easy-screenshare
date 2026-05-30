mod app;
mod capture;
mod fallback;
mod render;
mod signaling;
mod webrtc_client;

use clap::{Parser, Subcommand};

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
    println!("scap is_supported   = {}", scap::is_supported());
    println!("scap has_permission = {}", scap::has_permission());
    let displays = scap::get_all_targets()
        .into_iter()
        .filter(|t| matches!(t, scap::Target::Display(_)))
        .count();
    println!("capturable displays = {displays}");

    let mut capture = capture::VideoCapture::start(30)?;
    println!("VideoCapture::start ok — waiting up to 5s for the first frame...");
    match tokio::time::timeout(std::time::Duration::from_secs(5), capture.rx.recv()).await {
        Ok(Some(f)) => {
            println!("OK: first frame {}x{} stride={} ({} bytes)", f.width, f.height, f.stride, f.data.len());
            Ok(())
        }
        Ok(None) => anyhow::bail!("capture channel closed before delivering a frame"),
        Err(_) => anyhow::bail!("timed out after 5s with no frame — capture is not producing data"),
    }
}
