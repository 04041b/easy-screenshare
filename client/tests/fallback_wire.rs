//! Wire-format roundtrip tests for the relay fallback path.
//!
//! The browser viewer parses the same byte layout in
//! `backend/src/viewer_html.ts` (≈ line 332), so the header must remain
//! byte-for-byte compatible: `[stream(1) | flags(1) | ts_us_le(8) | payload]`.

use screenshare::fallback::{frame_to_bytes, parse_frame};
use screenshare::webrtc_client::sender::EncodedFrame;

fn roundtrip(frame: EncodedFrame) {
    let bytes = frame_to_bytes(&frame);
    let parsed = parse_frame(&bytes).expect("parse_frame returned None");
    assert_eq!(parsed.stream, frame.stream);
    assert_eq!(parsed.keyframe, frame.keyframe);
    assert_eq!(parsed.timestamp_us, frame.timestamp_us);
    assert_eq!(parsed.data, frame.data);
}

#[test]
fn roundtrip_video_keyframe() {
    roundtrip(EncodedFrame {
        stream: 0,
        keyframe: true,
        timestamp_us: 123_456_789,
        data: vec![0xde, 0xad, 0xbe, 0xef],
    });
}

#[test]
fn roundtrip_audio_delta() {
    roundtrip(EncodedFrame {
        stream: 1,
        keyframe: false,
        timestamp_us: 0,
        data: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
    });
}

#[test]
fn roundtrip_empty_payload() {
    roundtrip(EncodedFrame {
        stream: 0,
        keyframe: false,
        timestamp_us: 42,
        data: vec![],
    });
}

#[test]
fn roundtrip_large_payload() {
    // ~64 KiB — well past any small-buffer optimisation boundary.
    let data: Vec<u8> = (0..64 * 1024u32).map(|i| (i & 0xff) as u8).collect();
    roundtrip(EncodedFrame {
        stream: 0,
        keyframe: true,
        timestamp_us: u64::MAX / 2,
        data,
    });
}

#[test]
fn header_layout_matches_browser_parser() {
    // The browser viewer reads:
    //   stream = u8[0], flags = u8[1], ts = little-endian u64[2..10].
    // Pin that down here so a future refactor can't silently break the
    // browser side.
    let frame = EncodedFrame {
        stream: 0,
        keyframe: true,
        timestamp_us: 0x0123_4567_89AB_CDEF,
        data: vec![0xff],
    };
    let bytes = frame_to_bytes(&frame);
    assert_eq!(bytes[0], 0, "stream byte");
    assert_eq!(bytes[1], 1, "flags byte: bit 0 = keyframe");
    let ts_le: [u8; 8] = bytes[2..10].try_into().unwrap();
    assert_eq!(
        ts_le,
        0x0123_4567_89AB_CDEFu64.to_le_bytes(),
        "timestamp must be little-endian u64"
    );
    assert_eq!(&bytes[10..], &[0xff], "payload follows the 10-byte header");
}

#[test]
fn flags_non_keyframe_is_zero() {
    let frame = EncodedFrame {
        stream: 1,
        keyframe: false,
        timestamp_us: 0,
        data: vec![],
    };
    let bytes = frame_to_bytes(&frame);
    assert_eq!(bytes[1], 0);
}

#[test]
fn parse_frame_rejects_short_buffer() {
    let buf = [0u8; 9];
    assert!(parse_frame(&buf).is_none());
}

#[test]
fn parse_frame_accepts_exactly_ten_bytes() {
    // 10-byte buffer = header only, zero-length payload. Should parse.
    let buf = [0u8; 10];
    let f = parse_frame(&buf).expect("expected Some for 10-byte buffer");
    assert!(f.data.is_empty());
}
