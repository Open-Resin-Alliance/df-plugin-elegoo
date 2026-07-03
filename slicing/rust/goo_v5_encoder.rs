// GOO V5.1 encoder (Little Endian) — matches ELEGOO SatelLite output for Jupiter 2
use crate::engine::SlicerV3Error;
use crate::types::SliceJobV3;
use md5::{Digest, Md5};
use serde_json::Value;

use super::goo_layout::{
    encode_v5_layer_left_right, push_crlf, push_f32_le, push_str_fixed, push_u16_le, push_u32_le,
    push_u8,
};
use super::goo_metadata::{
    compute_print_time_seconds, parse_goo_build_model_from_job, parse_software_info_from_metadata,
    parse_timing_model_from_job,
};
use super::goo_preview::build_goo_previews;
use super::goo_types::{
    GooBuildModel, GooPreparedLayer, GooTimingModel, GOO_FILE_MAGIC, GOO_HEADER_SIZE,
    GOO_V5_FILE_VERSION, GOO_V5_LAYER_DEF_SIZE,
};

/// Prepare layers for V5 (RLE encoding is endianness-agnostic; shared with V1.2 path).
pub(super) fn prepare_layers_for_goo_v5_with_progress(
    raw_masks: &[Vec<u8>],
    is_anti_aliased: bool,
    threshold: u8,
    layer_height_mm: f32,
    bottom_layer_count: u32,
    full_width_px: u16,
    on_progress: Option<&dyn Fn(u32, u32)>,
) -> Vec<GooPreparedLayer> {
    let total = (raw_masks.len() * 2) as u32;
    let mut layers = Vec::with_capacity(raw_masks.len() * 2);
    for (layer_id, mask) in raw_masks.iter().enumerate() {
        let (left, right) = encode_v5_layer_left_right(
            layer_id,
            mask,
            full_width_px,
            is_anti_aliased,
            threshold,
            layer_height_mm,
            bottom_layer_count,
        );
        layers.push(left);
        layers.push(right);
        if let Some(cb) = on_progress {
            cb((layer_id * 2 + 2) as u32, total.max(1));
        }
    }
    layers
}

pub(super) fn build_goo_v5_container_bytes_with_progress(
    job: &SliceJobV3,
    prepared: &[GooPreparedLayer],
    on_progress: Option<&dyn Fn(u32, u32)>,
) -> Result<Vec<u8>, SlicerV3Error> {
    let timing = parse_timing_model_from_job(job);
    let build = parse_goo_build_model_from_job(job);
    let software_version = parse_software_info_from_metadata(&job.metadata_json);
    let previews = build_goo_previews(job.export_thumbnail_png_base64.as_deref())?;
    let rle_block_count = prepared.len() as u32;
    let layer_count = rle_block_count;
    eprintln!(
        "[GooV5-CONTAINER] rle_blocks={} layer_count={}",
        rle_block_count, layer_count
    );
    let print_time = compute_print_time_seconds(layer_count as usize, &timing);
    let machine_z_mm = parse_machine_z_from_metadata(&job.metadata_json);
    let is_grayscale = job.produces_grayscale_output();

    let mut out = Vec::with_capacity(GOO_HEADER_SIZE as usize + prepared.len() * 512);

    // ── Header (195477 bytes, Little Endian) ──────────────────────────────
    write_goo_v5_header(
        &mut out,
        &timing,
        &build,
        &software_version,
        &previews.small,
        &previews.large,
        layer_count,
        job.source_width_px as u16,
        job.source_height_px as u16,
        job.build_width_mm,
        job.build_depth_mm,
        machine_z_mm,
        job.layer_height_mm,
        print_time,
        is_grayscale,
    );

    debug_assert_eq!(
        out.len() as u32,
        GOO_HEADER_SIZE,
        "Goo V5 header size mismatch: wrote {}, expected {}",
        out.len(),
        GOO_HEADER_SIZE
    );

    // ── Layer section ─────────────────────────────────────────────────────
    // 16-byte section header (matches SatelLite static-mode output)
    out.extend_from_slice(&[
        0x02, 0xa4, 0xfb, 0x02, 0x00, 0x25, 0x08, 0x03, 0x00, 0x26, 0x21, 0x03, 0x00, 0x78, 0x03,
        0xa1,
    ]);

    // Table 1: per-layer settings pointers (layer_count × 8 bytes)
    let t1_start = out.len();
    out.resize(out.len() + layer_count as usize * 8, 0);

    // T2: virtual-memory page table — 2 entries per layer × 8 bytes each
    let t2_start = out.len();
    // T2: 2 entries per logical layer, each entry is 8 bytes (u32 addr + u32 size)
    let t2_size = layer_count as usize * 2 * 8;
    out.resize(out.len() + t2_size, 0);

    // 14-byte padding between T2 and per-layer settings (matches SatelLite)
    out.extend_from_slice(&[
        0x00, 0xa3, 0x01, 0x00, 0x00, 0x00, 0x33, 0x70, 0x45, 0x00, 0x8e, 0x00, 0x00, 0x00,
    ]);

    // Per-layer settings (66 bytes each, all LE)
    let mut t1_entries: Vec<u32> = Vec::with_capacity(layer_count as usize);
    for i in 0..layer_count as usize {
        t1_entries.push(out.len() as u32);
        write_goo_v5_layer_def(&mut out, i, &timing, job.layer_height_mm);
    }

    // RLE data — concatenated byte streams (no per-layer delimiters in V5.1)
    for layer in prepared.iter() {
        out.extend_from_slice(&layer.encoded);
    }

    // ── Fill Table 1 ──────────────────────────────────────────────────────
    for (i, &off) in t1_entries.iter().enumerate() {
        let base = t1_start + i * 8;
        out[base..base + 4].copy_from_slice(&off.to_le_bytes());
        out[base + 4..base + 8].copy_from_slice(&GOO_V5_LAYER_DEF_SIZE.to_le_bytes());
    }

    // ── Fill T2 ───────────────────────────────────────────────────────────
    // T2 maps each layer to 2 virtual-memory pages. v1=virtual address, v2=page size.
    let total_rle: u64 = prepared.iter().map(|l| l.encoded.len() as u64).sum();
    let page_size: u32 = ((total_rle * 105 / 100) / layer_count as u64) as u32;
    let base_addr: u32 = 0x037449a2; // matches SatelLite base

    // 2 T2 entries per logical layer (one per half-screen block)
    for i in 0..(layer_count as usize * 2) {
        let addr_a = base_addr.wrapping_add(i as u32 * page_size);
        let t2_off = t2_start + i * 8;
        out[t2_off..t2_off + 4].copy_from_slice(&addr_a.to_le_bytes());
        out[t2_off + 4..t2_off + 8].copy_from_slice(&page_size.to_le_bytes());
    }

    // ── Footer: padding + MD5 hash ────────────────────────────────────────
    out.extend_from_slice(&[0u8; 20]);
    push_f32_le(&mut out, 1.15);
    push_u32_le(&mut out, 0);
    push_crlf(&mut out);

    let digest = Md5::digest(&out);
    out.extend_from_slice(format!("{:032x}", digest).as_bytes());

    if let Some(cb) = on_progress {
        cb(layer_count + 1, layer_count + 1);
    }
    Ok(out)
}

fn parse_machine_z_from_metadata(metadata_json: &str) -> f32 {
    let Ok(meta) = serde_json::from_str::<Value>(metadata_json) else {
        return 220.0;
    };
    meta.get("printer")
        .and_then(|o| o.get("buildVolumeMm"))
        .and_then(|o| o.get("height"))
        .and_then(Value::as_f64)
        .map(|v| v as f32)
        .unwrap_or(220.0)
}

#[allow(clippy::too_many_arguments)]
fn write_goo_v5_header(
    out: &mut Vec<u8>,
    timing: &GooTimingModel,
    build: &GooBuildModel,
    software_version: &str,
    small_preview: &[u8],
    large_preview: &[u8],
    layer_count: u32,
    res_x: u16,
    res_y: u16,
    display_width_mm: f32,
    display_height_mm: f32,
    machine_z_mm: f32,
    layer_height_mm: f32,
    print_time_sec: u32,
    is_grayscale: bool,
) {
    out.extend_from_slice(GOO_V5_FILE_VERSION);
    out.extend_from_slice(&GOO_FILE_MAGIC);
    push_str_fixed(out, "DragonFruit", 32);
    push_str_fixed(out, software_version, 24);
    push_str_fixed(out, &build.created_datetime, 24);
    push_str_fixed(out, &build.machine_name, 32);
    push_str_fixed(out, &build.machine_type, 32);
    push_str_fixed(out, &build.profile_name, 32);
    push_u16_le(out, build.anti_aliasing_level);
    push_u16_le(out, build.grey_level);
    push_u16_le(out, build.blur_level);

    out.extend_from_slice(small_preview);
    push_crlf(out);
    out.extend_from_slice(large_preview);
    push_crlf(out);

    push_u32_le(out, layer_count);
    push_u16_le(out, res_x);
    push_u16_le(out, res_y);
    push_u8(out, build.mirror_x as u8);
    push_u8(out, build.mirror_y as u8);
    push_f32_le(out, display_width_mm);
    push_f32_le(out, display_height_mm);
    push_f32_le(out, machine_z_mm);
    push_f32_le(out, layer_height_mm);
    push_f32_le(out, timing.normal_exposure_sec);
    push_u8(out, timing.delay_mode);
    push_f32_le(out, timing.light_off_delay_sec);
    push_f32_le(out, timing.bottom_wait_time_after_cure_sec);
    push_f32_le(out, timing.bottom_wait_time_after_lift_sec);
    push_f32_le(out, timing.bottom_wait_time_before_cure_sec);
    push_f32_le(out, timing.wait_time_after_cure_sec);
    push_f32_le(out, timing.wait_time_after_lift_sec);
    push_f32_le(out, timing.wait_time_before_cure_sec);
    push_f32_le(out, timing.bottom_exposure_sec);
    push_u32_le(out, timing.bottom_layer_count);
    push_f32_le(out, timing.bottom_lift_distance_mm);
    push_f32_le(out, timing.bottom_lift_speed_mm_min);
    push_f32_le(out, timing.lift_distance_mm);
    push_f32_le(out, timing.lift_speed_mm_min);
    push_f32_le(out, timing.bottom_retract_distance_mm);
    push_f32_le(out, timing.bottom_retract_speed_mm_min);
    push_f32_le(out, timing.retract_distance_mm);
    push_f32_le(out, timing.retract_speed_mm_min);
    push_f32_le(out, timing.bottom_lift_distance2_mm);
    push_f32_le(out, timing.bottom_lift_speed2_mm_min);
    push_f32_le(out, timing.lift_distance2_mm);
    push_f32_le(out, timing.lift_speed2_mm_min);
    push_f32_le(out, timing.bottom_retract_distance2_mm);
    push_f32_le(out, timing.bottom_retract_speed2_mm_min);
    push_f32_le(out, timing.retract_distance2_mm);
    push_f32_le(out, timing.retract_speed2_mm_min);
    push_u16_le(out, timing.bottom_light_pwm);
    push_u16_le(out, timing.light_pwm);
    push_u8(out, 0); // PerLayerSettings = false (static mode)
    push_u32_le(out, print_time_sec);
    push_f32_le(out, 0.0);
    push_f32_le(out, 0.0);
    push_f32_le(out, 0.0);
    push_str_fixed(out, "$", 8);
    push_u32_le(out, GOO_HEADER_SIZE);
    push_u8(out, if is_grayscale { 1 } else { 0 });
    push_u16_le(out, timing.transition_layer_count);
}

/// Per-layer definition — 66 bytes, all LE.
///
/// Layout matches ELEGOO SatelLite V5.1:
///   u16 Pause  |  f32[0] max-Z  |  f32[1] PositionZ  |  f32[2] Exposure
///   f32[3] LightOff  |  f32[4] WAfterCure  |  f32[5] WAfterLift  |  f32[6] WBeforeCure
///   f32[7] LiftH  |  f32[8] LiftS  |  f32[9] LiftH2  |  f32[10] LiftS2
///   f32[11] RetractH  |  f32[12] RetractS  |  f32[13] RetractH2  |  f32[14] RetractS2
///   u16 PWM  |  u16 CRLF
fn write_goo_v5_layer_def(
    out: &mut Vec<u8>,
    layer_index: usize,
    timing: &GooTimingModel,
    layer_height_mm: f32,
) {
    let z = (layer_index as f32 + 1.0) * layer_height_mm;
    let is_bottom = (layer_index as u32) < timing.bottom_layer_count;

    let (exp, lo, wac, wal, wbc, lh, ls, lh2, ls2, rh, rs, rh2, rs2, pwm) = if is_bottom {
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

    push_u16_le(out, 0);
    push_f32_le(out, 200.0); // f32[0] — constant 200.0 in all SatelLite V5.1 files - LIKELY pausePosition from GOO v1.2
    push_f32_le(out, z);
    push_f32_le(out, exp);
    push_f32_le(out, lo);
    push_f32_le(out, wac);
    push_f32_le(out, wal);
    push_f32_le(out, wbc);
    push_f32_le(out, lh);
    push_f32_le(out, ls);
    push_f32_le(out, lh2);
    push_f32_le(out, ls2);
    push_f32_le(out, rh);
    push_f32_le(out, rs);
    push_f32_le(out, rh2);
    push_f32_le(out, rs2);
    push_u16_le(out, pwm);
    push_crlf(out);
}
