// GOO layout and container building — shared byte primitives, RLE packing,
// layer preparation, and the top-level dispatcher.

use crate::engine::SlicerV3Error;
use crate::types::SliceJobV3;

use super::goo_types::{GooPreparedLayer, GOO_CRLF, GOO_LAYER_MAGIC};

use super::goo_encoder::build_goo_container_bytes_with_progress as build_goo_v12_with_progress;

// === Shared utility functions ===

#[inline]
pub(super) fn push_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

#[inline]
pub(super) fn push_u16_be(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

#[inline]
pub(super) fn push_u32_be(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

#[inline]
pub(super) fn push_f32_be(out: &mut Vec<u8>, value: f32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Write a fixed-size string field: null-terminate and zero-pad to `len` bytes.
pub(super) fn push_str_fixed(out: &mut Vec<u8>, s: &str, len: usize) {
    let bytes = s.as_bytes();
    let copy_len = bytes.len().min(len.saturating_sub(1));
    out.extend_from_slice(&bytes[..copy_len]);
    for _ in 0..(len - copy_len) {
        out.push(0);
    }
}

#[inline]
pub(super) fn push_crlf(out: &mut Vec<u8>) {
    out.extend_from_slice(&GOO_CRLF);
}

// === Shared RLE encoding and layer handling ===

/// Encode a flat 8-bit grayscale pixel buffer using Goo RLE.
///
/// Output: `0x55` magic, variable-length chunks, one's-complement checksum byte.
///
/// First byte of each chunk = `[TT][SS][CCCC]`:
///   TT: 00=black(0x00), 01=gray(explicit color byte appended), 11=white(0xFF)
///   SS: stride size — 00=4-bit, 01=12-bit, 10=20-bit, 11=28-bit count
///   CCCC: low 4 bits of count
///
/// For SS=01: count = next_byte<<4 | CCCC
/// For SS=10: count = b1<<12 | b2<<4 | CCCC
/// For SS=11: count = b1<<20 | b2<<12 | b3<<4 | CCCC
pub(super) fn goo_rle_encode(pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len() / 4 + 16);
    out.push(GOO_LAYER_MAGIC);

    let mut i = 0;
    while i < pixels.len() {
        let color = pixels[i];
        let run_start = i;
        i += 1;
        while i < pixels.len() && pixels[i] == color {
            i += 1;
        }
        let run_len = (i - run_start) as u32;
        let chunk_type: u8 = match color {
            0x00 => 0x00,
            0xFF => 0x03,
            _ => 0x01,
        };
        push_goo_run(&mut out, chunk_type, run_len, color);
    }

    // One's-complement checksum of all bytes after the magic byte.
    let sum: u8 = out[1..].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    out.push(!sum);
    out
}

pub(super) fn push_goo_run(out: &mut Vec<u8>, chunk_type: u8, run_len: u32, color: u8) {
    let low_nibble = (run_len & 0xF) as u8;
    let stride_bits: u8 = if run_len <= 0xF {
        0
    } else if run_len <= 0xFFF {
        1
    } else if run_len <= 0xF_FFFF {
        2
    } else {
        3
    };
    out.push((chunk_type << 6) | (stride_bits << 4) | low_nibble);

    // Gray chunks carry the color byte immediately after the first chunk
    // byte, BEFORE any extended run-length bytes (UVtools GooFile AddRep).
    if chunk_type == 0x01 {
        out.push(color);
    }

    match stride_bits {
        1 => {
            out.push((run_len >> 4) as u8);
        }
        2 => {
            out.push(((run_len >> 12) & 0xFF) as u8);
            out.push(((run_len >> 4) & 0xFF) as u8);
        }
        3 => {
            out.push(((run_len >> 20) & 0xFF) as u8);
            out.push(((run_len >> 12) & 0xFF) as u8);
            out.push(((run_len >> 4) & 0xFF) as u8);
        }
        _ => {}
    }
}

/// Encode rasterizer RLE runs directly into a Goo layer payload
/// (magic + chunks + checksum), skipping the full pixel buffer.
pub(super) fn encode_goo_rle_from_runs(
    runs: &[crate::rle::RleRun],
    is_anti_aliased: bool,
    threshold: u8,
    total_pixels: usize,
) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(runs.len() * 3 + 16);
    encoded.push(GOO_LAYER_MAGIC);

    if runs.is_empty() {
        // All-black layer: single zero run.
        push_goo_run(&mut encoded, 0x00, total_pixels.min(u32::MAX as usize) as u32, 0x00);
    } else {
        for run in runs {
            let value = if is_anti_aliased {
                run.value
            } else if run.value > threshold {
                0xFF
            } else {
                0x00
            };
            let chunk_type = match value {
                0x00 => 0x00,
                0xFF => 0x03,
                _ => 0x01,
            };
            push_goo_run(&mut encoded, chunk_type, run.length, value);
        }
    }

    let sum: u8 = encoded[1..].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    encoded.push(!sum);
    encoded
}

pub(super) fn encode_single_goo_layer_from_raw_mask(
    layer_index: usize,
    raw_mask: &[u8],
    is_anti_aliased: bool,
    threshold: u8,
    layer_height_mm: f32,
    bottom_layer_count: u32,
) -> GooPreparedLayer {
    let pixels: Vec<u8> = if is_anti_aliased {
        raw_mask.to_vec()
    } else {
        raw_mask
            .iter()
            .map(|&p| if p > threshold { 0xFF } else { 0x00 })
            .collect()
    };
    GooPreparedLayer {
        index: layer_index,
        position_z_mm: (layer_index as f32 + 1.0) * layer_height_mm,
        is_bottom: (layer_index as u32) < bottom_layer_count,
        encoded: goo_rle_encode(&pixels),
    }
}

pub(super) fn encode_single_goo_empty_layer(
    layer_index: usize,
    expected_pixels: usize,
    layer_height_mm: f32,
    bottom_layer_count: u32,
) -> GooPreparedLayer {
    GooPreparedLayer {
        index: layer_index,
        position_z_mm: (layer_index as f32 + 1.0) * layer_height_mm,
        is_bottom: (layer_index as u32) < bottom_layer_count,
        encoded: encode_goo_rle_from_runs(&[], false, 0, expected_pixels),
    }
}

pub(super) fn prepare_layers_for_goo_with_progress(
    raw_masks: &[Vec<u8>],
    is_anti_aliased: bool,
    threshold: u8,
    layer_height_mm: f32,
    bottom_layer_count: u32,
    on_progress: Option<&dyn Fn(u32, u32)>,
) -> Vec<GooPreparedLayer> {
    let total = raw_masks.len() as u32;
    let mut out = Vec::with_capacity(raw_masks.len());

    for (index, layer) in raw_masks.iter().enumerate() {
        out.push(encode_single_goo_layer_from_raw_mask(
            index,
            layer,
            is_anti_aliased,
            threshold,
            layer_height_mm,
            bottom_layer_count,
        ));

        if let Some(progress) = on_progress {
            progress((index as u32) + 1, total.max(1));
        }
    }

    out
}

// === Public Dispatcher ===
// GOO V1.2 is the only variant today; a future variant (e.g. the
// little-endian V5.x layout) would branch here, mirroring the CTB
// v5/v5enc dispatch in ctb_layout.rs.

pub(super) fn build_goo_container_bytes(
    job: &SliceJobV3,
    prepared: &[GooPreparedLayer],
) -> Result<Vec<u8>, SlicerV3Error> {
    build_goo_container_bytes_with_progress(job, prepared, None)
}

pub(super) fn build_goo_container_bytes_with_progress(
    job: &SliceJobV3,
    prepared: &[GooPreparedLayer],
    on_progress: Option<&dyn Fn(u32, u32)>,
) -> Result<Vec<u8>, SlicerV3Error> {
    build_goo_v12_with_progress(job, prepared, on_progress)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle_starts_with_magic() {
        let out = goo_rle_encode(&[0u8; 4]);
        assert_eq!(out[0], GOO_LAYER_MAGIC);
    }

    #[test]
    fn rle_black_run_4bit() {
        // 15 black pixels → 1 chunk byte + checksum
        let out = goo_rle_encode(&[0u8; 15]);
        // first byte: TT=00, SS=00, CCCC=1111 = 0x0F
        assert_eq!(out[1], 0x0F);
    }

    #[test]
    fn rle_white_run_4bit() {
        // 1 white pixel → TT=11, SS=00, CCCC=0001 = 0xC1
        let out = goo_rle_encode(&[0xFF; 1]);
        assert_eq!(out[1], 0xC1);
    }

    #[test]
    fn rle_gray_run_emits_color_byte() {
        // 1 gray pixel (value=128) → TT=01, SS=00, CCCC=0001 = 0x41, then 0x80
        let out = goo_rle_encode(&[0x80; 1]);
        assert_eq!(out[1], 0x41);
        assert_eq!(out[2], 0x80);
    }

    #[test]
    fn rle_gray_long_run_places_color_before_length_bytes() {
        // 256 gray pixels → TT=01, SS=01, CCCC=0 = 0x50, color byte, then
        // the extended length byte (256 >> 4 = 16). UVtools reads the gray
        // byte immediately after the chunk byte.
        let out = goo_rle_encode(&[0x80; 256]);
        assert_eq!(out[1], 0x50);
        assert_eq!(out[2], 0x80);
        assert_eq!(out[3], 16);
    }

    #[test]
    fn rle_12bit_stride() {
        // 256 black pixels → needs 12-bit stride
        let out = goo_rle_encode(&[0u8; 256]);
        // TT=00, SS=01, CCCC=(256 & 0xF)=0 → first byte = 0x10
        assert_eq!(out[1], 0x10);
        // next_byte = 256 >> 4 = 16
        assert_eq!(out[2], 16);
    }

    #[test]
    fn rle_checksum_is_ones_complement() {
        let pixels = vec![0u8; 4];
        let out = goo_rle_encode(&pixels);
        let last = *out.last().unwrap();
        let sum: u8 = out[1..out.len() - 1]
            .iter()
            .fold(0u8, |a, &b| a.wrapping_add(b));
        assert_eq!(last, !sum);
    }

    #[test]
    fn rle_roundtrip_empty_produces_magic_and_checksum() {
        let out = goo_rle_encode(&[]);
        // Just magic + checksum (checksum of empty = !0 = 0xFF)
        assert_eq!(out, vec![GOO_LAYER_MAGIC, 0xFF]);
    }

    #[test]
    fn empty_layer_encodes_single_black_run() {
        let layer = encode_single_goo_empty_layer(0, 256, 0.05, 1);
        assert!(layer.is_bottom);
        // magic, TT=00 SS=01 chunk (0x10), ext byte 16, checksum
        assert_eq!(layer.encoded[0], GOO_LAYER_MAGIC);
        assert_eq!(layer.encoded[1], 0x10);
        assert_eq!(layer.encoded[2], 16);
    }
}
