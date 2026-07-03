// GOO V1.2 container assembly — a single fixed big-endian header, inline
// per-layer records with CRLF delimiters, and an 11-byte footer.
// Reference: UVtools GooFile (https://github.com/sn4k3/UVtools).
use crate::engine::SlicerV3Error;
use crate::types::SliceJobV3;

use super::goo_layout::{
    push_crlf, push_f32_be, push_str_fixed, push_u16_be, push_u32_be, push_u8,
};
use super::goo_metadata::{
    parse_goo_build_model_from_job, parse_software_info_from_metadata,
    parse_timing_model_from_job,
};
use super::goo_preview::build_goo_previews;
use super::goo_types::{
    GooBuildModel, GooPreparedLayer, GooTimingModel, GOO_FILE_MAGIC, GOO_FILE_VERSION,
    GOO_HEADER_SIZE, GOO_LAYER_DEF_SIZE,
};

pub(super) fn compute_print_time_seconds(total_layers: usize, timing: &GooTimingModel) -> u32 {
    let bottom_count = timing.bottom_layer_count as usize;
    let normal_count = total_layers.saturating_sub(bottom_count);

    let lift_mm_s = |mm_min: f32| if mm_min > 0.0 { mm_min / 60.0 } else { 1.0 };
    let travel_sec = |dist: f32, speed: f32| dist / lift_mm_s(speed);

    let bottom_per_layer = timing.bottom_exposure_sec
        + travel_sec(timing.bottom_lift_distance_mm, timing.bottom_lift_speed_mm_min)
        + timing.bottom_wait_time_after_lift_sec
        + travel_sec(timing.bottom_retract_distance_mm, timing.bottom_retract_speed_mm_min)
        + timing.bottom_wait_time_before_cure_sec
        + timing.bottom_wait_time_after_cure_sec;

    let normal_per_layer = timing.normal_exposure_sec
        + travel_sec(timing.lift_distance_mm, timing.lift_speed_mm_min)
        + timing.wait_time_after_lift_sec
        + travel_sec(timing.retract_distance_mm, timing.retract_speed_mm_min)
        + timing.wait_time_before_cure_sec
        + timing.wait_time_after_cure_sec;

    let total = (bottom_count as f32) * bottom_per_layer + (normal_count as f32) * normal_per_layer;
    total.round().max(0.0) as u32
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
    let print_time_sec = compute_print_time_seconds(prepared.len(), &timing);
    let is_grayscale = job.produces_grayscale_output();

    let mut out = Vec::with_capacity(GOO_HEADER_SIZE as usize + prepared.len() * 512);

    write_goo_header(
        &mut out,
        job,
        &timing,
        &build,
        &software_version,
        &previews.small,
        &previews.large,
        layer_count,
        print_time_sec,
        is_grayscale,
    );

    assert_eq!(out.len(), GOO_HEADER_SIZE as usize);

    let total_prepared = prepared.len() as u32;
    for (idx, layer) in prepared.iter().enumerate() {
        write_goo_layer(&mut out, layer, &timing);
        if let Some(progress) = on_progress {
            progress((idx as u32) + 1, total_prepared.max(1));
        }
    }

    // Footer: three padding bytes then the file magic (11 bytes total).
    push_u8(&mut out, 0);
    push_u8(&mut out, 0);
    push_u8(&mut out, 0);
    out.extend_from_slice(&GOO_FILE_MAGIC);

    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn write_goo_header(
    out: &mut Vec<u8>,
    job: &SliceJobV3,
    timing: &GooTimingModel,
    build: &GooBuildModel,
    software_version: &str,
    small_preview: &[u8],
    large_preview: &[u8],
    layer_count: u32,
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
    push_u16_be(out, job.source_width_px as u16);      // [16]  2 bytes
    push_u16_be(out, job.source_height_px as u16);     // [17]  2 bytes
    push_u8(out, build.mirror_x as u8);                // [18]  1 byte
    push_u8(out, build.mirror_y as u8);                // [19]  1 byte
    push_f32_be(out, job.build_width_mm);              // [20]  4 bytes
    push_f32_be(out, job.build_depth_mm);              // [21]  4 bytes
    push_f32_be(out, build.machine_z_mm);              // [22]  4 bytes
    push_f32_be(out, job.layer_height_mm);             // [23]  4 bytes
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

    debug_assert_eq!(out.len() - def_start, GOO_LAYER_DEF_SIZE);

    out.extend_from_slice(&layer.encoded);         // RLE data
    push_crlf(out);                                // Post-data delimiter
}
