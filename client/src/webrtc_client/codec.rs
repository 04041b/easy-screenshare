use anyhow::Result;

/// Convert a BGRA frame (top-down, stride bytes per row) to I420 planar.
/// Returns (y, u, v) planes sized `width*height`, `width*height/4`, `width*height/4`.
/// Uses BT.601 limited-range coefficients.
pub fn bgra_to_i420(bgra: &[u8], width: u32, height: u32, stride: u32) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let w = width as usize;
    let h = height as usize;
    let s = stride as usize;
    if bgra.len() < s * h {
        anyhow::bail!("bgra buffer too small: {} < {}", bgra.len(), s * h);
    }
    if w % 2 != 0 || h % 2 != 0 {
        anyhow::bail!("width and height must be even for I420");
    }

    let mut y_plane = vec![0u8; w * h];
    let mut u_plane = vec![0u8; (w / 2) * (h / 2)];
    let mut v_plane = vec![0u8; (w / 2) * (h / 2)];

    // Y for every pixel
    for j in 0..h {
        let row = &bgra[j * s..j * s + w * 4];
        let y_row = &mut y_plane[j * w..(j + 1) * w];
        for i in 0..w {
            let b = row[i * 4] as i32;
            let g = row[i * 4 + 1] as i32;
            let r = row[i * 4 + 2] as i32;
            // BT.601 limited
            let y = (66 * r + 129 * g + 25 * b + 128) >> 8;
            y_row[i] = (y + 16).clamp(0, 255) as u8;
        }
    }
    // U/V for every 2x2 block
    for j in (0..h).step_by(2) {
        for i in (0..w).step_by(2) {
            let mut bs = 0i32;
            let mut gs = 0i32;
            let mut rs = 0i32;
            for dj in 0..2 {
                for di in 0..2 {
                    let off = (j + dj) * s + (i + di) * 4;
                    bs += bgra[off] as i32;
                    gs += bgra[off + 1] as i32;
                    rs += bgra[off + 2] as i32;
                }
            }
            let b = bs / 4;
            let g = gs / 4;
            let r = rs / 4;
            let u = (-38 * r - 74 * g + 112 * b + 128) >> 8;
            let v = (112 * r - 94 * g - 18 * b + 128) >> 8;
            let idx = (j / 2) * (w / 2) + (i / 2);
            u_plane[idx] = (u + 128).clamp(0, 255) as u8;
            v_plane[idx] = (v + 128).clamp(0, 255) as u8;
        }
    }

    Ok((y_plane, u_plane, v_plane))
}

/// Pack i420 planes into a single contiguous YUV420P buffer in Y/U/V order.
pub fn pack_i420(y: &[u8], u: &[u8], v: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(y.len() + u.len() + v.len());
    out.extend_from_slice(y);
    out.extend_from_slice(u);
    out.extend_from_slice(v);
    out
}
