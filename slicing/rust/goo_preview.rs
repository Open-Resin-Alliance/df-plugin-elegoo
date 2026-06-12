use crate::engine::SlicerV3Error;
use base64::Engine as _;

const SMALL_W: usize = 116;
const SMALL_H: usize = 116;
const LARGE_W: usize = 290;
const LARGE_H: usize = 290;

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
    Ok(GooPreviewBlobs {
        small: blank_rgb565(SMALL_W, SMALL_H),
        large: blank_rgb565(LARGE_W, LARGE_H),
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

    let small_rgb = resize_and_blend_rgba_to_rgb_nearest(&cropped, cw, ch, SMALL_W, SMALL_H);
    let large_rgb = resize_and_blend_rgba_to_rgb_nearest(&cropped, cw, ch, LARGE_W, LARGE_H);

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

fn resize_and_blend_rgba_to_rgb_nearest(
    rgba: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Vec<u8> {
    if src_w == 0 || src_h == 0 {
        return vec![0u8; dst_w * dst_h * 3];
    }

    // Fit src into dst maintaining aspect ratio, center it on black background
    let scale = (dst_w as f32 / src_w as f32).min(dst_h as f32 / src_h as f32);
    let fit_w = ((src_w as f32 * scale).round() as usize).max(1);
    let fit_h = ((src_h as f32 * scale).round() as usize).max(1);
    let off_x = (dst_w - fit_w) / 2;
    let off_y = (dst_h - fit_h) / 2;

    let mut out = vec![0u8; dst_w * dst_h * 3];

    for dst_y in 0..fit_h {
        let src_y = ((dst_y as f32 / fit_h as f32) * src_h as f32) as usize;
        let src_y = src_y.min(src_h - 1);
        for dst_x in 0..fit_w {
            let src_x = ((dst_x as f32 / fit_w as f32) * src_w as f32) as usize;
            let src_x = src_x.min(src_w - 1);

            let si = (src_y * src_w + src_x) * 4;
            let r = rgba[si] as u32;
            let g = rgba[si + 1] as u32;
            let b = rgba[si + 2] as u32;
            let a = rgba[si + 3] as u32;

            // Blend over black background
            let r_out = ((r * a) / 255) as u8;
            let g_out = ((g * a) / 255) as u8;
            let b_out = ((b * a) / 255) as u8;

            let di = ((off_y + dst_y) * dst_w + (off_x + dst_x)) * 3;
            out[di] = r_out;
            out[di + 1] = g_out;
            out[di + 2] = b_out;
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

fn blank_rgb565(w: usize, h: usize) -> Vec<u8> {
    vec![0u8; w * h * 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_preview_sizes() {
        let blobs = build_goo_previews(None).unwrap();
        assert_eq!(blobs.small.len(), SMALL_W * SMALL_H * 2);
        assert_eq!(blobs.large.len(), LARGE_W * LARGE_H * 2);
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
