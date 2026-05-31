//! Integration tests for the BGRA→I420 converter.
//!
//! Values target BT.601 limited-range coefficients, the same matrix
//! `bgra_to_i420_into` asks `yuvutils-rs` for. A ±2 tolerance covers
//! integer-rounding drift between the canonical real-valued formulae and the
//! SIMD integer pipeline (NEON / AVX2 / SSE4 dispatch under the hood).

use screenshare::webrtc_client::codec::bgra_to_i420_into;

/// Build a width×height BGRA buffer filled with a single (b, g, r, a) pixel.
fn solid(width: u32, height: u32, b: u8, g: u8, r: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..(width * height) {
        buf.extend_from_slice(&[b, g, r, 255]);
    }
    buf
}

/// Same as [`solid`] but with a per-row padding (extra zero bytes appended
/// after each row) so the stride is `width * 4 + extra_per_row`.
fn solid_padded(width: u32, height: u32, b: u8, g: u8, r: u8, extra_per_row: usize) -> Vec<u8> {
    let row_bytes = (width * 4) as usize;
    let stride = row_bytes + extra_per_row;
    let mut buf = vec![0u8; stride * height as usize];
    for y in 0..height as usize {
        let row_start = y * stride;
        for x in 0..width as usize {
            let off = row_start + x * 4;
            buf[off] = b;
            buf[off + 1] = g;
            buf[off + 2] = r;
            buf[off + 3] = 255;
        }
    }
    buf
}

fn assert_near(actual: u8, expected: u8, label: &str) {
    let diff = (actual as i32 - expected as i32).abs();
    assert!(
        diff <= 2,
        "{label}: expected {expected}±2, got {actual} (diff {diff})"
    );
}

/// Run a conversion and return the (packed) buffer plus its plane slices.
fn convert(bgra: &[u8], w: u32, h: u32, stride: u32) -> (Vec<u8>, std::ops::Range<usize>, std::ops::Range<usize>, std::ops::Range<usize>) {
    let mut out = Vec::new();
    bgra_to_i420_into(bgra, w, h, stride, &mut out).unwrap();
    let y_end = (w * h) as usize;
    let uv_size = ((w / 2) * (h / 2)) as usize;
    (out, 0..y_end, y_end..y_end + uv_size, y_end + uv_size..y_end + 2 * uv_size)
}

#[test]
fn bgra_white_maps_to_bt601_limited_white() {
    let (buf, y, u, v) = convert(&solid(2, 2, 255, 255, 255), 2, 2, 8);
    assert_near(buf[y.start], 235, "Y(white)");
    assert_near(buf[u.start], 128, "U(white)");
    assert_near(buf[v.start], 128, "V(white)");
}

#[test]
fn bgra_black_maps_to_bt601_limited_black() {
    let (buf, y, u, v) = convert(&solid(2, 2, 0, 0, 0), 2, 2, 8);
    assert_near(buf[y.start], 16, "Y(black)");
    assert_near(buf[u.start], 128, "U(black)");
    assert_near(buf[v.start], 128, "V(black)");
}

#[test]
fn bgra_pure_red() {
    let (buf, y, u, v) = convert(&solid(2, 2, 0, 0, 255), 2, 2, 8);
    assert_near(buf[y.start], 81, "Y(red)");
    assert_near(buf[u.start], 90, "U(red)");
    assert_near(buf[v.start], 240, "V(red)");
}

#[test]
fn bgra_pure_green() {
    let (buf, y, u, v) = convert(&solid(2, 2, 0, 255, 0), 2, 2, 8);
    assert_near(buf[y.start], 145, "Y(green)");
    assert_near(buf[u.start], 54, "U(green)");
    assert_near(buf[v.start], 34, "V(green)");
}

#[test]
fn bgra_pure_blue() {
    let (buf, y, u, v) = convert(&solid(2, 2, 255, 0, 0), 2, 2, 8);
    assert_near(buf[y.start], 41, "Y(blue)");
    assert_near(buf[u.start], 240, "U(blue)");
    assert_near(buf[v.start], 110, "V(blue)");
}

#[test]
fn bgra_packed_buffer_sizes() {
    let w = 8u32;
    let h = 6u32;
    let (buf, y, u, v) = convert(&solid(w, h, 12, 34, 56), w, h, w * 4);
    let y_size = (w * h) as usize;
    let uv_size = ((w / 2) * (h / 2)) as usize;
    assert_eq!(y.len(), y_size);
    assert_eq!(u.len(), uv_size);
    assert_eq!(v.len(), uv_size);
    assert_eq!(buf.len(), y_size + 2 * uv_size);
}

#[test]
fn bgra_stride_padding_matches_unpadded() {
    // Two 4x4 solid-red images: one tight, one with 8 extra bytes per row.
    let tight = solid(4, 4, 0, 0, 255);
    let padded = solid_padded(4, 4, 0, 0, 255, 8);
    let (buf1, _, _, _) = convert(&tight, 4, 4, 16);
    let (buf2, _, _, _) = convert(&padded, 4, 4, 24);
    assert_eq!(buf1, buf2, "Packed I420 should be identical regardless of source stride");
}

#[test]
fn bgra_odd_dimensions_error() {
    // 3x2 has odd width — I420 sub-sampling requires even dimensions.
    let buf = vec![0u8; 3 * 2 * 4];
    let mut out = Vec::new();
    let err = bgra_to_i420_into(&buf, 3, 2, 12, &mut out).unwrap_err();
    assert!(
        err.to_string().contains("even"),
        "expected even-dimension error, got: {err}"
    );
}

#[test]
fn bgra_too_small_buffer_error() {
    // Claim 4x4 with stride=16 (=> 64 bytes needed) but supply only 32.
    let buf = vec![0u8; 32];
    let mut out = Vec::new();
    let err = bgra_to_i420_into(&buf, 4, 4, 16, &mut out).unwrap_err();
    assert!(
        err.to_string().contains("too small"),
        "expected too-small error, got: {err}"
    );
}

#[test]
fn out_buffer_is_reused_in_place() {
    // The hot-path contract: callers reuse `out` across frames so per-frame
    // allocation cost is zero once it's sized. Calling twice in a row should
    // leave `out` exactly the right size (resize on first call, untouched on
    // second).
    let w = 4u32;
    let h = 4u32;
    let bgra = solid(w, h, 0, 0, 255);
    let expected_len = ((w * h) + 2 * (w / 2) * (h / 2)) as usize;

    let mut out = Vec::new();
    bgra_to_i420_into(&bgra, w, h, w * 4, &mut out).unwrap();
    assert_eq!(out.len(), expected_len);

    let cap_after_first = out.capacity();
    bgra_to_i420_into(&bgra, w, h, w * 4, &mut out).unwrap();
    assert_eq!(out.len(), expected_len);
    assert_eq!(out.capacity(), cap_after_first, "second call should not realloc");
}
