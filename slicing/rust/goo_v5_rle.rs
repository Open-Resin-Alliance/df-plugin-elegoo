// GOO V5.1 per-panel RLE grammars + little-endian byte primitives.
//
// V5.1 stores each layer as PartitionCount half-screen panels, each framed
// `0x55 <chunk stream> <ones-complement checksum> 0D 0A`. Two grammars exist
// (spec §7.2 binary / §7.7 VUF); the real firmware question of which one the
// printer requires is still open, so both are implemented and selectable.
// Reference: /root/GOO_v5_Format_Spec.md, cross-validated by UVtools GooV5File.

use crate::rle::RleRun;

use super::goo_types::{GooV5RleMode, GooV5Settings, GOO_CRLF, GOO_LAYER_MAGIC};

/// Binary-grammar value code for a white pixel (renders white; not luminance).
pub(super) const GOO_V5_CODE_WHITE: u8 = 0x99;
/// Binary-grammar value code for a black pixel.
pub(super) const GOO_V5_CODE_BLACK: u8 = 0xA7;

// === Little-endian byte primitives (V1.2 is big-endian; see goo_layout.rs) ===

#[inline]
pub(super) fn push_u16_le(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[inline]
pub(super) fn push_u32_le(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[inline]
pub(super) fn push_f32_le(out: &mut Vec<u8>, value: f32) {
    out.extend_from_slice(&value.to_le_bytes());
}

// === Shared block framing ===

/// Wrap a chunk-stream body into the V5 block frame:
/// `0x55 <body> <checksum = ~sum(body)> 0D 0A`.
fn frame_block(body: Vec<u8>) -> Vec<u8> {
    let sum: u8 = body.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(GOO_LAYER_MAGIC);
    out.extend_from_slice(&body);
    out.push(!sum);
    out.extend_from_slice(&GOO_CRLF);
    out
}

/// Push little-endian run-length extension bytes shared by both grammars:
/// `low4 = value & 0xF`, then `nb` extension bytes (LE), `nb ≤ 3`.
/// Returns the first byte's `(nb << 4) | low4` bits.
fn le_run_ext(ext: &mut Vec<u8>, value: u32) -> u8 {
    debug_assert!(value < (1 << 28), "run-ext value exceeds 28 bits");
    let low4 = (value & 0xF) as u8;
    let mut hi = value >> 4;
    let mut nb = 0u8;
    while hi > 0 {
        ext.push((hi & 0xFF) as u8);
        hi >>= 8;
        nb += 1;
    }
    (nb << 4) | low4
}

// === Binary grammar (spec §7.2/§7.3) ===

/// Collapse engine runs into (code, length) pairs of the two binary value
/// codes, thresholding grayscale input. An empty stream (all-black layer)
/// becomes one black run of `panel_pixels`.
fn binary_code_runs(runs: &[RleRun], threshold: u8, panel_pixels: usize) -> Vec<(u8, u64)> {
    let mut out: Vec<(u8, u64)> = Vec::with_capacity(runs.len().min(4096) + 1);
    let mut covered: u64 = 0;
    for run in runs {
        if run.length == 0 {
            continue;
        }
        let code = if run.value > threshold {
            GOO_V5_CODE_WHITE
        } else {
            GOO_V5_CODE_BLACK
        };
        covered += run.length as u64;
        match out.last_mut() {
            Some((last_code, last_len)) if *last_code == code => *last_len += run.length as u64,
            _ => out.push((code, run.length as u64)),
        }
    }

    // Pad any uncovered tail with black so the panel is always complete.
    let panel = panel_pixels as u64;
    if covered < panel {
        let pad = panel - covered;
        match out.last_mut() {
            Some((code, len)) if *code == GOO_V5_CODE_BLACK => *len += pad,
            _ => out.push((GOO_V5_CODE_BLACK, pad)),
        }
    }
    out
}

/// Encode one panel with the binary grammar (§7.2): runs of ≥2 pixels emit
/// `tt=01` chunks carrying the value code AFTER the LE run-ext bytes with the
/// `+2` offset (stored run = pixels − 2); isolated pixels emit the value code
/// byte itself (`0x99`/`0xA7`, which live in the `tt=10` single-pixel range);
/// the final chunk omits its value byte (that position is the checksum).
pub(super) fn encode_panel_binary_from_runs(
    runs: &[RleRun],
    threshold: u8,
    panel_pixels: usize,
) -> Vec<u8> {
    let code_runs = binary_code_runs(runs, threshold, panel_pixels);
    let mut body = Vec::with_capacity(code_runs.len() * 4 + 8);

    // A `tt=01` chunk stores pixels−2, so it can hold 2..=(2^28−1)+2 pixels.
    const MAX_CHUNK: u64 = (1 << 28) - 1 + 2;

    let total_chunks: usize = code_runs
        .iter()
        .map(|&(_, len)| {
            if len == 1 {
                1
            } else {
                (len / MAX_CHUNK) as usize + usize::from(len % MAX_CHUNK != 0)
            }
        })
        .sum();

    let mut chunk_index = 0usize;
    for &(code, mut len) in &code_runs {
        if len == 1 {
            // Single pixel: the value code byte itself (tt=10 range).
            body.push(code);
            chunk_index += 1;
            continue;
        }
        while len > 0 {
            let mut take = len.min(MAX_CHUNK);
            // Never strand a length-1 remainder that a `+2` chunk can't hold.
            if len - take == 1 {
                take -= 1;
            }
            if take == 1 {
                body.push(code);
                chunk_index += 1;
                len -= 1;
                continue;
            }
            let mut ext = Vec::with_capacity(3);
            let bits = le_run_ext(&mut ext, (take - 2) as u32);
            body.push((0b01 << 6) | bits);
            body.extend_from_slice(&ext);
            chunk_index += 1;
            // The final chunk of the block omits its value byte.
            if chunk_index < total_chunks {
                body.push(code);
            }
            len -= take;
        }
    }

    frame_block(body)
}

// === VUF grammar (spec §7.7) ===

/// Quantize an 8-bit pixel into an N-level grayscale value (fixed-point
/// `pixel * max_level / 255`, round-nearest).
#[inline]
fn quantize_level(pixel: u8, max_level: u8) -> u8 {
    if max_level == 255 {
        return pixel;
    }
    (((pixel as u32) * (max_level as u32) + 127) / 255) as u8
}

/// Collapse engine runs into (level, length) pairs. Non-anti-aliased content
/// is thresholded to 0 / max_level (matching how the reference transcoder
/// feeds binary content through the VUF codec); grayscale content is
/// quantized per pixel value.
fn vuf_level_runs(
    runs: &[RleRun],
    is_anti_aliased: bool,
    threshold: u8,
    max_level: u8,
    panel_pixels: usize,
) -> Vec<(u8, u64)> {
    let mut out: Vec<(u8, u64)> = Vec::with_capacity(runs.len().min(4096) + 1);
    let mut covered: u64 = 0;
    for run in runs {
        if run.length == 0 {
            continue;
        }
        let level = if is_anti_aliased {
            quantize_level(run.value, max_level)
        } else if run.value > threshold {
            max_level
        } else {
            0
        };
        covered += run.length as u64;
        match out.last_mut() {
            Some((last_level, last_len)) if *last_level == level => {
                *last_len += run.length as u64
            }
            _ => out.push((level, run.length as u64)),
        }
    }

    let panel = panel_pixels as u64;
    if covered < panel {
        let pad = panel - covered;
        match out.last_mut() {
            Some((level, len)) if *level == 0 => *len += pad,
            _ => out.push((0, pad)),
        }
    }
    out
}

/// Emit `0x40-0x7F` continuation chunks extending the active level by `run`
/// pixels (stored run = pixels − 1, LE ext bytes).
fn push_vuf_continuation(body: &mut Vec<u8>, mut run: u64) {
    const MAX_CHUNK: u64 = 1 << 28; // stored value = run − 1 < 2^28
    while run > 0 {
        let take = run.min(MAX_CHUNK);
        let mut ext = Vec::with_capacity(3);
        let bits = le_run_ext(&mut ext, (take - 1) as u32);
        body.push(0x40 | bits);
        body.extend_from_slice(&ext);
        run -= take;
    }
}

/// Encode one panel with the VUF grammar (§7.7). Stateful: one persistent
/// active grayscale level (starts at 0). Level changes are introduced with a
/// wide-compact chunk (`0x00-0x07`: run = b0+1, absolute value byte follows)
/// or a single-pixel diff (`0x80-0xBF`, `0xA0` repurposed for +32); runs of
/// the active level extend via `0x40-0x7F` continuations. `0xC0+` is never
/// emitted.
pub(super) fn encode_panel_vuf_from_runs(
    runs: &[RleRun],
    is_anti_aliased: bool,
    threshold: u8,
    max_level: u8,
    panel_pixels: usize,
) -> Vec<u8> {
    let level_runs = vuf_level_runs(runs, is_anti_aliased, threshold, max_level, panel_pixels);
    let mut body = Vec::with_capacity(level_runs.len() * 4 + 8);
    let mut active: u8 = 0;

    for &(level, run) in &level_runs {
        if run == 0 {
            continue;
        }
        if level == active {
            push_vuf_continuation(&mut body, run);
            continue;
        }

        let diff = level as i32 - active as i32;
        if run == 1 && diff != 0 && (-32..=32).contains(&diff) {
            // Single-pixel signed diff from the active level.
            let b0 = if diff == 32 {
                0xA0
            } else {
                0x80 | (((diff + 0x20) as u8) & 0x3F)
            };
            body.push(b0);
            active = level;
            continue;
        }

        // Set the new level with a wide-compact chunk, then extend.
        let first = run.min(8);
        body.push((first - 1) as u8); // 0x00-0x07: run = b0 + 1
        body.push(level);
        active = level;
        if run > first {
            push_vuf_continuation(&mut body, run - first);
        }
    }

    frame_block(body)
}

// === Panel dispatch helpers ===

/// Encode one panel with the configured grammar.
pub(super) fn encode_panel_from_runs(
    runs: &[RleRun],
    settings: &GooV5Settings,
    threshold: u8,
    is_anti_aliased: bool,
    panel_pixels: usize,
) -> Vec<u8> {
    match settings.rle_mode {
        GooV5RleMode::Binary => encode_panel_binary_from_runs(runs, threshold, panel_pixels),
        GooV5RleMode::Vuf => encode_panel_vuf_from_runs(
            runs,
            is_anti_aliased,
            threshold,
            settings.max_level(),
            panel_pixels,
        ),
    }
}

/// Encode one panel directly from a column window of a full-width raw mask
/// (raw-mask fallback paths; the streaming block-RLE path is the fast path).
/// An empty mask encodes as an all-black panel.
pub(super) fn encode_panel_from_mask_window(
    mask: &[u8],
    full_width: usize,
    height: usize,
    start_col: usize,
    window_width: usize,
    settings: &GooV5Settings,
    threshold: u8,
    is_anti_aliased: bool,
) -> Vec<u8> {
    let mut acc = crate::rle::RleAccum::new();
    if !mask.is_empty() {
        for row in 0..height {
            let base = row * full_width + start_col;
            crate::rle::emit_row(&mut acc, &mask[base..base + window_width]);
        }
    }
    let runs = acc.finish();
    encode_panel_from_runs(
        &runs,
        settings,
        threshold,
        is_anti_aliased,
        window_width * height,
    )
}

// === Reference decoders (ports of the spec §12 / §7.7 reference decoders) ===

/// Validate the frame (`0x55` magic + checksum + CRLF) and return the chunk
/// stream body.
pub(super) fn unframe_block(block: &[u8]) -> Result<&[u8], String> {
    if block.len() < 4 || block[0] != GOO_LAYER_MAGIC {
        return Err("V5 block missing 0x55 magic".to_string());
    }
    if block[block.len() - 2..] != GOO_CRLF {
        return Err("V5 block missing trailing CRLF".to_string());
    }
    let body = &block[1..block.len() - 3];
    let sum: u8 = body.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    if !sum != block[block.len() - 3] {
        return Err("V5 block checksum mismatch".to_string());
    }
    Ok(body)
}

/// Decode one binary-grammar panel to a 0/255 mask (spec §12 reference).
pub(super) fn decode_panel_binary(block: &[u8], panel_pixels: usize) -> Result<Vec<u8>, String> {
    let body = unframe_block(block)?;
    let mut out = vec![0u8; panel_pixels];
    let mut i = 0usize;
    let mut q = 0usize;
    while q < body.len() && i < panel_pixels {
        let b0 = body[q];
        let tt = b0 >> 6;
        q += 1;
        let (val, run) = if tt == 2 {
            // Single pixel: value = the code byte itself.
            let mut run = 1usize;
            if (b0 & 0xF) == 0xF {
                run = body[q] as usize;
                q += 1;
            }
            (b0, run)
        } else {
            let nb = ((b0 >> 4) & 3) as usize;
            let mut run = (b0 & 0xF) as u32;
            for k in 0..nb {
                run |= (body[q] as u32) << (4 + 8 * k);
                q += 1;
            }
            let val = match tt {
                0 => 0x00,
                3 => 0xFF,
                _ => {
                    // Explicit value byte AFTER the run-ext; omitted on the
                    // final chunk.
                    if q < body.len() {
                        let v = body[q];
                        q += 1;
                        v
                    } else {
                        0x00
                    }
                }
            };
            (val, run as usize + 2)
        };
        let end = (i + run).min(panel_pixels);
        let white = val == GOO_V5_CODE_WHITE || val == 0xFF;
        if white {
            out[i..end].fill(255);
        }
        i = end;
    }
    if i != panel_pixels {
        return Err(format!(
            "binary panel decoded {} of {} pixels",
            i, panel_pixels
        ));
    }
    Ok(out)
}

/// Decode one VUF-grammar panel to grayscale levels (spec §7.7 table).
pub(super) fn decode_panel_vuf(
    block: &[u8],
    panel_pixels: usize,
    max_level: u8,
) -> Result<Vec<u8>, String> {
    let body = unframe_block(block)?;
    let mut out = vec![0u8; panel_pixels];
    let mut active: u8 = 0;
    let mut i = 0usize;
    let mut q = 0usize;
    while q < body.len() && i < panel_pixels {
        let b0 = body[q];
        q += 1;
        let (level, run) = match b0 {
            0x00..=0x07 => {
                let level = body[q];
                q += 1;
                (level, b0 as usize + 1)
            }
            0x08..=0x3F => {
                // Compact form: code 7 is the max_level sentinel.
                let code = b0 & 7;
                let level = if code == 7 { max_level } else { code };
                (level, (b0 >> 3) as usize)
            }
            0x40..=0x7F => {
                let nb = ((b0 >> 4) & 3) as usize;
                let mut run = (b0 & 0xF) as u32;
                for k in 0..nb {
                    run |= (body[q] as u32) << (4 + 8 * k);
                    q += 1;
                }
                (active, run as usize + 1)
            }
            0x80..=0xBF => {
                let diff = if b0 == 0xA0 {
                    32i32
                } else {
                    ((b0 & 0x3F) as i32) - 0x20
                };
                (((active as i32) + diff).clamp(0, 255) as u8, 1)
            }
            _ => return Err(format!("VUF panel contains 0xC0+ byte {:#04x}", b0)),
        };
        let end = (i + run).min(panel_pixels);
        out[i..end].fill(level);
        active = level;
        i = end;
    }
    if i != panel_pixels {
        return Err(format!("VUF panel decoded {} of {} pixels", i, panel_pixels));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rle::RleRun;

    fn runs_from_pixels(pixels: &[u8]) -> Vec<RleRun> {
        let mut acc = crate::rle::RleAccum::new();
        crate::rle::emit_row(&mut acc, pixels);
        acc.finish()
    }

    #[test]
    fn binary_panel_round_trips_mixed_content() {
        // black run, white run, isolated white pixel, isolated black pixel.
        let mut pixels = vec![0u8; 20];
        pixels.extend_from_slice(&[255; 30]);
        pixels.push(0);
        pixels.push(255); // isolated white between blacks
        pixels.extend_from_slice(&[0; 12]);
        let runs = runs_from_pixels(&pixels);

        let block = encode_panel_binary_from_runs(&runs, 127, pixels.len());
        let decoded = decode_panel_binary(&block, pixels.len()).expect("decode");
        let expected: Vec<u8> = pixels.iter().map(|&p| if p > 127 { 255 } else { 0 }).collect();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn binary_panel_final_chunk_omits_value_byte() {
        // Trailing black run: last chunk is tt=01 with no value byte, so the
        // byte before the checksum must be a run-ext byte, not 0xA7.
        let mut pixels = vec![255u8; 4];
        pixels.extend_from_slice(&[0; 100]);
        let runs = runs_from_pixels(&pixels);
        let block = encode_panel_binary_from_runs(&runs, 127, pixels.len());
        // body = block[1..len-3]; final chunk: b0=0x51 (tt=01,nb=1), ext.
        let body = &block[1..block.len() - 3];
        assert_ne!(*body.last().unwrap(), GOO_V5_CODE_BLACK);
        let decoded = decode_panel_binary(&block, pixels.len()).expect("decode");
        assert_eq!(&decoded[..4], &[255u8; 4][..]);
        assert!(decoded[4..].iter().all(|&p| p == 0));
    }

    #[test]
    fn binary_panel_empty_layer_is_single_black_run() {
        let block = encode_panel_binary_from_runs(&[], 127, 1000);
        let decoded = decode_panel_binary(&block, 1000).expect("decode");
        assert!(decoded.iter().all(|&p| p == 0));
        // magic + b0 + 2 ext bytes (998 needs 12 bits) + checksum + CRLF
        assert!(block.len() <= 8);
    }

    #[test]
    fn binary_panel_isolated_pixels_use_code_bytes() {
        let pixels = [0u8, 0, 255, 0, 0];
        let runs = runs_from_pixels(&pixels);
        let block = encode_panel_binary_from_runs(&runs, 127, pixels.len());
        assert!(block.contains(&GOO_V5_CODE_WHITE));
        let decoded = decode_panel_binary(&block, pixels.len()).expect("decode");
        assert_eq!(decoded, vec![0, 0, 255, 0, 0]);
    }

    #[test]
    fn vuf_panel_round_trips_binary_content() {
        let mut pixels = vec![0u8; 50];
        pixels.extend_from_slice(&[255; 25]);
        pixels.push(0);
        pixels.push(255);
        pixels.extend_from_slice(&[0; 300]);
        let runs = runs_from_pixels(&pixels);

        let block = encode_panel_vuf_from_runs(&runs, false, 127, 255, pixels.len());
        let decoded = decode_panel_vuf(&block, pixels.len(), 255).expect("decode");
        let expected: Vec<u8> = pixels.iter().map(|&p| if p > 127 { 255 } else { 0 }).collect();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn vuf_panel_round_trips_grayscale_content() {
        let mut pixels = Vec::new();
        for level in [0u8, 12, 130, 255, 254, 200, 0, 255] {
            pixels.extend_from_slice(&vec![level; 9]);
        }
        pixels.push(37); // isolated gray pixel (diff from 255... exceeds ±32 → compact)
        pixels.extend_from_slice(&[0; 40]);
        let runs = runs_from_pixels(&pixels);

        let block = encode_panel_vuf_from_runs(&runs, true, 127, 255, pixels.len());
        let decoded = decode_panel_vuf(&block, pixels.len(), 255).expect("decode");
        assert_eq!(decoded, pixels);
    }

    #[test]
    fn vuf_panel_never_emits_high_range_bytes() {
        let mut pixels = Vec::new();
        for level in 0u8..=255 {
            pixels.push(level);
        }
        let runs = runs_from_pixels(&pixels);
        let block = encode_panel_vuf_from_runs(&runs, true, 127, 255, pixels.len());
        let body = &block[1..block.len() - 3];
        // Value bytes of wide-compact chunks may be >= 0xC0; walk the grammar
        // instead of scanning raw bytes.
        let decoded = decode_panel_vuf(&block, pixels.len(), 255).expect("decode without 0xC0 opcodes");
        assert_eq!(decoded, pixels);
        assert!(!body.is_empty());
    }

    #[test]
    fn vuf_panel_empty_layer_is_single_continuation() {
        let block = encode_panel_vuf_from_runs(&[], false, 127, 255, 47_098_800);
        let decoded = decode_panel_vuf(&block, 47_098_800, 255).expect("decode");
        assert!(decoded.iter().all(|&p| p == 0));
        // magic + b0 + 3 ext bytes + checksum + CRLF = 8 bytes.
        assert_eq!(block.len(), 8);
    }

    #[test]
    fn vuf_quantizes_to_pixel_bit_width() {
        let pixels = vec![0u8, 36, 73, 109, 146, 182, 219, 255];
        let runs = runs_from_pixels(&pixels);
        let block = encode_panel_vuf_from_runs(&runs, true, 127, 7, pixels.len());
        let decoded = decode_panel_vuf(&block, pixels.len(), 255).expect("decode");
        let expected: Vec<u8> = pixels
            .iter()
            .map(|&p| (((p as u32) * 7 + 127) / 255) as u8)
            .collect();
        assert_eq!(decoded, expected);
    }
}
