use crate::engine::SlicerV3Error;
use base64::Engine as _;

const SMALL_W: usize = 116;
const SMALL_H: usize = 116;
const LARGE_W: usize = 290;
const LARGE_H: usize = 290;

// Actual pixel dimensions the printer renders previews at.
// The printer stretches stored pixels to fill these, so we pre-compensate.
const SMALL_DISPLAY_W: usize = 200;
const SMALL_DISPLAY_H: usize = 125;
const LARGE_DISPLAY_W: usize = 400;
const LARGE_DISPLAY_H: usize = 300;

// Background gradient: dark dragonfruit purple → dark dragonfruit green (diagonal).
// Matches CTB preview gradient so both formats share a consistent look.
const GRADIENT_START: [u32; 3] = [32, 10, 42];
const GRADIENT_END: [u32; 3] = [14, 34, 14];
const BAYER4X4: [[i32; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

pub(super) struct GooPreviewBlobs {
    pub small: Vec<u8>,
    pub large: Vec<u8>,
}

pub(super) fn build_goo_previews(
    thumbnail_png_base64: Option<&str>,
) -> Result<GooPreviewBlobs, SlicerV3Error> {
    if let Some(b64) = thumbnail_png_base64 {
        if !b64.is_empty() {
            match decode_and_build(b64) {
                Ok(blobs) => return Ok(blobs),
                Err(_) => {}
            }
        }
    }
    // No thumbnail — render gradient-only background using a 1×1 transparent pixel.
    let transparent = [0u8, 0, 0, 0];
    Ok(GooPreviewBlobs {
        small: rgb_to_rgb565_be(
            &render_for_display_compensated(&transparent, 1, 1, SMALL_W, SMALL_H, SMALL_DISPLAY_W, SMALL_DISPLAY_H),
            SMALL_W, SMALL_H,
        ),
        large: rgb_to_rgb565_be(
            &render_for_display_compensated(&transparent, 1, 1, LARGE_W, LARGE_H, LARGE_DISPLAY_W, LARGE_DISPLAY_H),
            LARGE_W, LARGE_H,
        ),
    })
}

fn decode_and_build(b64: &str) -> Result<GooPreviewBlobs, SlicerV3Error> {
    let png_bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| SlicerV3Error::UnsupportedOutput(format!("preview base64 decode: {e}")))?;

    let decoder = png::Decoder::new(std::io::Cursor::new(&png_bytes));
    let mut reader = decoder
        .read_info()
        .map_err(|e| SlicerV3Error::UnsupportedOutput(format!("preview PNG read_info: {e}")))?;

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| SlicerV3Error::UnsupportedOutput(format!("preview PNG decode: {e}")))?;

    let raw = &buf[..info.buffer_size()];
    let src_w = info.width as usize;
    let src_h = info.height as usize;

    let rgba = to_rgba(raw, src_w, src_h, info.color_type, info.bit_depth)?;

    let (cropped, cw, ch) = crop_transparent_border(&rgba, src_w, src_h);

    let small_rgb = render_for_display_compensated(&cropped, cw, ch, SMALL_W, SMALL_H, SMALL_DISPLAY_W, SMALL_DISPLAY_H);
    let large_rgb = render_for_display_compensated(&cropped, cw, ch, LARGE_W, LARGE_H, LARGE_DISPLAY_W, LARGE_DISPLAY_H);

    Ok(GooPreviewBlobs {
        small: rgb_to_rgb565_be(&small_rgb, SMALL_W, SMALL_H),
        large: rgb_to_rgb565_be(&large_rgb, LARGE_W, LARGE_H),
    })
}

fn to_rgba(
    raw: &[u8],
    w: usize,
    h: usize,
    color_type: png::ColorType,
    bit_depth: png::BitDepth,
) -> Result<Vec<u8>, SlicerV3Error> {
    let pixels = w * h;
    let is_16bit = bit_depth == png::BitDepth::Sixteen;

    let mut rgba = vec![0u8; pixels * 4];

    match color_type {
        png::ColorType::Rgba => {
            if is_16bit {
                for (i, chunk) in raw.chunks_exact(8).enumerate() {
                    rgba[i * 4] = chunk[0];
                    rgba[i * 4 + 1] = chunk[2];
                    rgba[i * 4 + 2] = chunk[4];
                    rgba[i * 4 + 3] = chunk[6];
                }
            } else {
                rgba.copy_from_slice(&raw[..pixels * 4]);
            }
        }
        png::ColorType::Rgb => {
            if is_16bit {
                for (i, chunk) in raw.chunks_exact(6).enumerate() {
                    rgba[i * 4] = chunk[0];
                    rgba[i * 4 + 1] = chunk[2];
                    rgba[i * 4 + 2] = chunk[4];
                    rgba[i * 4 + 3] = 255;
                }
            } else {
                for (i, chunk) in raw.chunks_exact(3).enumerate() {
                    rgba[i * 4] = chunk[0];
                    rgba[i * 4 + 1] = chunk[1];
                    rgba[i * 4 + 2] = chunk[2];
                    rgba[i * 4 + 3] = 255;
                }
            }
        }
        png::ColorType::GrayscaleAlpha => {
            if is_16bit {
                for (i, chunk) in raw.chunks_exact(4).enumerate() {
                    let v = chunk[0];
                    rgba[i * 4] = v;
                    rgba[i * 4 + 1] = v;
                    rgba[i * 4 + 2] = v;
                    rgba[i * 4 + 3] = chunk[2];
                }
            } else {
                for (i, chunk) in raw.chunks_exact(2).enumerate() {
                    let v = chunk[0];
                    rgba[i * 4] = v;
                    rgba[i * 4 + 1] = v;
                    rgba[i * 4 + 2] = v;
                    rgba[i * 4 + 3] = chunk[1];
                }
            }
        }
        png::ColorType::Grayscale => {
            let stride = if is_16bit { 2 } else { 1 };
            for (i, chunk) in raw.chunks_exact(stride).enumerate() {
                let v = chunk[0];
                rgba[i * 4] = v;
                rgba[i * 4 + 1] = v;
                rgba[i * 4 + 2] = v;
                rgba[i * 4 + 3] = 255;
            }
        }
        _ => {
            return Err(SlicerV3Error::UnsupportedOutput(format!(
                "unsupported PNG color type for Goo preview: {:?}",
                color_type
            )));
        }
    }

    Ok(rgba)
}

fn crop_transparent_border(rgba: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let mut min_x = w;
    let mut max_x = 0usize;
    let mut min_y = h;
    let mut max_y = 0usize;

    for y in 0..h {
        for x in 0..w {
            let a = rgba[(y * w + x) * 4 + 3];
            if a > 0 {
                if x < min_x { min_x = x; }
                if x > max_x { max_x = x; }
                if y < min_y { min_y = y; }
                if y > max_y { max_y = y; }
            }
        }
    }

    if min_x > max_x || min_y > max_y {
        // Fully transparent — return a 1×1 black pixel
        return (vec![0, 0, 0, 255], 1, 1);
    }

    let cw = max_x - min_x + 1;
    let ch = max_y - min_y + 1;
    let mut out = vec![0u8; cw * ch * 4];
    for y in 0..ch {
        let src_row = (min_y + y) * w + min_x;
        let dst_row = y * cw;
        out[dst_row * 4..(dst_row + cw) * 4]
            .copy_from_slice(&rgba[src_row * 4..(src_row + cw) * 4]);
    }
    (out, cw, ch)
}

/// Render `src` into a `dst_w × dst_h` stored image with display-stretch compensation.
///
/// The gradient and dithering are computed in display space (before stretch compensation),
/// so they appear undistorted on the printer. Each stored pixel maps through the printer's
/// display canvas first, then back to source coords — pre-distorting the image inversely.
fn render_for_display_compensated(
    rgba: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    display_w: usize,
    display_h: usize,
) -> Vec<u8> {
    // How the source fits (letterboxed/pillarboxed) inside the display canvas.
    let scale = if src_w == 0 || src_h == 0 {
        0.0f32
    } else {
        (display_w as f32 / src_w as f32).min(display_h as f32 / src_h as f32)
    };
    let fit_w = src_w as f32 * scale;
    let fit_h = src_h as f32 * scale;
    let off_x = (display_w as f32 - fit_w) / 2.0;
    let off_y = (display_h as f32 - fit_h) / 2.0;

    let denom = (display_w + display_h).max(1) as u64;
    let dither_u8 = |v: u32, d: i32| -> u8 {
        ((v as i32 + (d - 8) * 3 / 8).clamp(0, 255)) as u8
    };

    let mut out = vec![0u8; dst_w * dst_h * 3];

    for dst_y in 0..dst_h {
        let disp_y = (dst_y as f32 + 0.5) * display_h as f32 / dst_h as f32;
        for dst_x in 0..dst_w {
            let disp_x = (dst_x as f32 + 0.5) * display_w as f32 / dst_w as f32;
            let dither = BAYER4X4[dst_y & 3][dst_x & 3];

            // Gradient in display space — matches CTB: diagonal purple→green
            let t = ((disp_x as u64 + disp_y as u64) * 255 / denom) as u32;
            let bg_r = (GRADIENT_START[0] * (255 - t) + GRADIENT_END[0] * t) / 255;
            let bg_g = (GRADIENT_START[1] * (255 - t) + GRADIENT_END[1] * t) / 255;
            let bg_b = (GRADIENT_START[2] * (255 - t) + GRADIENT_END[2] * t) / 255;

            let di = (dst_y * dst_w + dst_x) * 3;

            if scale == 0.0 || disp_x < off_x || disp_x >= off_x + fit_w
                || disp_y < off_y || disp_y >= off_y + fit_h
            {
                // Letterbox / pillarbox — pure gradient
                out[di]     = dither_u8(bg_r, dither);
                out[di + 1] = dither_u8(bg_g, dither);
                out[di + 2] = dither_u8(bg_b, dither);
                continue;
            }

            let src_x = (((disp_x - off_x) / fit_w) * src_w as f32) as usize;
            let src_y = (((disp_y - off_y) / fit_h) * src_h as f32) as usize;
            let src_x = src_x.min(src_w - 1);
            let src_y = src_y.min(src_h - 1);

            let si = (src_y * src_w + src_x) * 4;
            let r = rgba[si] as u32;
            let g = rgba[si + 1] as u32;
            let b = rgba[si + 2] as u32;
            let a = rgba[si + 3] as u32;

            if a == 255 {
                out[di]     = r as u8;
                out[di + 1] = g as u8;
                out[di + 2] = b as u8;
            } else if a == 0 {
                out[di]     = dither_u8(bg_r, dither);
                out[di + 1] = dither_u8(bg_g, dither);
                out[di + 2] = dither_u8(bg_b, dither);
            } else {
                let inv_a = 255 - a;
                out[di]     = dither_u8((r * a + bg_r * inv_a) / 255, dither);
                out[di + 1] = dither_u8((g * a + bg_g * inv_a) / 255, dither);
                out[di + 2] = dither_u8((b * a + bg_b * inv_a) / 255, dither);
            }
        }
    }

    out
}

fn rgb_to_rgb565_be(rgb: &[u8], w: usize, h: usize) -> Vec<u8> {
    let pixel_count = w * h;
    let mut out = Vec::with_capacity(pixel_count * 2);
    for chunk in rgb[..pixel_count * 3].chunks_exact(3) {
        let r = (chunk[0] as u16) >> 3;
        let g = (chunk[1] as u16) >> 2;
        let b = (chunk[2] as u16) >> 3;
        let pixel: u16 = (r << 11) | (g << 5) | b;
        out.extend_from_slice(&pixel.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_thumbnail_produces_correct_sizes() {
        let blobs = build_goo_previews(None).unwrap();
        assert_eq!(blobs.small.len(), SMALL_W * SMALL_H * 2);
        assert_eq!(blobs.large.len(), LARGE_W * LARGE_H * 2);
    }

    #[test]
    fn no_thumbnail_renders_gradient_not_black() {
        let blobs = build_goo_previews(None).unwrap();
        // Gradient background means not all pixels are zero.
        assert!(blobs.small.iter().any(|&b| b != 0));
    }

    #[test]
    fn gradient_top_left_differs_from_bottom_right() {
        // Diagonal gradient must vary across the display canvas.
        let rgb = render_for_display_compensated(
            &[0, 0, 0, 0], 1, 1,
            SMALL_W, SMALL_H, SMALL_DISPLAY_W, SMALL_DISPLAY_H,
        );
        let top_left = [rgb[0], rgb[1], rgb[2]];
        let last = (SMALL_W * SMALL_H - 1) * 3;
        let bottom_right = [rgb[last], rgb[last + 1], rgb[last + 2]];
        assert_ne!(top_left, bottom_right);
    }

    #[test]
    fn rgb565_black() {
        let rgb = vec![0u8; 3];
        let out = rgb_to_rgb565_be(&rgb, 1, 1);
        assert_eq!(out, vec![0, 0]);
    }

    #[test]
    fn rgb565_white() {
        let rgb = vec![0xFF, 0xFF, 0xFF];
        let out = rgb_to_rgb565_be(&rgb, 1, 1);
        assert_eq!(out, vec![0xFF, 0xFF]);
    }
}
