mod app;
mod capture;
mod fallback;
mod render;
mod signaling;
mod webrtc_client;

use clap::{Parser, Subcommand};

const DEFAULT_BACKEND: &str = "https://screenshare-backend.example.workers.dev";

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
        None => app::run_gui(rt, backend),
    }
}
