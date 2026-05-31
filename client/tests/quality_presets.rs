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
}

#[test]
fn quality_bitrate_matches_readme() {
    assert_eq!(Quality::Low.bitrate_kbps(), 1_200);
    assert_eq!(Quality::Medium.bitrate_kbps(), 2_500);
    assert_eq!(Quality::High.bitrate_kbps(), 4_000);
}

#[test]
fn quality_resolution_matches_readme() {
    assert_eq!(Quality::Low.resolution(), Resolution::P720);
    assert_eq!(Quality::Medium.resolution(), Resolution::P1080);
    assert_eq!(Quality::High.resolution(), Resolution::P1080);
}

#[test]
fn resolution_widths() {
    // No `Native` variant in this build — `width()` returns u32 directly.
    assert_eq!(Resolution::P480.width(), 640);
    assert_eq!(Resolution::P720.width(), 1280);
    assert_eq!(Resolution::P1080.width(), 1920);
    assert_eq!(Resolution::P1440.width(), 2560);
    assert_eq!(Resolution::P2160.width(), 3840);
}
