use anyhow::Result;
use yuvutils_rs::{
    bgra_to_yuv420, BufferStoreMut, YuvConversionMode, YuvPlanarImageMut, YuvRange,
    YuvStandardMatrix,
};

/// Convert a BGRA frame (top-down, `stride` bytes per row) to packed I420 in
/// place, writing Y/U/V planes contiguously into `out` — the layout libvpx
/// `Encoder::encode` consumes.
///
/// Backed by `yuvutils-rs`, which dispatches to NEON on aarch64 and AVX2/SSE4
/// on x86_64. The previous hand-rolled scalar BT.601 loop was the single
/// biggest CPU cost in the encoder thread; this is ~5–10× faster on the
/// platforms we ship. BT.601 limited range matches what the old code emitted.
///
/// `out` is reused across frames — the caller owns the allocation, so per-frame
/// allocation cost is zero once it's sized.
pub fn bgra_to_i420_into(
    bgra: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    out: &mut Vec<u8>,
) -> Result<()> {
    if width % 2 != 0 || height % 2 != 0 {
        anyhow::bail!("width and height must be even for I420");
    }
    let w = width as usize;
    let h = height as usize;
    let s = stride as usize;
    if bgra.len() < s * h {
        anyhow::bail!("bgra buffer too small: {} < {}", bgra.len(), s * h);
    }

    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);
    let total = y_size + 2 * uv_size;
    if out.len() != total {
        out.resize(total, 0);
    }
    let (y_part, rest) = out.split_at_mut(y_size);
    let (u_part, v_part) = rest.split_at_mut(uv_size);

    let mut image = YuvPlanarImageMut {
        y_plane: BufferStoreMut::Borrowed(y_part),
        y_stride: width,
        u_plane: BufferStoreMut::Borrowed(u_part),
        u_stride: width / 2,
        v_plane: BufferStoreMut::Borrowed(v_part),
        v_stride: width / 2,
        width,
        height,
    };

    bgra_to_yuv420(
        &mut image,
        bgra,
        stride,
        YuvRange::Limited,
        YuvStandardMatrix::Bt601,
        YuvConversionMode::Balanced,
    )
    .map_err(|e| anyhow::anyhow!("bgra_to_yuv420: {e:?}"))?;

    Ok(())
}
