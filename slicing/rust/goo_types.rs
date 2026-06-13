pub(super) const DEFAULT_MACHINE_NAME: &str = "DragonFruit Printer";
pub(super) const DEFAULT_MACHINE_TYPE: &str = "DLP";
pub(super) const DEFAULT_PROFILE_NAME: &str = "DragonFruit Profile";
pub(super) const DEFAULT_BINARY_THRESHOLD: u8 = 127;

pub(super) const GOO_LAYER_MAGIC: u8 = 0x55;
pub(super) const GOO_CRLF: [u8; 2] = [0x0D, 0x0A];
pub(super) const GOO_FILE_VERSION: &[u8; 4] = b"V1.2";
pub(super) const GOO_FILE_MAGIC: [u8; 8] = [0x07, 0x00, 0x00, 0x00, 0x44, 0x4C, 0x50, 0x00];

/// Total fixed header size (bytes). Written into LayerDefAddress; must match the
/// exact number of bytes written by `write_goo_header`.
///
/// Breakdown:
///   Pre-preview:        4+8+32+24+24+32+32+32+2+2+2 = 194
///   Small preview+CRLF: 26912+2                     = 26914
///   Large preview+CRLF: 168200+2                    = 168202
///   Post-preview:       4+2+2+1+1+4+4+4+4+4+1+4    = 35
///   Wait times (6×4):                               = 24
///   BottomExposure+Count:                           = 8
///   Lift/Retract (16×4):                            = 64
///   End fields:         2+2+1+4+4+4+4+8+4+1+2      = 36
pub(super) const GOO_HEADER_SIZE: u32 = 195477;

#[derive(Debug, Clone)]
pub(super) struct GooPreparedLayer {
    pub position_z_mm: f32,
    pub is_bottom: bool,
    /// RLE bytes: 0x55 magic + chunks + one's-complement checksum.
    pub encoded: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct GooTimingModel {
    pub normal_exposure_sec: f32,
    pub bottom_exposure_sec: f32,
    pub light_off_delay_sec: f32,
    pub bottom_light_off_delay_sec: f32,
    pub bottom_layer_count: u32,
    pub lift_distance_mm: f32,
    pub lift_distance2_mm: f32,
    pub lift_speed_mm_min: f32,
    pub lift_speed2_mm_min: f32,
    pub retract_distance_mm: f32,
    pub retract_distance2_mm: f32,
    pub retract_speed_mm_min: f32,
    pub retract_speed2_mm_min: f32,
    pub bottom_lift_distance_mm: f32,
    pub bottom_lift_distance2_mm: f32,
    pub bottom_lift_speed_mm_min: f32,
    pub bottom_lift_speed2_mm_min: f32,
    pub bottom_retract_distance_mm: f32,
    pub bottom_retract_distance2_mm: f32,
    pub bottom_retract_speed_mm_min: f32,
    pub bottom_retract_speed2_mm_min: f32,
    pub wait_time_before_cure_sec: f32,
    pub wait_time_after_cure_sec: f32,
    pub wait_time_after_lift_sec: f32,
    pub bottom_wait_time_before_cure_sec: f32,
    pub bottom_wait_time_after_cure_sec: f32,
    pub bottom_wait_time_after_lift_sec: f32,
    pub transition_layer_count: u16,
    pub light_pwm: u16,
    pub bottom_light_pwm: u16,
    /// 0 = LightOff (use light_off_delay), 1 = WaitTime (use per-stage wait times).
    pub delay_mode: u8,
}

#[derive(Debug, Clone)]
pub(super) struct GooBuildModel {
    pub machine_name: String,
    pub machine_type: String,
    pub profile_name: String,
    pub anti_aliasing_level: u16,
    pub grey_level: u16,
    pub blur_level: u16,
    pub mirror_x: bool,
    pub mirror_y: bool,
    pub created_datetime: String,
}
