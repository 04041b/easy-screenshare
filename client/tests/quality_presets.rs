//! Document the Quality / Resolution presets the README advertises. If
//! someone tweaks a preset, this test trips and the README needs to follow.

use screenshare::capture::{Quality, Resolution};

#[test]
fn quality_default_is_high() {
    assert_eq!(Quality::default(), Quality::High);
}

#[test]
fn quality_fps_matches_readme() {
    assert_eq!(Quality::Low.fps(), 15);
    assert_eq!(Quality::Medium.fps(), 24);
    assert_eq!(Quality::High.fps(), 30);
    assert_eq!(Quality::Ultra.fps(), 60);
    assert_eq!(Quality::Original.fps(), 30);
}

#[test]
fn quality_bitrate_matches_readme() {
    assert_eq!(Quality::Low.bitrate_kbps(), 1_200);
    assert_eq!(Quality::Medium.bitrate_kbps(), 2_500);
    assert_eq!(Quality::High.bitrate_kbps(), 4_000);
    assert_eq!(Quality::Ultra.bitrate_kbps(), 8_000);
    assert_eq!(Quality::Original.bitrate_kbps(), 8_000);
}

#[test]
fn quality_resolution_matches_readme() {
    assert_eq!(Quality::Low.resolution(), Resolution::P720);
    assert_eq!(Quality::Medium.resolution(), Resolution::P1080);
    assert_eq!(Quality::High.resolution(), Resolution::P1080);
    assert_eq!(Quality::Ultra.resolution(), Resolution::P1080);
    assert_eq!(Quality::Original.resolution(), Resolution::Native);
}

#[test]
fn original_bitrate_scales_with_capture_dimensions() {
    // Fixed presets ignore dimensions and return their static bitrate.
    assert_eq!(Quality::High.bitrate_kbps_for_capture(3840, 2160), 4_000);
    assert_eq!(Quality::Ultra.bitrate_kbps_for_capture(3840, 2160), 8_000);

    // Original below the 1080p crossover stays at the static floor.
    assert_eq!(Quality::Original.bitrate_kbps_for_capture(1280, 720), 8_000);

    // 4K30 lands around 24 Mbps at 0.10 bits/pixel/second.
    let kbps_4k = Quality::Original.bitrate_kbps_for_capture(3840, 2160);
    assert!((20_000..=28_000).contains(&kbps_4k), "4K30 got {kbps_4k}");

    // 5K30 is clamped to the 40 Mbps ceiling.
    assert_eq!(Quality::Original.bitrate_kbps_for_capture(5120, 2880), 40_000);
}

#[test]
fn resolution_widths() {
    // `Native` returns None so the capture path uses the source display's
    // own width (no downscale). The Np presets carry their nominal width.
    assert_eq!(Resolution::Native.width(), None);
    assert_eq!(Resolution::P480.width(), Some(640));
    assert_eq!(Resolution::P720.width(), Some(1280));
    assert_eq!(Resolution::P1080.width(), Some(1920));
    assert_eq!(Resolution::P1440.width(), Some(2560));
    assert_eq!(Resolution::P2160.width(), Some(3840));
}
