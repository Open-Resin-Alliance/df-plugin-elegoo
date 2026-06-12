mod goo_encoder;
mod goo_layout;
mod goo_metadata;
mod goo_preview;
mod goo_types;

use crate::encoders::FormatEncoder;
use crate::engine::SlicerV3Error;
use crate::types::{LayerAreaStatsV3, RenderedLayersV3, SliceJobV3};
use goo_encoder::{build_goo_container_bytes_with_progress, prepare_layers_for_goo};
use goo_metadata::{parse_threshold_from_job, parse_timing_model_from_job};
use std::path::Path;

pub struct GooPluginEncoder;

pub fn create_plugin_encoder() -> Vec<Box<dyn FormatEncoder>> {
    vec![Box::new(GooPluginEncoder)]
}

impl FormatEncoder for GooPluginEncoder {
    fn output_format(&self) -> &'static str {
        ".goo"
    }

    fn requires_png_layers(&self) -> bool {
        false
    }

    fn requires_raw_mask_layers(&self) -> bool {
        true
    }

    fn estimate_encode_progress_units(&self, rendered_layers: &RenderedLayersV3) -> u32 {
        rendered_layers
            .raw_mask_layers
            .as_ref()
            .map(|v| v.len() as u32)
            .unwrap_or(0)
            .saturating_add(1)
            .max(1)
    }

    fn encode_container_from_rendered_layers_with_progress(
        &self,
        job: &SliceJobV3,
        rendered_layers: &RenderedLayersV3,
        _layer_area_stats: &[LayerAreaStatsV3],
        on_progress: Option<&dyn Fn(u32, u32)>,
    ) -> Result<Vec<u8>, SlicerV3Error> {
        let raw_masks = rendered_layers
            .raw_mask_layers
            .as_ref()
            .ok_or_else(|| SlicerV3Error::MissingRenderedLayerPayload(
                "raw mask layers are required for Goo encoding".to_string(),
            ))?;

        if raw_masks.is_empty() {
            return Err(SlicerV3Error::MissingRenderedLayerPayload(
                "no rendered layers were provided for Goo encoding".to_string(),
            ));
        }

        let expected_pixels =
            (job.source_width_px as usize).saturating_mul(job.source_height_px as usize);
        for (idx, layer) in raw_masks.iter().enumerate() {
            if layer.len() != expected_pixels {
                return Err(SlicerV3Error::MissingRenderedLayerPayload(format!(
                    "Goo layer {idx} size mismatch: expected {expected_pixels} bytes, got {}",
                    layer.len()
                )));
            }
        }

        let timing = parse_timing_model_from_job(job);
        let threshold = parse_threshold_from_job(job);
        let is_anti_aliased = job.produces_grayscale_output();

        let prepared = prepare_layers_for_goo(
            raw_masks,
            is_anti_aliased,
            threshold,
            job.layer_height_mm,
            timing.bottom_layer_count,
        );

        let bytes = build_goo_container_bytes_with_progress(job, &prepared, on_progress)?;

        if bytes.is_empty() {
            return Err(SlicerV3Error::UnsupportedOutput(
                "Goo encoding produced empty payload".to_string(),
            ));
        }

        Ok(bytes)
    }

    fn encode_container_from_rendered_layers(
        &self,
        job: &SliceJobV3,
        rendered_layers: &RenderedLayersV3,
        layer_area_stats: &[LayerAreaStatsV3],
    ) -> Result<Vec<u8>, SlicerV3Error> {
        self.encode_container_from_rendered_layers_with_progress(
            job,
            rendered_layers,
            layer_area_stats,
            None,
        )
    }

    fn encode_container_to_path(
        &self,
        job: &SliceJobV3,
        rendered_layers: &RenderedLayersV3,
        layer_area_stats: &[LayerAreaStatsV3],
        output_path: &Path,
    ) -> Result<(), SlicerV3Error> {
        let bytes =
            self.encode_container_from_rendered_layers(job, rendered_layers, layer_area_stats)?;
        std::fs::write(output_path, bytes)?;
        Ok(())
    }
}
