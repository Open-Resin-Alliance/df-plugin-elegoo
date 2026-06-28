use crate::engine::SlicerV3Error;
use crate::types::SliceJobV3;
use serde_json::Value;

use super::goo_layout::{
    encode_single_goo_layer_from_raw_mask, push_crlf, push_f32_be, push_str_fixed, push_u16_be,
    push_u32_be, push_u8,
};
use super::goo_metadata::{
    compute_print_time_seconds, parse_goo_build_model_from_job, parse_software_info_from_metadata,
    parse_timing_model_from_job,
};
use super::goo_preview::build_goo_previews;
use super::goo_types::{
    GooBuildModel, GooPreparedLayer, GooTimingModel, GOO_FILE_MAGIC, GOO_FILE_VERSION,
    GOO_HEADER_SIZE,
};

pub(super) fn prepare_layers_for_goo_with_progress(
    raw_masks: &[Vec<u8>],
    is_anti_aliased: bool,
    threshold: u8,
    layer_height_mm: f32,
    bottom_layer_count: u32,
    on_progress: Option<&dyn Fn(u32, u32)>,
) -> Vec<GooPreparedLayer> {
    let total = raw_masks.len() as u32;
    raw_masks
        .iter()
        .enumerate()
        .map(|(layer_id, mask)| {
            let layer = encode_single_goo_layer_from_raw_mask(
                layer_id,
                mask,
                is_anti_aliased,
                threshold,
                layer_height_mm,
                bottom_layer_count,
            );
            if let Some(cb) = on_progress {
                cb(layer_id as u32 + 1, total.max(1));
            }
            layer
        })
        .collect()
}

#[cfg(test)]
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
    let timing = parse_timing_model_from_job(job);
    let build = parse_goo_build_model_from_job(job);
    let software_version = parse_software_info_from_metadata(&job.metadata_json);

    let previews = build_goo_previews(job.export_thumbnail_png_base64.as_deref())?;

    let layer_count = prepared.len() as u32;
    let print_time = compute_print_time_seconds(prepared.len(), &timing);
    let machine_z_mm = parse_machine_z_from_metadata(&job.metadata_json);
    let is_grayscale = job.produces_grayscale_output();

    let mut out = Vec::with_capacity(GOO_HEADER_SIZE as usize + prepared.len() * 512);

    write_goo_header(
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
        "Goo header size mismatch: wrote {} bytes, expected {}",
        out.len(),
        GOO_HEADER_SIZE,
    );

    let total_progress = layer_count.saturating_add(1).max(1);
    for (i, layer) in prepared.iter().enumerate() {
        write_goo_layer(&mut out, layer, &timing);
        if let Some(cb) = on_progress {
            cb(i as u32 + 1, total_progress);
        }
    }
    if let Some(cb) = on_progress {
        cb(total_progress, total_progress);
    }

    push_u8(&mut out, 0); push_u8(&mut out, 0); push_u8(&mut out, 0);
    out.extend_from_slice(&GOO_FILE_MAGIC);

    Ok(out)
}

fn parse_machine_z_from_metadata(metadata_json: &str) -> f32 {
    let default = 220.0 as f32;
    let Ok(meta) = serde_json::from_str::<Value>(metadata_json) else {
        return default;
    };
    meta.get("printer")
        .and_then(|o| o.get("buildVolumeMm"))
        .and_then(|o| o.get("height"))
        .and_then(Value::as_f64)
        .map(|v| v as f32)
        .unwrap_or(default)
}

#[allow(clippy::too_many_arguments)]
fn write_goo_header(
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
    // ── Pre-preview fields (194 bytes) ────────────────────────────────────
    out.extend_from_slice(GOO_FILE_VERSION);           // [0]  4 bytes
    out.extend_from_slice(&GOO_FILE_MAGIC);            // [1]  8 bytes
    push_str_fixed(out, "DragonFruit", 32);            // [2] 32 bytes
    push_str_fixed(out, software_version, 24);         // [3] 24 bytes
    push_str_fixed(out, &build.created_datetime, 24);  // [4] 24 bytes
    push_str_fixed(out, &build.machine_name, 32);      // [5] 32 bytes
    push_str_fixed(out, &build.machine_type, 32);      // [6] 32 bytes
    push_str_fixed(out, &build.profile_name, 32);      // [7] 32 bytes
    push_u16_be(out, build.anti_aliasing_level);       // [8]   2 bytes
    push_u16_be(out, build.grey_level);                // [9]   2 bytes
    push_u16_be(out, build.blur_level);                // [10]  2 bytes

    // ── Previews (195116 bytes) ───────────────────────────────────────────
    out.extend_from_slice(small_preview);              // [11] 26912 bytes
    push_crlf(out);                                    // [12]   2 bytes
    out.extend_from_slice(large_preview);              // [13] 168200 bytes
    push_crlf(out);                                    // [14]   2 bytes

    // ── Post-preview part 1 (67 bytes) ───────────────────────────────────
    push_u32_be(out, layer_count);                     // [15]  4 bytes
    push_u16_be(out, res_x);                           // [16]  2 bytes
    push_u16_be(out, res_y);                           // [17]  2 bytes
    push_u8(out, build.mirror_x as u8);                // [18]  1 byte
    push_u8(out, build.mirror_y as u8);                // [19]  1 byte
    push_f32_be(out, display_width_mm);                // [20]  4 bytes
    push_f32_be(out, display_height_mm);               // [21]  4 bytes
    push_f32_be(out, machine_z_mm);                    // [22]  4 bytes
    push_f32_be(out, layer_height_mm);                 // [23]  4 bytes
    push_f32_be(out, timing.normal_exposure_sec);      // [24]  4 bytes
    push_u8(out, timing.delay_mode);                   // [25]  1 byte
    push_f32_be(out, timing.light_off_delay_sec);      // [26]  4 bytes
    // [27-32] Wait times — 6 × f32 = 24 bytes
    push_f32_be(out, timing.bottom_wait_time_after_cure_sec);
    push_f32_be(out, timing.bottom_wait_time_after_lift_sec);
    push_f32_be(out, timing.bottom_wait_time_before_cure_sec);
    push_f32_be(out, timing.wait_time_after_cure_sec);
    push_f32_be(out, timing.wait_time_after_lift_sec);
    push_f32_be(out, timing.wait_time_before_cure_sec);
    push_f32_be(out, timing.bottom_exposure_sec);      // [33]  4 bytes
    push_u32_be(out, timing.bottom_layer_count);       // [34]  4 bytes

    // ── Lift / Retract — 16 × f32 = 64 bytes ─────────────────────────────
    push_f32_be(out, timing.bottom_lift_distance_mm);  // [35] BottomLiftHeight
    push_f32_be(out, timing.bottom_lift_speed_mm_min); // [36] BottomLiftSpeed
    push_f32_be(out, timing.lift_distance_mm);         // [37] LiftHeight
    push_f32_be(out, timing.lift_speed_mm_min);        // [38] LiftSpeed
    push_f32_be(out, timing.bottom_retract_distance_mm); // [39] BottomRetractHeight
    push_f32_be(out, timing.bottom_retract_speed_mm_min); // [40] BottomRetractSpeed
    push_f32_be(out, timing.retract_distance_mm);      // [41] RetractHeight
    push_f32_be(out, timing.retract_speed_mm_min);     // [42] RetractSpeed
    push_f32_be(out, timing.bottom_lift_distance2_mm); // [43] BottomLiftHeight2
    push_f32_be(out, timing.bottom_lift_speed2_mm_min); // [44] BottomLiftSpeed2
    push_f32_be(out, timing.lift_distance2_mm);        // [45] LiftHeight2
    push_f32_be(out, timing.lift_speed2_mm_min);       // [46] LiftSpeed2
    push_f32_be(out, timing.bottom_retract_distance2_mm); // [47] BottomRetractHeight2
    push_f32_be(out, timing.bottom_retract_speed2_mm_min); // [48] BottomRetractSpeed2
    push_f32_be(out, timing.retract_distance2_mm);     // [49] RetractHeight2
    push_f32_be(out, timing.retract_speed2_mm_min);    // [50] RetractSpeed2

    // ── End fields (36 bytes) ─────────────────────────────────────────────
    push_u16_be(out, timing.bottom_light_pwm);         // [51]  2 bytes
    push_u16_be(out, timing.light_pwm);                // [52]  2 bytes
    push_u8(out, 1);                                   // [53]  1 byte  PerLayerSettings=true
    push_u32_be(out, print_time_sec);                  // [54]  4 bytes
    push_f32_be(out, 0.0);                             // [55]  4 bytes Volume
    push_f32_be(out, 0.0);                             // [56]  4 bytes MaterialGrams
    push_f32_be(out, 0.0);                             // [57]  4 bytes MaterialCost
    push_str_fixed(out, "$", 8);                       // [58]  8 bytes PriceCurrencySymbol
    push_u32_be(out, GOO_HEADER_SIZE);                 // [59]  4 bytes LayerDefAddress
    push_u8(out, if is_grayscale { 1 } else { 0 });    // [60]  1 byte  GrayScaleLevel
    push_u16_be(out, timing.transition_layer_count);   // [61]  2 bytes TransitionLayerCount
}

//TODO generalize function
fn interpolate_transition_layer_exposure(
    layer: &GooPreparedLayer,
    timing: &GooTimingModel
) -> f32 {
    return timing.bottom_exposure_sec 
        - (timing.bottom_exposure_sec - timing.normal_exposure_sec) 
            / (timing.transition_layer_count as f32) 
            * (layer.index as u32 - timing.bottom_layer_count + 1) as f32;
}

fn write_goo_layer(out: &mut Vec<u8>, layer: &GooPreparedLayer, timing: &GooTimingModel) {
    let (
        exposure, light_off, w_after_cure, w_after_lift, w_before_cure,
        lift_h, lift_s, lift_h2, lift_s2,
        ret_h, ret_s, ret_h2, ret_s2,
        pwm,
    ) = if layer.is_bottom {
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
            if (layer.index as u32) < (timing.bottom_layer_count + timing.transition_layer_count as u32) {
                interpolate_transition_layer_exposure(layer, &timing)
            } else { timing.normal_exposure_sec }, //Layer exposure time
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

    push_u16_be(out, 0);                          // Pause
    push_f32_be(out, 0.0);                         // PausePositionZ
    push_f32_be(out, layer.position_z_mm);         // PositionZ
    push_f32_be(out, exposure);                    // ExposureTime
    push_f32_be(out, light_off);                   // LightOffDelay
    push_f32_be(out, w_after_cure);                // WaitTimeAfterCure
    push_f32_be(out, w_after_lift);                // WaitTimeAfterLift
    push_f32_be(out, w_before_cure);               // WaitTimeBeforeCure
    push_f32_be(out, lift_h);                      // LiftHeight
    push_f32_be(out, lift_s);                      // LiftSpeed
    push_f32_be(out, lift_h2);                     // LiftHeight2
    push_f32_be(out, lift_s2);                     // LiftSpeed2
    push_f32_be(out, ret_h);                       // RetractHeight
    push_f32_be(out, ret_s);                       // RetractSpeed
    push_f32_be(out, ret_h2);                      // RetractHeight2
    push_f32_be(out, ret_s2);                      // RetractSpeed2
    push_u16_be(out, pwm);                         // LightPWM
    push_crlf(out);                                // DelimiterData
    push_u32_be(out, layer.encoded.len() as u32);  // DataLength
    out.extend_from_slice(&layer.encoded);         // RLE data
    push_crlf(out);                                // Post-data delimiter
}
