use crate::types::SliceJobV3;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

use super::goo_types::{
    GooBuildModel, GooTimingModel, DEFAULT_BINARY_THRESHOLD, DEFAULT_MACHINE_NAME,
    DEFAULT_MACHINE_TYPE, DEFAULT_PROFILE_NAME,
};

fn parse_json(s: &str) -> Option<Value> {
    serde_json::from_str::<Value>(s).ok()
}

fn str_field<'a>(meta: &'a Value, obj: &str, field: &str) -> Option<&'a str> {
    meta.get(obj).and_then(|o| o.get(field)).and_then(Value::as_str)
}

pub(super) fn parse_threshold_from_metadata(metadata_json: &str) -> u8 {
    let Some(meta) = parse_json(metadata_json) else {
        return DEFAULT_BINARY_THRESHOLD;
    };
    meta.get("goo")
        .and_then(|v| v.get("binaryThreshold"))
        .and_then(Value::as_u64)
        .or_else(|| {
            meta.get("material")
                .and_then(|v| v.get("binaryThreshold"))
                .and_then(Value::as_u64)
        })
        .map(|v| v.min(255) as u8)
        .unwrap_or(DEFAULT_BINARY_THRESHOLD)
}

pub(super) fn parse_timing_model_from_metadata(metadata_json: &str) -> GooTimingModel {
    let zero = GooTimingModel {
        normal_exposure_sec: 0.0,
        bottom_exposure_sec: 0.0,
        light_off_delay_sec: 0.0,
        bottom_light_off_delay_sec: 0.0,
        bottom_layer_count: 0,
        lift_distance_mm: 0.0,
        lift_distance2_mm: 0.0,
        lift_speed_mm_min: 0.0,
        lift_speed2_mm_min: 0.0,
        retract_distance_mm: 6.0,
        retract_distance2_mm: 0.0,
        retract_speed_mm_min: 0.0,
        retract_speed2_mm_min: 0.0,
        bottom_lift_distance_mm: 0.0,
        bottom_lift_distance2_mm: 0.0,
        bottom_lift_speed_mm_min: 0.0,
        bottom_lift_speed2_mm_min: 0.0,
        bottom_retract_distance_mm: 6.0,
        bottom_retract_distance2_mm: 0.0,
        bottom_retract_speed_mm_min: 90.0,
        bottom_retract_speed2_mm_min: 0.0,
        wait_time_before_cure_sec: 0.0,
        wait_time_after_cure_sec: 0.0,
        wait_time_after_lift_sec: 0.0,
        bottom_wait_time_before_cure_sec: 0.0,
        bottom_wait_time_after_cure_sec: 0.0,
        bottom_wait_time_after_lift_sec: 0.0,
        transition_layer_count: 0,
        light_pwm: 255,
        bottom_light_pwm: 255,
        delay_mode: 0,
    };

    let Some(meta) = parse_json(metadata_json) else {
        return zero;
    };

    // Goo-specific overrides take precedence over generic material fields.
    let goo = |field: &str| -> Option<f32> {
        meta.get("goo")
            .and_then(|o| o.get(field))
            .and_then(Value::as_f64)
            .map(|v| v as f32)
    };
    let mat = |field: &str| -> Option<f32> {
        meta.get("material")
            .and_then(|o| o.get(field))
            .and_then(Value::as_f64)
            .map(|v| v as f32)
    };
    let goo_u32 = |field: &str| -> Option<u32> {
        meta.get("goo")
            .and_then(|o| o.get(field))
            .and_then(Value::as_u64)
            .map(|v| v as u32)
    };
    let mat_u32 = |field: &str| -> Option<u32> {
        meta.get("material")
            .and_then(|o| o.get(field))
            .and_then(Value::as_u64)
            .map(|v| v as u32)
    };

    let is_simple = meta
        .get("printer")
        .and_then(|o| o.get("settingsMode"))
        .and_then(Value::as_str)
        .map(|s| s.eq_ignore_ascii_case("simple"))
        .unwrap_or(false);

    let is_tilting_mode = meta
        .get("printer")
        .and_then(|o| o.get("settingsMode"))
        .and_then(Value::as_str)
        .map(|s| s.eq_ignore_ascii_case("tilting"))
        .unwrap_or(false);

    let normal_exposure_sec =
        goo("normalExposureSec").or_else(|| mat("normalExposureSec")).unwrap_or(zero.normal_exposure_sec);
    let bottom_exposure_sec =
        goo("bottomExposureSec").or_else(|| mat("bottomExposureSec")).unwrap_or(zero.bottom_exposure_sec);
    let light_off_delay_sec =
        goo("lightOffDelaySec").or_else(|| mat("lightOffDelaySec")).unwrap_or(zero.light_off_delay_sec);
    let bottom_light_off_delay_sec =
        goo("bottomLightOffDelaySec").or_else(|| mat("bottomLightOffDelaySec")).unwrap_or(zero.bottom_light_off_delay_sec);
    let bottom_layer_count =
        goo_u32("bottomLayerCount").or_else(|| mat_u32("bottomLayerCount")).unwrap_or(zero.bottom_layer_count);

    let lift_distance_mm =
        goo("liftDistanceMm").or_else(|| mat("liftDistanceMm")).unwrap_or(zero.lift_distance_mm);
    let lift_speed_mm_min =
        goo("liftSpeedMmMin").or_else(|| mat("liftSpeedMmMin")).unwrap_or(zero.lift_speed_mm_min);
    let retract_distance_mm =
        goo("retractDistanceMm").or_else(|| mat("retractDistanceMm")).unwrap_or(lift_distance_mm);
    let retract_speed_mm_min =
        goo("retractSpeedMmMin").or_else(|| mat("retractSpeedMmMin")).unwrap_or(zero.retract_speed_mm_min);

    let bottom_lift_distance_mm =
        goo("bottomLiftDistanceMm").or_else(|| mat("bottomLiftDistanceMm")).unwrap_or(lift_distance_mm);
    let bottom_lift_speed_mm_min =
        goo("bottomLiftSpeedMmMin").or_else(|| mat("bottomLiftSpeedMmMin")).unwrap_or(zero.bottom_lift_speed_mm_min);
    let bottom_retract_distance_mm =
        goo("bottomRetractDistanceMm").or_else(|| mat("bottomRetractDistanceMm")).unwrap_or(bottom_lift_distance_mm);
    let bottom_retract_speed_mm_min =
        goo("bottomRetractSpeedMmMin").or_else(|| mat("bottomRetractSpeedMmMin")).unwrap_or(zero.bottom_retract_speed_mm_min);

    let (lift_distance2_mm, lift_speed2_mm_min, retract_distance2_mm, retract_speed2_mm_min,
         bottom_lift_distance2_mm, bottom_lift_speed2_mm_min, bottom_retract_distance2_mm, bottom_retract_speed2_mm_min,
         wait_time_before_cure_sec, wait_time_after_cure_sec, wait_time_after_lift_sec,
         bottom_wait_time_before_cure_sec, bottom_wait_time_after_cure_sec, bottom_wait_time_after_lift_sec) =
        if is_simple {
            (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0)
        } else {
            (
                goo("liftDistance2Mm").or_else(|| mat("liftDistance2Mm")).unwrap_or(0.0),
                goo("liftSpeed2MmMin").or_else(|| mat("liftSpeed2MmMin")).unwrap_or(0.0),
                goo("retractDistance2Mm").or_else(|| mat("retractDistance2Mm")).unwrap_or(0.0),
                goo("retractSpeed2MmMin").or_else(|| mat("retractSpeed2MmMin")).unwrap_or(0.0),
                goo("bottomLiftDistance2Mm").or_else(|| mat("bottomLiftDistance2Mm")).unwrap_or(0.0),
                goo("bottomLiftSpeed2MmMin").or_else(|| mat("bottomLiftSpeed2MmMin")).unwrap_or(0.0),
                goo("bottomRetractDistance2Mm").or_else(|| mat("bottomRetractDistance2Mm")).unwrap_or(0.0),
                goo("bottomRetractSpeed2MmMin").or_else(|| mat("bottomRetractSpeed2MmMin")).unwrap_or(0.0),
                goo("waitTimeBeforeCureSec").or_else(|| mat("waitTimeBeforeCureSec")).unwrap_or(0.0),
                goo("waitTimeAfterCureSec").or_else(|| mat("waitTimeAfterCureSec")).unwrap_or(0.0),
                goo("waitTimeAfterLiftSec").or_else(|| mat("waitTimeAfterLiftSec")).unwrap_or(0.0),
                goo("bottomWaitTimeBeforeCureSec").or_else(|| mat("bottomWaitTimeBeforeCureSec")).unwrap_or(0.0),
                goo("bottomWaitTimeAfterCureSec").or_else(|| mat("bottomWaitTimeAfterCureSec")).unwrap_or(0.0),
                goo("bottomWaitTimeAfterLiftSec").or_else(|| mat("bottomWaitTimeAfterLiftSec")).unwrap_or(0.0),
            )
        };

    if is_tilting_mode {
        timing.lift_speed_mm_min, timing.bottom_lift_speed_mm_min = 60.0;
        timing.retract_speed_mm_min, timing.bottom_retract_speed_mm_min = 150.0;
        timing.lift_distance_mm = read_f32("layerHeightMm");
        timing.bottom_lift_distance_mm = read_f32("layerHeightMm");
    }

    let transition_layer_count = goo_u32("transitionLayerCount")
        .or_else(|| mat_u32("transitionLayerCount"))
        .unwrap_or(zero.transition_layer_count as u32) as u16;

    let light_pwm_pct = goo("projectorPwmPercent")
        .or_else(|| mat("projectorPwmPercent"))
        .unwrap_or(100.0);
    let light_pwm = ((light_pwm_pct.clamp(0.0, 100.0) / 100.0) * 255.0).round() as u16;

    let bottom_light_pwm_pct = goo("bottomProjectorPwmPercent")
        .or_else(|| mat("bottomProjectorPwmPercent"))
        .unwrap_or(light_pwm_pct);
    let bottom_light_pwm = ((bottom_light_pwm_pct.clamp(0.0, 100.0) / 100.0) * 255.0).round() as u16;

    // Use WaitTime mode if any wait time is non-zero.
    let delay_mode = if wait_time_before_cure_sec > 0.0
        || wait_time_after_lift_sec > 0.0
        || bottom_wait_time_before_cure_sec > 0.0
        || bottom_wait_time_after_lift_sec > 0.0
    {
        1
    } else {
        0
    };

    GooTimingModel {
        normal_exposure_sec,
        bottom_exposure_sec,
        light_off_delay_sec,
        bottom_light_off_delay_sec,
        bottom_layer_count,
        lift_distance_mm,
        lift_distance2_mm,
        lift_speed_mm_min,
        lift_speed2_mm_min,
        retract_distance_mm,
        retract_distance2_mm,
        retract_speed_mm_min,
        retract_speed2_mm_min,
        bottom_lift_distance_mm,
        bottom_lift_distance2_mm,
        bottom_lift_speed_mm_min,
        bottom_lift_speed2_mm_min,
        bottom_retract_distance_mm,
        bottom_retract_distance2_mm,
        bottom_retract_speed_mm_min,
        bottom_retract_speed2_mm_min,
        wait_time_before_cure_sec,
        wait_time_after_cure_sec,
        wait_time_after_lift_sec,
        bottom_wait_time_before_cure_sec,
        bottom_wait_time_after_cure_sec,
        bottom_wait_time_after_lift_sec,
        transition_layer_count,
        light_pwm,
        bottom_light_pwm,
        delay_mode,
    }
}

pub(super) fn parse_build_model_from_metadata(metadata_json: &str) -> GooBuildModel {
    let Some(meta) = parse_json(metadata_json) else {
        return default_build_model();
    };

    let machine_name = str_field(&meta, "goo", "MachineName")
        .or_else(|| str_field(&meta, "printer", "name"))
        .unwrap_or(DEFAULT_MACHINE_NAME)
        .to_string();

    let machine_type = str_field(&meta, "goo", "MachineType")
        .unwrap_or(DEFAULT_MACHINE_TYPE)
        .to_string();

    let profile_name = str_field(&meta, "goo", "ProfileName")
        .or_else(|| str_field(&meta, "material", "name"))
        .unwrap_or(DEFAULT_PROFILE_NAME)
        .to_string();

    let anti_aliasing_level = meta
        .get("goo")
        .and_then(|o| o.get("antiAliasingLevel"))
        .and_then(Value::as_u64)
        .map(|v| v as u16)
        .unwrap_or(8);

    let grey_level = meta
        .get("goo")
        .and_then(|o| o.get("greyLevel"))
        .and_then(Value::as_u64)
        .map(|v| v as u16)
        .unwrap_or(1);

    let blur_level = meta
        .get("goo")
        .and_then(|o| o.get("blurLevel"))
        .and_then(Value::as_u64)
        .map(|v| v as u16)
        .unwrap_or(0);

    let mirror_x = meta
        .get("goo")
        .and_then(|o| o.get("mirrorX"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mirror_y = meta
        .get("goo")
        .and_then(|o| o.get("mirrorY"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let created_datetime = now_datetime_string();

    GooBuildModel {
        machine_name,
        machine_type,
        profile_name,
        anti_aliasing_level,
        grey_level,
        blur_level,
        mirror_x,
        mirror_y,
        created_datetime,
    }
}

fn default_build_model() -> GooBuildModel {
    GooBuildModel {
        machine_name: DEFAULT_MACHINE_NAME.to_string(),
        machine_type: DEFAULT_MACHINE_TYPE.to_string(),
        profile_name: DEFAULT_PROFILE_NAME.to_string(),
        anti_aliasing_level: 8,
        grey_level: 1,
        blur_level: 0,
        mirror_x: false,
        mirror_y: false,
        created_datetime: now_datetime_string(),
    }
}

fn now_datetime_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    unix_secs_to_datetime_string(secs)
}

fn unix_secs_to_datetime_string(secs: u64) -> String {
    // Minimal ISO-like datetime without chrono. Accurate for years 2000-2099.
    let mut s = secs;
    let sec = s % 60;
    s /= 60;
    let min = s % 60;
    s /= 60;
    let hour = s % 24;
    s /= 24;
    // Days since 1970-01-01
    let days_since_epoch = s;
    let (year, month, day) = days_to_ymd(days_since_epoch as u32);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year, month, day, hour, min, sec
    )
}

fn days_to_ymd(mut days: u32) -> (u32, u32, u32) {
    // Gregorian calendar computation.
    days += 719468;
    let era = days / 146097;
    let doe = days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

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

pub(super) fn parse_goo_build_model_from_job(job: &SliceJobV3) -> GooBuildModel {
    parse_build_model_from_metadata(&job.metadata_json)
}

pub(super) fn parse_timing_model_from_job(job: &SliceJobV3) -> GooTimingModel {
    parse_timing_model_from_metadata(&job.metadata_json)
}

pub(super) fn parse_threshold_from_job(job: &SliceJobV3) -> u8 {
    parse_threshold_from_metadata(&job.metadata_json)
}

pub(super) fn parse_software_info_from_metadata(metadata_json: &str) -> String {
    let Some(meta) = parse_json(metadata_json) else {
        return crate::ENGINE_VERSION.to_string();
    };
    let version = meta
        .get("goo")
        .and_then(|o| o.get("softwareVersion"))
        .and_then(Value::as_str)
        .unwrap_or(crate::ENGINE_VERSION)
        .to_string();
    version
}
