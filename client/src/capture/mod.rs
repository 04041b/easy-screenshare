pub mod audio;
pub mod video;

/// Drop the calling thread's OS scheduling priority to BELOW_NORMAL on
/// Windows. No-op everywhere else. Used by the capture reader and the
/// VP8 encoder threads so that, when the user is running a game on the
/// same machine, the game's render thread (which usually sits at NORMAL
/// or ABOVE_NORMAL) keeps scheduler priority and doesn't get knocked
/// down to ~30 fps fighting our encode loop for cores.
pub fn lower_thread_priority_for_background_work() {
    #[cfg(target_os = "windows")]
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

pub use audio::{AudioCapture, AudioFrame};
pub use scap::capturer::Resolution;
pub use video::{VideoCapture, VideoFrame};

/// User-facing video quality preset. Selected in the GUI before sharing
/// starts; locked in for the duration of the share. Lower presets shrink
/// pixel volume, frame rate, and encoded bitrate together — the usual fix
/// for relay-fallback lag on slow uplinks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Low,
    Medium,
    High,
}

impl Quality {
    pub fn fps(self) -> u32 {
        match self {
            Self::Low => 15,
            Self::Medium => 24,
            Self::High => 30,
        }
    }
    pub fn resolution(self) -> Resolution {
        match self {
            Self::Low => Resolution::_720p,
            Self::Medium | Self::High => Resolution::_1080p,
        }
    }
    pub fn bitrate_kbps(self) -> u32 {
        match self {
            Self::Low => 1_200,
            Self::Medium => 2_500,
            Self::High => 4_000,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Low (720p · 15 fps · 1.2 Mbps)",
            Self::Medium => "Medium (1080p · 24 fps · 2.5 Mbps)",
            Self::High => "High (1080p · 30 fps · 4 Mbps)",
        }
    }
}

impl Default for Quality {
    fn default() -> Self {
        Self::High
    }
}
