//! Integration tests for the BGRA→I420 converter and the I420 packer.
//!
//! Values target BT.601 limited-range coefficients, the same formulae used
//! in `bgra_to_i420`. A ±2 tolerance covers integer-rounding drift between
//! the canonical real-valued formulae and the integer pipeline.

use screenshare::webrtc_client::codec::{bgra_to_i420, pack_i420};

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

#[test]
fn bgra_white_maps_to_bt601_limited_white() {
    let (y, u, v) = bgra_to_i420(&solid(2, 2, 255, 255, 255), 2, 2, 8).unwrap();
    assert_near(y[0], 235, "Y(white)");
    assert_near(u[0], 128, "U(white)");
    assert_near(v[0], 128, "V(white)");
}

#[test]
fn bgra_black_maps_to_bt601_limited_black() {
    let (y, u, v) = bgra_to_i420(&solid(2, 2, 0, 0, 0), 2, 2, 8).unwrap();
    assert_near(y[0], 16, "Y(black)");
    assert_near(u[0], 128, "U(black)");
    assert_near(v[0], 128, "V(black)");
}

#[test]
fn bgra_pure_red() {
    let (y, u, v) = bgra_to_i420(&solid(2, 2, 0, 0, 255), 2, 2, 8).unwrap();
    assert_near(y[0], 81, "Y(red)");
    assert_near(u[0], 90, "U(red)");
    assert_near(v[0], 240, "V(red)");
}

#[test]
fn bgra_pure_green() {
    let (y, u, v) = bgra_to_i420(&solid(2, 2, 0, 255, 0), 2, 2, 8).unwrap();
    assert_near(y[0], 145, "Y(green)");
    assert_near(u[0], 54, "U(green)");
    assert_near(v[0], 34, "V(green)");
}

#[test]
fn bgra_pure_blue() {
    let (y, u, v) = bgra_to_i420(&solid(2, 2, 255, 0, 0), 2, 2, 8).unwrap();
    assert_near(y[0], 41, "Y(blue)");
    assert_near(u[0], 240, "U(blue)");
    assert_near(v[0], 110, "V(blue)");
}

#[test]
fn bgra_plane_sizes() {
    let w = 8u32;
    let h = 6u32;
    let (y, u, v) = bgra_to_i420(&solid(w, h, 12, 34, 56), w, h, w * 4).unwrap();
    assert_eq!(y.len(), (w * h) as usize);
    assert_eq!(u.len(), ((w / 2) * (h / 2)) as usize);
    assert_eq!(v.len(), ((w / 2) * (h / 2)) as usize);
}

#[test]
fn bgra_stride_padding_matches_unpadded() {
    // Two 4x4 solid-red images: one tight, one with 8 extra bytes per row.
    let tight = solid(4, 4, 0, 0, 255);
    let padded = solid_padded(4, 4, 0, 0, 255, 8);
    let (y1, u1, v1) = bgra_to_i420(&tight, 4, 4, 16).unwrap();
    let (y2, u2, v2) = bgra_to_i420(&padded, 4, 4, 24).unwrap();
    assert_eq!(y1, y2, "Y plane should be identical regardless of stride");
    assert_eq!(u1, u2, "U plane should be identical regardless of stride");
    assert_eq!(v1, v2, "V plane should be identical regardless of stride");
}

#[test]
fn bgra_odd_dimensions_error() {
    // 3x2 has odd width — I420 sub-sampling requires even dimensions.
    let buf = vec![0u8; 3 * 2 * 4];
    let err = bgra_to_i420(&buf, 3, 2, 12).unwrap_err();
    assert!(
        err.to_string().contains("even"),
        "expected even-dimension error, got: {err}"
    );
}

#[test]
fn bgra_too_small_buffer_error() {
    // Claim 4x4 with stride=16 (=> 64 bytes needed) but supply only 32.
    let buf = vec![0u8; 32];
    let err = bgra_to_i420(&buf, 4, 4, 16).unwrap_err();
    assert!(
        err.to_string().contains("too small"),
        "expected too-small error, got: {err}"
    );
}

#[test]
fn pack_i420_length_is_sum() {
    let y = vec![1u8; 16];
    let u = vec![2u8; 4];
    let v = vec![3u8; 4];
    let packed = pack_i420(&y, &u, &v);
    assert_eq!(packed.len(), y.len() + u.len() + v.len());
}

#[test]
fn pack_i420_order_is_y_then_u_then_v() {
    let y = vec![0xAA; 4];
    let u = vec![0xBB; 2];
    let v = vec![0xCC; 2];
    let packed = pack_i420(&y, &u, &v);
    assert_eq!(&packed[..4], &y[..]);
    assert_eq!(&packed[4..6], &u[..]);
    assert_eq!(&packed[6..8], &v[..]);
}
