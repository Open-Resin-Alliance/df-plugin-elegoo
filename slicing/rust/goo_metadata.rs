use crate::types::SliceJobV3;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

use super::goo_types::{
    GooBuildModel, GooTimingModel, DEFAULT_BINARY_THRESHOLD, DEFAULT_MACHINE_NAME,
    DEFAULT_MACHINE_TYPE, DEFAULT_PROFILE_NAME,
};

const DEFAULT_MACHINE_Z_MM: f32 = 220.0;

fn parse_json(metadata_json: &str) -> Option<Value> {
    serde_json::from_str::<Value>(metadata_json).ok()
}

fn str_field<'a>(meta: &'a Value, obj: &str, field: &str) -> Option<&'a str> {
    meta.get(obj).and_then(|o| o.get(field)).and_then(Value::as_str)
}

pub(super) fn parse_threshold_from_metadata(metadata_json: &str) -> u8 {
    let Some(meta) = parse_json(metadata_json) else {
        return DEFAULT_BINARY_THRESHOLD;
    };

    let direct = meta
        .get("goo")
        .and_then(|v| v.get("binaryThreshold"))
        .and_then(Value::as_u64);

    let nested = meta
        .get("export")
        .and_then(|v| v.get("goo"))
        .and_then(|v| v.get("binaryThreshold"))
        .and_then(Value::as_u64);

    let material = meta
        .get("material")
        .and_then(|v| v.get("binaryThreshold"))
        .and_then(Value::as_u64);

    direct
        .or(nested)
        .or(material)
        .map(|v| v.min(255) as u8)
        .unwrap_or(DEFAULT_BINARY_THRESHOLD)
}

pub(super) fn parse_timing_model_from_metadata(metadata_json: &str) -> GooTimingModel {
    let Some(meta) = parse_json(metadata_json) else {
        return GooTimingModel {
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
    };

    let material = meta.get("material").and_then(Value::as_object);
    let goo = meta.get("goo").and_then(Value::as_object);
    let export_goo = meta
        .get("export")
        .and_then(|v| v.get("goo"))
        .and_then(Value::as_object);
    // The differential material settings contract shares field definitions
    // with the CTB plugin, so some GOO fields arrive under the `ctb` object
    // (see plugins/elegoo/materialSettings/*.json). Read those as fallbacks.
    let ctb = meta.get("ctb").and_then(Value::as_object);
    let export_ctb = meta
        .get("export")
        .and_then(|v| v.get("ctb"))
        .and_then(Value::as_object);

    let settings_mode = goo
        .and_then(|m| m.get("settingsMode"))
        .and_then(Value::as_str)
        .or_else(|| goo.and_then(|m| m.get("mode")).and_then(Value::as_str))
        .or_else(|| {
            export_goo
                .and_then(|m| m.get("settingsMode"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            export_goo
                .and_then(|m| m.get("mode"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            meta.get("printer")
                .and_then(Value::as_object)
                .and_then(|m| m.get("settingsMode"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            meta.get("printer")
                .and_then(Value::as_object)
                .and_then(|m| m.get("mode"))
                .and_then(Value::as_str)
        })
        .map(|v| v.trim().to_ascii_lowercase());
    let is_simple_mode = matches!(settings_mode.as_deref(), Some("simple"));
    let is_tilting_mode = matches!(settings_mode.as_deref(), Some("tilting"));
    // "twostage" (and any unknown mode) keeps every parsed field active.

    let read_f32_opt = |key: &str| -> Option<f32> {
        goo.and_then(|m| m.get(key))
            .and_then(Value::as_f64)
            .or_else(|| export_goo.and_then(|m| m.get(key)).and_then(Value::as_f64))
            .or_else(|| ctb.and_then(|m| m.get(key)).and_then(Value::as_f64))
            .or_else(|| export_ctb.and_then(|m| m.get(key)).and_then(Value::as_f64))
            .or_else(|| material.and_then(|m| m.get(key)).and_then(Value::as_f64))
            .map(|v| v as f32)
    };
    let read_f32 = |key: &str| read_f32_opt(key).unwrap_or(0.0);
    let read_u32 = |key: &str| {
        goo.and_then(|m| m.get(key))
            .and_then(Value::as_u64)
            .or_else(|| export_goo.and_then(|m| m.get(key)).and_then(Value::as_u64))
            .or_else(|| ctb.and_then(|m| m.get(key)).and_then(Value::as_u64))
            .or_else(|| export_ctb.and_then(|m| m.get(key)).and_then(Value::as_u64))
            .or_else(|| material.and_then(|m| m.get(key)).and_then(Value::as_u64))
            .unwrap_or(0) as u32
    };

    let light_pwm_pct = read_f32_opt("projectorPwmPercent").unwrap_or(100.0);
    let bottom_light_pwm_pct = read_f32_opt("bottomProjectorPwmPercent")
        .or_else(|| read_f32_opt("bottomLayerProjectorPwmPercent"))
        .unwrap_or(light_pwm_pct);
    let pwm_byte = |pct: f32| ((pct.clamp(0.0, 100.0) / 100.0) * 255.0).round() as u16;

    let mut timing = GooTimingModel {
        normal_exposure_sec: read_f32("normalExposureSec"),
        bottom_exposure_sec: read_f32("bottomExposureSec"),
        light_off_delay_sec: read_f32("lightOffDelaySec"),
        bottom_light_off_delay_sec: read_f32("bottomLightOffDelaySec"),
        bottom_layer_count: read_u32("bottomLayerCount"),
        lift_distance_mm: read_f32("liftDistanceMm"),
        lift_distance2_mm: read_f32("liftDistance2Mm"),
        lift_speed_mm_min: read_f32("liftSpeedMmMin"),
        lift_speed2_mm_min: read_f32("liftSpeed2MmMin"),
        retract_distance_mm: read_f32("retractDistanceMm"),
        retract_distance2_mm: read_f32("retractDistance2Mm"),
        retract_speed_mm_min: read_f32("retractSpeedMmMin"),
        retract_speed2_mm_min: read_f32("retractSpeed2MmMin"),
        bottom_lift_distance_mm: read_f32("bottomLiftDistanceMm"),
        bottom_lift_distance2_mm: read_f32("bottomLiftDistance2Mm"),
        bottom_lift_speed_mm_min: read_f32("bottomLiftSpeedMmMin"),
        bottom_lift_speed2_mm_min: read_f32("bottomLiftSpeed2MmMin"),
        bottom_retract_distance_mm: read_f32("bottomRetractDistanceMm"),
        bottom_retract_distance2_mm: read_f32("bottomRetractDistance2Mm"),
        bottom_retract_speed_mm_min: read_f32("bottomRetractSpeedMmMin"),
        bottom_retract_speed2_mm_min: read_f32("bottomRetractSpeed2MmMin"),
        wait_time_before_cure_sec: read_f32("waitTimeBeforeCureSec"),
        wait_time_after_cure_sec: read_f32("waitTimeAfterCureSec"),
        wait_time_after_lift_sec: read_f32("waitTimeAfterLiftSec"),
        bottom_wait_time_before_cure_sec: read_f32("bottomWaitTimeBeforeCureSec"),
        bottom_wait_time_after_cure_sec: read_f32("bottomWaitTimeAfterCureSec"),
        bottom_wait_time_after_lift_sec: read_f32("bottomWaitTimeAfterLiftSec"),
        transition_layer_count: read_u32("transitionLayerCount").min(u16::MAX as u32) as u16,
        light_pwm: pwm_byte(light_pwm_pct),
        bottom_light_pwm: pwm_byte(bottom_light_pwm_pct),
        delay_mode: 1,
    };

    let sanitize_non_negative = |value: f32| {
        if !value.is_finite() || value <= 0.0 {
            0.0
        } else {
            value
        }
    };

    timing.normal_exposure_sec = sanitize_non_negative(timing.normal_exposure_sec);
    timing.bottom_exposure_sec = sanitize_non_negative(timing.bottom_exposure_sec);
    timing.light_off_delay_sec = sanitize_non_negative(timing.light_off_delay_sec);
    timing.bottom_light_off_delay_sec = sanitize_non_negative(timing.bottom_light_off_delay_sec);
    timing.lift_distance_mm = sanitize_non_negative(timing.lift_distance_mm);
    timing.lift_distance2_mm = sanitize_non_negative(timing.lift_distance2_mm);
    timing.lift_speed_mm_min = sanitize_non_negative(timing.lift_speed_mm_min);
    timing.lift_speed2_mm_min = sanitize_non_negative(timing.lift_speed2_mm_min);
    timing.retract_distance_mm = sanitize_non_negative(timing.retract_distance_mm);
    timing.retract_distance2_mm = sanitize_non_negative(timing.retract_distance2_mm);
    timing.retract_speed_mm_min = sanitize_non_negative(timing.retract_speed_mm_min);
    timing.retract_speed2_mm_min = sanitize_non_negative(timing.retract_speed2_mm_min);
    timing.bottom_lift_distance_mm = sanitize_non_negative(timing.bottom_lift_distance_mm);
    timing.bottom_lift_distance2_mm = sanitize_non_negative(timing.bottom_lift_distance2_mm);
    timing.bottom_lift_speed_mm_min = sanitize_non_negative(timing.bottom_lift_speed_mm_min);
    timing.bottom_lift_speed2_mm_min = sanitize_non_negative(timing.bottom_lift_speed2_mm_min);
    timing.bottom_retract_distance_mm = sanitize_non_negative(timing.bottom_retract_distance_mm);
    timing.bottom_retract_distance2_mm = sanitize_non_negative(timing.bottom_retract_distance2_mm);
    timing.bottom_retract_speed_mm_min = sanitize_non_negative(timing.bottom_retract_speed_mm_min);
    timing.bottom_retract_speed2_mm_min =
        sanitize_non_negative(timing.bottom_retract_speed2_mm_min);
    timing.wait_time_before_cure_sec = sanitize_non_negative(timing.wait_time_before_cure_sec);
    timing.wait_time_after_cure_sec = sanitize_non_negative(timing.wait_time_after_cure_sec);
    timing.wait_time_after_lift_sec = sanitize_non_negative(timing.wait_time_after_lift_sec);
    timing.bottom_wait_time_before_cure_sec =
        sanitize_non_negative(timing.bottom_wait_time_before_cure_sec);
    timing.bottom_wait_time_after_cure_sec =
        sanitize_non_negative(timing.bottom_wait_time_after_cure_sec);
    timing.bottom_wait_time_after_lift_sec =
        sanitize_non_negative(timing.bottom_wait_time_after_lift_sec);

    if timing.light_pwm == 0 {
        timing.light_pwm = 255;
    }
    if timing.bottom_light_pwm == 0 {
        timing.bottom_light_pwm = 255;
    }

    if timing.retract_distance_mm <= 0.0 {
        timing.retract_distance_mm = timing.lift_distance_mm;
    }
    if timing.bottom_lift_distance_mm <= 0.0 {
        timing.bottom_lift_distance_mm = timing.lift_distance_mm;
    }
    if timing.bottom_lift_speed_mm_min <= 0.0 {
        timing.bottom_lift_speed_mm_min = timing.lift_speed_mm_min;
    }
    if timing.bottom_retract_distance_mm <= 0.0 {
        timing.bottom_retract_distance_mm = timing.bottom_lift_distance_mm;
    }
    if timing.bottom_retract_speed_mm_min <= 0.0 {
        timing.bottom_retract_speed_mm_min = timing.retract_speed_mm_min;
    }

    if is_simple_mode {
        timing.lift_distance2_mm = 0.0;
        timing.lift_speed2_mm_min = 0.0;
        timing.retract_distance2_mm = 0.0;
        timing.retract_speed2_mm_min = 0.0;
        timing.bottom_lift_distance2_mm = 0.0;
        timing.bottom_lift_speed2_mm_min = 0.0;
        timing.bottom_retract_distance2_mm = 0.0;
        timing.bottom_retract_speed2_mm_min = 0.0;

        // Simple mode has no bottom-specific wait overrides.
        timing.bottom_wait_time_before_cure_sec = timing.wait_time_before_cure_sec;
        timing.bottom_wait_time_after_cure_sec = timing.wait_time_after_cure_sec;
        timing.bottom_wait_time_after_lift_sec = timing.wait_time_after_lift_sec;
    }

    if is_tilting_mode {
        timing.lift_speed_mm_min = 60.0;
        timing.bottom_lift_speed_mm_min = 60.0;
        timing.retract_speed_mm_min = 150.0;
        timing.bottom_retract_speed_mm_min = 150.0;
        timing.lift_distance_mm = read_f32("layerHeightMm");
        timing.bottom_lift_distance_mm = read_f32("layerHeightMm");
    }

    // GOO DelayMode: 0 = light-off delay, 1 = per-stage wait times.
    timing.delay_mode = if timing.wait_time_before_cure_sec > 0.0
        || timing.wait_time_after_cure_sec > 0.0
        || timing.wait_time_after_lift_sec > 0.0
        || timing.bottom_wait_time_before_cure_sec > 0.0
        || timing.bottom_wait_time_after_cure_sec > 0.0
        || timing.bottom_wait_time_after_lift_sec > 0.0
    {
        1
    } else {
        0
    };

    timing
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

    let machine_z_mm = meta
        .get("printer")
        .and_then(|o| o.get("buildVolumeMm"))
        .and_then(|o| o.get("height"))
        .and_then(Value::as_f64)
        .map(|v| v as f32)
        .unwrap_or(DEFAULT_MACHINE_Z_MM);

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
        machine_z_mm,
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
        machine_z_mm: DEFAULT_MACHINE_Z_MM,
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
    meta.get("goo")
        .and_then(|o| o.get("softwareVersion"))
        .and_then(Value::as_str)
        .unwrap_or(crate::ENGINE_VERSION)
        .to_string()
}
