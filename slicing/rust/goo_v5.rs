// GOO V5.1 container assembly — little-endian, per-layer column-partition
// panels, DRAM streaming tables (preamble + LDT/T1 + IEDT/T2 + RDT), 66-byte
// layer definitions, and an MD5-suffixed footer.
// Reference: /root/GOO_v5_Format_Spec.md (header size 195,492 as validated
// against real Satellite output; NOT UVtools PR #1129's 195,496).
use crate::engine::SlicerV3Error;
use crate::types::SliceJobV3;
use md5::{Digest, Md5};

use super::goo_encoder::compute_print_time_seconds;
use super::goo_layout::{push_crlf, push_str_fixed, push_u8};
use super::goo_metadata::{
    parse_goo_build_model_from_job, parse_goo_v5_settings_from_job,
    parse_software_info_from_metadata, parse_timing_model_from_job,
};
use super::goo_preview::build_goo_previews;
use super::goo_types::{
    GooTimingModel, GooV5PreparedLayer, GOO_FILE_MAGIC, GOO_V5_FOOTER_BLOB_SIZE,
    GOO_V5_LAYER_DEF_SIZE, GOO_V5_LDT_MARKER, GOO_V5_LDT_START, GOO_V5_MAGIC,
    GOO_V5_PARAMS_OFFSET, GOO_V5_PARTITION_COUNT, GOO_V5_PREAMBLE_OFFSET, GOO_V5_RDT_PAD_SIZE,
};
use super::goo_v5_rle::{push_f32_le, push_u16_le, push_u32_le};

/// Byte-swap RGB565 preview words: the V1.2 preview builder emits big-endian
/// words, V5.1 stores them little-endian.
fn rgb565_swap_to_le(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    for pair in out.chunks_exact_mut(2) {
        pair.swap(0, 1);
    }
    out
}

/// T2 / IEDT — the DRAM virtual page table (spec §5.1): one `[u32 addr]
/// [u32 page_size]` entry per RLE block. The running address is 64-bit;
/// `page_size` carries the 2³² wrap count; a one-time −162 applies after
/// block 0. `addr0 = 0x3A2 + 50049024 + 23040·L` (exact, no floor).
pub(super) fn write_goo_v5_t2(out: &mut Vec<u8>, block_sizes: &[usize], layer_count: u32) {
    let addr0: u64 = 0x3A2 + 50_049_024 + 23_040 * (layer_count as u64);
    let mut full: u64 = addr0;
    for (i, &bsz) in block_sizes.iter().enumerate() {
        let base = (bsz as u64) * 256;
        push_u32_le(out, (full & 0xFFFF_FFFF) as u32);
        push_u32_le(out, ((base + (full >> 32)) & 0xFFFF_FFFF) as u32);
        full += base;
        if i == 0 {
            full -= 162;
        }
    }
}

/// Footer blob (spec §8): `0x66` + NUL-terminated profile name + zero pad +
/// `f32 1.15` + `u32 0` + CRLF, sized to the RDT entry size. The profile
/// MUST equal the header profile @156 or the printer rejects the file.
fn build_goo_v5_footer_blob(profile_name: &str) -> Vec<u8> {
    const TAIL: usize = 4 + 4 + 2; // f32 + u32 + CRLF
    let mut blob = Vec::with_capacity(GOO_V5_FOOTER_BLOB_SIZE);
    blob.push(0x66);
    let name = profile_name.as_bytes();
    let max_name = GOO_V5_FOOTER_BLOB_SIZE - TAIL - 2; // magic + NUL
    blob.extend_from_slice(&name[..name.len().min(max_name)]);
    blob.push(0);
    blob.resize(GOO_V5_FOOTER_BLOB_SIZE - TAIL, 0);
    push_f32_le(&mut blob, 1.15);
    push_u32_le(&mut blob, 0);
    push_crlf(&mut blob);
    debug_assert_eq!(blob.len(), GOO_V5_FOOTER_BLOB_SIZE);
    blob
}

/// One 66-byte little-endian layer definition (spec §6), CRLF-terminated.
/// Slot order matches the V1.2 layer record: V5's wait-before-lift /
/// after-lift / after-retract land in the wait-after-cure / after-lift /
/// before-cure positions.
fn write_goo_v5_layer_def(
    out: &mut Vec<u8>,
    layer_index: usize,
    layer_height_mm: f32,
    timing: &GooTimingModel,
) {
    let is_bottom = (layer_index as u32) < timing.bottom_layer_count;
    let (
        exposure, light_off, w_after_cure, w_after_lift, w_before_cure,
        lift_h, lift_s, lift_h2, lift_s2,
        ret_h, ret_s, ret_h2, ret_s2,
        pwm,
    ) = if is_bottom {
        (
            timing.bottom_exposure_sec,
            timing.bottom_light_off_delay_sec,
            timing.bottom_wait_time_after_cure_sec,
            timing.bottom_wait_time_after_lift_sec,
            timing.bottom_wait_time_before_cure_sec,
            timing.bottom_lift_distance_mm,
            timing.bottom_lift_speed_mm_min,
            timing.bottom_lift_distance2_mm,
            timing.bottom_lift_speed2_mm_min,
            timing.bottom_retract_distance_mm,
            timing.bottom_retract_speed_mm_min,
            timing.bottom_retract_distance2_mm,
            timing.bottom_retract_speed2_mm_min,
            timing.bottom_light_pwm,
        )
    } else {
        (
            timing.normal_exposure_sec,
            timing.light_off_delay_sec,
            timing.wait_time_after_cure_sec,
            timing.wait_time_after_lift_sec,
            timing.wait_time_before_cure_sec,
            timing.lift_distance_mm,
            timing.lift_speed_mm_min,
            timing.lift_distance2_mm,
            timing.lift_speed2_mm_min,
            timing.retract_distance_mm,
            timing.retract_speed_mm_min,
            timing.retract_distance2_mm,
            timing.retract_speed2_mm_min,
            timing.light_pwm,
        )
    };

    let def_start = out.len();

    push_u16_le(out, 0);                                        // Pause flag
    push_f32_le(out, 0.0);                                      // Pause position Z
    push_f32_le(out, (layer_index as f32 + 1.0) * layer_height_mm); // Position Z
    push_f32_le(out, exposure);                                 // Exposure time
    push_f32_le(out, light_off);                                // Layer off time
    push_f32_le(out, w_after_cure);                             // Wait before lift
    push_f32_le(out, w_after_lift);                             // Wait after lift
    push_f32_le(out, w_before_cure);                            // Wait after retract
    push_f32_le(out, lift_h);                                   // Lift distance
    push_f32_le(out, lift_s);                                   // Lift speed
    push_f32_le(out, lift_h2);                                  // Lift distance 2
    push_f32_le(out, lift_s2);                                  // Lift speed 2
    push_f32_le(out, ret_h);                                    // Retract distance
    push_f32_le(out, ret_s);                                    // Retract speed
    push_f32_le(out, ret_h2);                                   // Retract distance 2
    push_f32_le(out, ret_s2);                                   // Retract speed 2
    push_u16_le(out, pwm);                                      // Light PWM
    push_crlf(out);                                             // Delimiter

    debug_assert_eq!(out.len() - def_start, GOO_V5_LAYER_DEF_SIZE);
}

pub(super) fn build_goo_v5_container_bytes(
    job: &SliceJobV3,
    prepared: &[GooV5PreparedLayer],
) -> Result<Vec<u8>, SlicerV3Error> {
    build_goo_v5_container_bytes_with_progress(job, prepared, None)
}

pub(super) fn build_goo_v5_container_bytes_with_progress(
    job: &SliceJobV3,
    prepared: &[GooV5PreparedLayer],
    on_progress: Option<&dyn Fn(u32, u32)>,
) -> Result<Vec<u8>, SlicerV3Error> {
    if prepared.is_empty() {
        return Err(SlicerV3Error::MissingRenderedLayerPayload(
            "no rendered layers were provided for Goo V5 encoding".to_string(),
        ));
    }
    for layer in prepared {
        if layer.blocks.len() != GOO_V5_PARTITION_COUNT {
            return Err(SlicerV3Error::UnsupportedOutput(format!(
                "Goo V5 layer {} has {} partition blocks, expected {}",
                layer.index,
                layer.blocks.len(),
                GOO_V5_PARTITION_COUNT
            )));
        }
    }

    let mut timing = parse_timing_model_from_job(job);
    let build = parse_goo_build_model_from_job(job);
    let v5 = parse_goo_v5_settings_from_job(job);
    let software_version = parse_software_info_from_metadata(&job.metadata_json);

    let previews = build_goo_previews(job.export_thumbnail_png_base64.as_deref())?;

    let layer_count = prepared.len() as u32;
    let print_time_sec = compute_print_time_seconds(prepared.len(), &timing);
    let is_grayscale = job.produces_grayscale_output();

    let l = layer_count as u64;
    let ldt_start = GOO_V5_LDT_START as u64;
    let iedt_marker = ldt_start + l * 8;
    let rdt_marker = ldt_start + l * 24 + 1;
    let defs_base = ldt_start + l * 24 + GOO_V5_RDT_PAD_SIZE as u64;
    let total_rle: u64 = prepared
        .iter()
        .flat_map(|p| p.blocks.iter())
        .map(|b| b.len() as u64)
        .sum();
    let end_of_rle = defs_base + l * (GOO_V5_LAYER_DEF_SIZE as u64) + total_rle;

    let mut out =
        Vec::with_capacity(end_of_rle as usize + GOO_V5_FOOTER_BLOB_SIZE + 32);

    if timing.bottom_retract_distance2_mm <= job.layer_height_mm && timing.retract_distance2_mm <= job.layer_height_mm {
        timing.bottom_retract_distance2_mm -= job.layer_height_mm;
        timing.retract_distance2_mm -= job.layer_height_mm;
    } else {
        timing.bottom_retract_distance_mm -= job.layer_height_mm;
        timing.retract_distance_mm -= job.layer_height_mm;
    }

    // ── Header + previews (0 .. 195,310) ──────────────────────────────────
    out.extend_from_slice(GOO_V5_MAGIC);               // "V5.1"
    out.extend_from_slice(&GOO_FILE_MAGIC);            // u32(7) LE + "DLP\0"
    push_str_fixed(&mut out, &v5.slicer_name, 32);     // slicer name @12 (§16)
    push_str_fixed(&mut out, &software_version, 24);
    push_str_fixed(&mut out, &build.created_datetime, 24);
    push_str_fixed(&mut out, &v5.printer_name, 32);    // printer name @92 (§16)
    push_str_fixed(&mut out, &v5.printer_type, 32);    // printer type @124 (§16)
    // Profile @156 must equal the footer profile (§16). 32 bytes: the V5.1
    // pre-preview region is 194 bytes (forced by params @195,310 − previews),
    // matching the V1.2 layout that §14's transcoder copies verbatim — the
    // §2 table's str(24) is inconsistent with its own §3 offsets.
    push_str_fixed(&mut out, &v5.profile_name, 32);
    push_u16_le(&mut out, build.anti_aliasing_level);
    push_u16_le(&mut out, build.grey_level);
    push_u16_le(&mut out, build.blur_level);
    out.extend_from_slice(&rgb565_swap_to_le(&previews.small)); // 116×116 RGB565 LE
    push_crlf(&mut out);
    out.extend_from_slice(&rgb565_swap_to_le(&previews.large)); // 290×290 RGB565 LE
    push_crlf(&mut out);
    assert_eq!(out.len(), GOO_V5_PARAMS_OFFSET as usize);

    // ── Printer parameters (195,310 .. 195,477, §3) — LE, V1.2 slot order ──
    push_u32_le(&mut out, layer_count);
    push_u16_le(&mut out, job.source_width_px as u16);
    push_u16_le(&mut out, job.source_height_px as u16);
    push_u8(&mut out, build.mirror_x as u8);
    push_u8(&mut out, build.mirror_y as u8);
    push_f32_le(&mut out, job.build_width_mm);
    push_f32_le(&mut out, job.build_depth_mm);
    push_f32_le(&mut out, build.machine_z_mm);
    push_f32_le(&mut out, job.layer_height_mm);
    push_f32_le(&mut out, timing.normal_exposure_sec);
    push_u8(&mut out, timing.delay_mode);
    push_f32_le(&mut out, timing.light_off_delay_sec);
    push_f32_le(&mut out, timing.bottom_wait_time_after_cure_sec); // bottom wait before lift
    push_f32_le(&mut out, timing.bottom_wait_time_after_lift_sec);
    push_f32_le(&mut out, timing.bottom_wait_time_before_cure_sec); // bottom wait after retract
    push_f32_le(&mut out, timing.wait_time_after_cure_sec);        // wait before lift
    push_f32_le(&mut out, timing.wait_time_after_lift_sec);
    push_f32_le(&mut out, timing.wait_time_before_cure_sec);       // wait after retract
    push_f32_le(&mut out, timing.bottom_exposure_sec);
    push_u32_le(&mut out, timing.bottom_layer_count);
    push_f32_le(&mut out, timing.bottom_lift_distance_mm);
    push_f32_le(&mut out, timing.bottom_lift_speed_mm_min);
    push_f32_le(&mut out, timing.lift_distance_mm);
    push_f32_le(&mut out, timing.lift_speed_mm_min);
    push_f32_le(&mut out, timing.bottom_retract_distance_mm);
    push_f32_le(&mut out, timing.bottom_retract_speed_mm_min);
    push_f32_le(&mut out, timing.retract_distance_mm);
    push_f32_le(&mut out, timing.retract_speed_mm_min);
    push_f32_le(&mut out, timing.bottom_lift_distance2_mm);
    push_f32_le(&mut out, timing.bottom_lift_speed2_mm_min);
    push_f32_le(&mut out, timing.lift_distance2_mm);
    push_f32_le(&mut out, timing.lift_speed2_mm_min);
    push_f32_le(&mut out, timing.bottom_retract_distance2_mm);
    push_f32_le(&mut out, timing.bottom_retract_speed2_mm_min);
    push_f32_le(&mut out, timing.retract_distance2_mm);
    push_f32_le(&mut out, timing.retract_speed2_mm_min);
    push_u16_le(&mut out, timing.bottom_light_pwm);
    push_u16_le(&mut out, timing.light_pwm);
    push_u8(&mut out, 0); // advance/per-layer mode: dynamic (real per-layer defs)
    push_u32_le(&mut out, print_time_sec);
    push_f32_le(&mut out, 0.0); // total volume
    push_f32_le(&mut out, 0.0); // total weight
    push_f32_le(&mut out, 0.0); // total price
    push_str_fixed(&mut out, "$", 8);
    push_u32_le(&mut out, GOO_V5_PREAMBLE_OFFSET); // offset of layer content
    push_u8(&mut out, if is_grayscale { 1 } else { 0 });
    push_u16_le(&mut out, timing.transition_layer_count);
    assert_eq!(out.len(), GOO_V5_PREAMBLE_OFFSET as usize);

    // ── DRAM preamble (§4.1) ──────────────────────────────────────────────
    push_u8(&mut out, GOO_V5_PARTITION_COUNT as u8);
    push_u32_le(&mut out, GOO_V5_LDT_MARKER);
    push_u32_le(&mut out, iedt_marker as u32);
    push_u32_le(&mut out, rdt_marker as u32);
    push_u8(&mut out, 0); // support anti-aliasing (not validated)
    push_u8(&mut out, v5.pixel_bit_width);
    push_u8(&mut out, 0xA1); // LDT magic — first byte of T1
    assert_eq!(out.len(), GOO_V5_LDT_START as usize);

    // ── T1 / LDT (§5) ─────────────────────────────────────────────────────
    for i in 0..l {
        push_u32_le(&mut out, (defs_base + i * GOO_V5_LAYER_DEF_SIZE as u64) as u32);
        push_u32_le(&mut out, GOO_V5_LAYER_DEF_SIZE as u32);
    }
    assert_eq!(out.len() as u64, iedt_marker);

    // ── T2 / IEDT (§5.1) ──────────────────────────────────────────────────
    let block_sizes: Vec<usize> = prepared
        .iter()
        .flat_map(|p| p.blocks.iter().map(|b| b.len()))
        .collect();
    write_goo_v5_t2(&mut out, &block_sizes, layer_count);
    assert_eq!(out.len() as u64, ldt_start + l * 24);

    // ── RDT pad (§5.2): 00 A3 count=1 [offset=end_of_rle][size=blob] ─────
    push_u8(&mut out, 0);
    push_u8(&mut out, 0xA3);
    push_u32_le(&mut out, 1);
    push_u32_le(&mut out, end_of_rle as u32);
    push_u32_le(&mut out, GOO_V5_FOOTER_BLOB_SIZE as u32);
    assert_eq!(out.len() as u64, defs_base);

    // ── Layer definitions (§6) ────────────────────────────────────────────
    for layer in prepared {
        write_goo_v5_layer_def(&mut out, layer.index, job.layer_height_mm, &timing);
    }
    assert_eq!(out.len() as u64, defs_base + l * GOO_V5_LAYER_DEF_SIZE as u64);

    // ── RLE image data: 2 framed blocks per layer, left then right ───────
    let total_prepared = prepared.len() as u32;
    for (idx, layer) in prepared.iter().enumerate() {
        for block in &layer.blocks {
            out.extend_from_slice(block);
        }
        if let Some(progress) = on_progress {
            progress((idx as u32) + 1, total_prepared.max(1));
        }
    }
    assert_eq!(out.len() as u64, end_of_rle);

    // ── Footer (§8): profile blob + 32-char lowercase MD5 over the rest ──
    out.extend_from_slice(&build_goo_v5_footer_blob(&v5.profile_name));
    let digest = Md5::digest(&out);
    let mut hex = String::with_capacity(32);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{:02x}", byte);
    }
    out.extend_from_slice(hex.as_bytes());

    Ok(out)
}
