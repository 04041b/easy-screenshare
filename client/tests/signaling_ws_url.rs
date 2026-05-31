//! Tests for `signaling::ws_url`. Documents the actual scheme transform,
//! the four sender/viewer call shapes, and the (current) behaviour with a
//! trailing slash on the base URL.

use screenshare::signaling::ws_url;

#[test]
fn sender_https_base_uses_wss_and_appends_token() {
    let url = ws_url(
        "https://api.example.com",
        "abc",
        "sender",
        Some("tok123"),
        None,
    );
    assert_eq!(
        url,
        "wss://api.example.com/ws/relay/abc?role=sender&token=tok123"
    );
}

#[test]
fn viewer_https_base_uses_wss_and_appends_pin() {
    let url = ws_url(
        "https://api.example.com",
        "abc",
        "viewer",
        None,
        Some("987654"),
    );
    assert_eq!(
        url,
        "wss://api.example.com/ws/relay/abc?role=viewer&pin=987654"
    );
}

#[test]
fn http_base_uses_ws() {
    let url = ws_url("http://localhost:8787", "abc", "sender", Some("t"), None);
    assert_eq!(
        url,
        "ws://localhost:8787/ws/relay/abc?role=sender&token=t"
    );
}

#[test]
fn neither_token_nor_pin_yields_role_only_query() {
    let url = ws_url("https://x", "abc", "sender", None, None);
    assert_eq!(url, "wss://x/ws/relay/abc?role=sender");
}

#[test]
fn both_token_and_pin_emit_both_params() {
    // Current behaviour: callers don't pass both today (sender uses token,
    // viewer uses pin), but the helper just appends each Some(_) — pin
    // last. Lock that down so a future refactor either preserves it or
    // updates this test deliberately.
    let url = ws_url("https://x", "abc", "sender", Some("t"), Some("p"));
    assert_eq!(url, "wss://x/ws/relay/abc?role=sender&token=t&pin=p");
}

#[test]
fn trailing_slash_on_base_currently_doubles_the_path_separator() {
    // The real caller in `main.rs` trims one trailing slash before passing
    // the base in, so we never see this in production. This test simply
    // documents *current* behaviour: `ws_url` itself does no trimming, so
    // a trailing slash on the base produces a `//ws/relay/...` path.
    // If a future change adds trimming inside ws_url, update this assertion.
    let url = ws_url("https://api.example.com/", "abc", "sender", Some("t"), None);
    assert_eq!(
        url,
        "wss://api.example.com//ws/relay/abc?role=sender&token=t"
    );
}
