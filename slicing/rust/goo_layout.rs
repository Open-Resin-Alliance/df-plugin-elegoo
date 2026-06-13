use super::goo_types::{GooPreparedLayer, GOO_CRLF, GOO_LAYER_MAGIC};

#[inline]
pub(super) fn push_u8(out: &mut Vec<u8>, val: u8) {
    out.push(val);
}

#[inline]
pub(super) fn push_u16_be(out: &mut Vec<u8>, val: u16) {
    out.extend_from_slice(&val.to_be_bytes());
}

#[inline]
pub(super) fn push_u32_be(out: &mut Vec<u8>, val: u32) {
    out.extend_from_slice(&val.to_be_bytes());
}

#[inline]
pub(super) fn push_f32_be(out: &mut Vec<u8>, val: f32) {
    out.extend_from_slice(&val.to_be_bytes());
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

    if run_len <= 15 {
        out.push((chunk_type << 6) | low_nibble);
    } else if run_len <= 4_095 {
        out.push((chunk_type << 6) | (1 << 4) | low_nibble);
        out.push((run_len >> 4) as u8);
    } else if run_len <= 1_048_575 {
        out.push((chunk_type << 6) | (2 << 4) | low_nibble);
        out.push(((run_len >> 12) & 0xFF) as u8);
        out.push(((run_len >> 4) & 0xFF) as u8);
    } else {
        out.push((chunk_type << 6) | (3 << 4) | low_nibble);
        out.push(((run_len >> 20) & 0xFF) as u8);
        out.push(((run_len >> 12) & 0xFF) as u8);
        out.push(((run_len >> 4) & 0xFF) as u8);
    }

    if chunk_type == 0x01 {
        out.push(color);
    }
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
}
