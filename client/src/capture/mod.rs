pub mod audio;
pub mod video;

pub use audio::{AudioCapture, AudioFrame};
pub use video::{VideoCapture, VideoFrame};

/// A target capture resolution, expressed as one of the standard "Np"
/// heights. We use our own enum here instead of re-exporting scap's so the
/// public API is uniform across platforms — scap isn't a dependency on
/// Windows since we use `windows-capture` directly there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    P480,
    P720,
    P1080,
    P1440,
    P2160,
}

impl Resolution {
    /// Pixel width for this preset. Height is derived from the source's
    /// actual aspect ratio at use sites.
    pub fn width(self) -> u32 {
        match self {
            Self::P480 => 640,
            Self::P720 => 1280,
            Self::P1080 => 1920,
            Self::P1440 => 2560,
            Self::P2160 => 3840,
        }
    }
}

#[cfg(not(target_os = "windows"))]
impl From<Resolution> for scap::capturer::Resolution {
    fn from(r: Resolution) -> Self {
        match r {
            Resolution::P480 => Self::_480p,
            Resolution::P720 => Self::_720p,
            Resolution::P1080 => Self::_1080p,
            Resolution::P1440 => Self::_1440p,
            Resolution::P2160 => Self::_2160p,
        }
    }
}

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
            Self::Low => Resolution::P720,
            Self::Medium | Self::High => Resolution::P1080,
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
