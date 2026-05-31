//! Tests for the nearest-neighbour BGRA downscaler used by the Windows
//! capture path. The function itself is platform-agnostic; only its caller
//! is `#[cfg(target_os = "windows")]`. Keeping the tests platform-agnostic
//! means CI runs them on macOS too.

use screenshare::capture::video::downscale_bgra;

fn bgra(b: u8, g: u8, r: u8) -> [u8; 4] {
    [b, g, r, 255]
}

/// Build a 4x4 BGRA image: top-left pixel is `tl`, everything else is `rest`.
fn quadrant_4x4(tl: [u8; 4], rest: [u8; 4]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 * 4 * 4);
    for y in 0..4 {
        for x in 0..4 {
            let pix = if x == 0 && y == 0 { tl } else { rest };
            buf.extend_from_slice(&pix);
        }
    }
    buf
}

#[test]
fn downscale_picks_nearest_source_pixel() {
    // 4x4 → 2x2 nearest-neighbour with the formula
    //   sx = ((dx*2 + 1) * src_w) / (dst_w * 2)
    // produces source coordinates (1,1), (3,1), (1,3), (3,3) for the four
    // output positions. So only the bottom-right of each 2x2 block is sampled;
    // the top-left red pixel never lands in the output.
    let red = bgra(0, 0, 255);
    let blue = bgra(255, 0, 0);
    let src = quadrant_4x4(red, blue);
    let dst = downscale_bgra(&src, 4, 4, 2, 2);
    assert_eq!(dst.len(), 2 * 2 * 4);
    for chunk in dst.chunks_exact(4) {
        assert_eq!(chunk, &blue, "every output pixel should sample blue");
    }
}

#[test]
fn downscale_samples_top_left_when_block_is_uniform() {
    // Sanity check: when the source quadrant containing the sampled pixel is
    // uniformly red, we get red out. Use a 2x2 → 1x1 — sampled coord is (1,1).
    let red = bgra(0, 0, 255);
    let src: Vec<u8> = std::iter::repeat(red).take(4).flatten().collect();
    let dst = downscale_bgra(&src, 2, 2, 1, 1);
    assert_eq!(dst, red.to_vec());
}

#[test]
fn downscale_identity_shape_preserves_bytes() {
    let src: Vec<u8> = (0..(4 * 4 * 4)).map(|i| i as u8).collect();
    let dst = downscale_bgra(&src, 4, 4, 4, 4);
    assert_eq!(dst, src);
}

#[test]
fn downscale_output_length_is_dst_w_times_dst_h_times_four() {
    let src = vec![0u8; 8 * 8 * 4];
    let dst = downscale_bgra(&src, 8, 8, 3, 5);
    assert_eq!(dst.len(), 3 * 5 * 4);
}
