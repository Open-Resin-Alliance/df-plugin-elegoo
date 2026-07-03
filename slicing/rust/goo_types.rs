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

/// Fixed per-layer definition size (bytes) preceding the RLE payload:
///   Pause(2) + PausePositionZ(4) + PositionZ(4) + ExposureTime(4)
///   + LightOffDelay(4) + WaitTimes(3×4) + Lift/Retract(8×4)
///   + LightPWM(2) + DelimiterData(2) + DataLength(4) = 70
pub(super) const GOO_LAYER_DEF_SIZE: usize = 70;

/// Offset of the post-preview numeric block (LayerCount, ResolutionX/Y, …):
/// pre-preview fields (194) + small preview (26912+2) + large preview (168200+2).
pub(super) const GOO_LAYER_COUNT_OFFSET: u64 = 195_310;

/// Offset of the LayerDefAddress header field ([59]): header end minus
/// LayerDefAddress(4) + GrayScaleLevel(1) + TransitionLayerCount(2).
pub(super) const GOO_LAYER_DEF_ADDRESS_OFFSET: u64 = GOO_HEADER_SIZE as u64 - 7;

// ── GOO V5.1 (Satellite / Jupiter 2) constants — little-endian container ──
// Reference: /root/GOO_v5_Format_Spec.md. Header size is 195,492 (validated
// against real files + live tracing); UVtools PR #1129's 195,496 is wrong.

pub(super) const GOO_V5_MAGIC: &[u8; 4] = b"V5.1";
/// Number of column-partition panels per layer at this architecture.
pub(super) const GOO_V5_PARTITION_COUNT: usize = 2;
/// End of the fixed header incl. previews (start of the printer parameters).
pub(super) const GOO_V5_PARAMS_OFFSET: u32 = 195_310;
/// Start of the 16-byte DRAM preamble (`offset of layer content` points here).
pub(super) const GOO_V5_PREAMBLE_OFFSET: u32 = 195_477;
/// Position of the LDT (T1) magic byte — the validated header size.
pub(super) const GOO_V5_LDT_MARKER: u32 = 195_492;
/// First byte of T1's entries (LDT marker + 1).
pub(super) const GOO_V5_LDT_START: u32 = 195_493;
/// Bytes per 66-byte little-endian layer definition (CRLF included).
pub(super) const GOO_V5_LAYER_DEF_SIZE: usize = 66;
/// Size of the RDT "pad" between T2 and the layer definitions.
pub(super) const GOO_V5_RDT_PAD_SIZE: u32 = 14;
/// Fixed pre-MD5 footer blob length (0x66 + profile + zero pad + f32 1.15 +
/// u32 0 + CRLF), matching the reference files' RDT entry size (0x8E).
pub(super) const GOO_V5_FOOTER_BLOB_SIZE: usize = 142;

#[derive(Debug, Clone)]
pub(super) struct GooV5PreparedLayer {
    pub index: usize,
    /// Framed RLE blocks (`0x55 … checksum CRLF`), left panel then right.
    pub blocks: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GooV5RleMode {
    /// Dense-grayscale/AA grammar (§7.7) — UVtools always encodes with this.
    Vuf,
    /// Binary grammar (§7.2) — kept selectable for hardware A/B testing.
    Binary,
}

#[derive(Debug, Clone)]
pub(super) struct GooV5Settings {
    pub rle_mode: GooV5RleMode,
    /// Preamble byte 14; `max_level = (1 << pixel_bit_width) - 1`.
    pub pixel_bit_width: u8,
    /// Identity strings the printer validates at load (spec §16).
    pub slicer_name: String,
    pub printer_name: String,
    pub printer_type: String,
    /// Must equal the footer profile blob or the printer rejects the file.
    pub profile_name: String,
}

impl GooV5Settings {
    pub fn max_level(&self) -> u8 {
        match self.pixel_bit_width.clamp(1, 8) {
            8 => 255,
            bits => (1u16 << bits) as u8 - 1,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct GooPreparedLayer {
    pub index: usize,
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
    pub machine_z_mm: f32,
}
