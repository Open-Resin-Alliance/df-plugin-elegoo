mod goo_encoder;
mod goo_layout;
mod goo_metadata;
mod goo_preview;
mod goo_types;

use crate::encoders::FormatEncoder;
use crate::encoders::RawMaskStreamEncoder;
use crate::encoders::RleStreamEncoder;
use crate::engine::SlicerV3Error;
use crate::types::{LayerAreaStatsV3, RenderedLayersV3, SliceJobV3};
use crossbeam_channel::bounded;
use goo_encoder::{
    build_goo_container_bytes_with_progress,
    prepare_layers_for_goo_with_progress,
};
use goo_layout::{encode_single_goo_layer_from_raw_mask, push_goo_run};
use goo_metadata::{parse_threshold_from_job, parse_timing_model_from_job};
use goo_types::{GooPreparedLayer, GOO_LAYER_MAGIC};
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

pub struct GooPluginEncoder;

fn choose_goo_encode_threads() -> usize {
    let hw = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    std::env::var("DF_V3_GOO_ENCODE_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v >= 1)
        .unwrap_or(hw)
        .clamp(1, hw)
}

fn cap_goo_encode_workers_for_mask_bytes(requested: usize, expected_pixels: usize) -> usize {
    let mut capped = requested.max(1);
    if let Some(budget_bytes) = std::env::var("DF_V3_MAX_GOO_INFLIGHT_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v >= 64)
        .map(|mb| mb.saturating_mul(1024 * 1024))
    {
        capped = capped.min((budget_bytes / expected_pixels.max(1)).max(1));
    } else {
        if expected_pixels >= 48 * 1024 * 1024 {
            capped = capped.min(2);
        } else if expected_pixels >= 24 * 1024 * 1024 {
            capped = capped.min(4);
        } else if expected_pixels >= 12 * 1024 * 1024 {
            capped = capped.min(8);
        }
    }
    capped.max(1)
}

fn choose_goo_encode_queue_depth(worker_count: usize, expected_pixels: usize) -> usize {
    if let Some(budget_bytes) = std::env::var("DF_V3_MAX_GOO_INFLIGHT_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v >= 64)
        .map(|mb| mb.saturating_mul(1024 * 1024))
    {
        return ((budget_bytes / expected_pixels.max(1)).max(1))
            .min(worker_count.saturating_mul(2).max(1));
    }
    if expected_pixels >= 48 * 1024 * 1024 {
        1
    } else if expected_pixels >= 24 * 1024 * 1024 {
        2
    } else if expected_pixels >= 12 * 1024 * 1024 {
        3
    } else {
        (worker_count.saturating_mul(3)).clamp(3, 24)
    }
}

// ── Parallel raw-mask streaming encoder ──────────────────────────────────────

struct GooRawMaskStreamingEncoder {
    job: SliceJobV3,
    work_tx: Option<crossbeam_channel::Sender<(u32, Vec<u8>)>>,
    result_rx: mpsc::Receiver<Result<GooPreparedLayer, SlicerV3Error>>,
    workers: Vec<thread::JoinHandle<()>>,
    consumed_layers: u32,
}

impl RawMaskStreamEncoder for GooRawMaskStreamingEncoder {
    fn consume_raw_mask_layer(
        &mut self,
        layer_index: u32,
        raw_mask: Vec<u8>,
    ) -> Result<(), SlicerV3Error> {
        let Some(ref tx) = self.work_tx else {
            return Err(SlicerV3Error::MissingRenderedLayerPayload(
                "Goo streaming encoder no longer accepts layers after finalize".to_string(),
            ));
        };
        tx.send((layer_index, raw_mask)).map_err(|_| {
            SlicerV3Error::MissingRenderedLayerPayload(
                "Goo streaming worker channel closed unexpectedly".to_string(),
            )
        })?;
        self.consumed_layers = self.consumed_layers.saturating_add(1);
        Ok(())
    }

    fn finalize_to_bytes(mut self: Box<Self>) -> Result<Vec<u8>, SlicerV3Error> {
        if self.consumed_layers == 0 {
            return Err(SlicerV3Error::MissingRenderedLayerPayload(
                "no rendered layers were provided for Goo encoding".to_string(),
            ));
        }

        let _ = self.work_tx.take();

        while let Some(handle) = self.workers.pop() {
            if handle.join().is_err() {
                return Err(SlicerV3Error::UnsupportedOutput(
                    "Goo streaming worker panicked".to_string(),
                ));
            }
        }

        let expected_layers = self.consumed_layers as usize;
        let mut ordered: Vec<Option<GooPreparedLayer>> = Vec::with_capacity(expected_layers);
        ordered.resize_with(expected_layers, || None);

        for _ in 0..expected_layers {
            let prepared = self.result_rx.recv().map_err(|_| {
                SlicerV3Error::MissingRenderedLayerPayload(
                    "Goo streaming worker results ended unexpectedly".to_string(),
                )
            })??;
            if prepared.index >= expected_layers {
                return Err(SlicerV3Error::MissingRenderedLayerPayload(format!(
                    "Goo worker emitted out-of-range layer index {} (expected < {})",
                    prepared.index, expected_layers
                )));
            }
            let index = prepared.index;
            if ordered[index].is_some() {
                return Err(SlicerV3Error::MissingRenderedLayerPayload(format!(
                    "Goo worker emitted duplicate layer index {}",
                    index
                )));
            }
            ordered[index] = Some(prepared);
        }

        let mut prepared = Vec::with_capacity(expected_layers);
        for (index, layer) in ordered.into_iter().enumerate() {
            let Some(layer) = layer else {
                return Err(SlicerV3Error::MissingRenderedLayerPayload(format!(
                    "Goo layer {} missing from streaming worker output",
                    index
                )));
            };
            prepared.push(layer);
        }

        build_goo_container_bytes_with_progress(&self.job, &prepared, None)
    }
}

// ── Sequential RLE streaming encoder ─────────────────────────────────────────

struct GooRleStreamingEncoder {
    job: SliceJobV3,
    is_anti_aliased: bool,
    threshold: u8,
    layer_height_mm: f32,
    bottom_layer_count: u32,
    total_pixels: usize,
    prepared: Vec<GooPreparedLayer>,
}

impl RleStreamEncoder for GooRleStreamingEncoder {
    fn consume_rle_layer(
        &mut self,
        layer_index: u32,
        runs: Vec<crate::rle::RleRun>,
    ) -> Result<(), SlicerV3Error> {
        let encoded = encode_goo_rle_from_runs(&runs, self.is_anti_aliased, self.threshold, self.total_pixels);
        self.prepared.push(GooPreparedLayer {
            index: layer_index as usize,
            position_z_mm: (layer_index as f32 + 1.0) * self.layer_height_mm,
            is_bottom: layer_index < self.bottom_layer_count,
            encoded,
        });
        Ok(())
    }

    fn finalize_to_bytes(mut self: Box<Self>) -> Result<Vec<u8>, SlicerV3Error> {
        if self.prepared.is_empty() {
            return Err(SlicerV3Error::MissingRenderedLayerPayload(
                "no rendered layers were provided for Goo RLE encoding".to_string(),
            ));
        }
        self.prepared.sort_unstable_by_key(|p| p.index);
        build_goo_container_bytes_with_progress(&self.job, &self.prepared, None)
    }

    fn parallel_encode_fn(
        &self,
    ) -> Option<
        Arc<dyn Fn(u32, &[crate::rle::RleRun]) -> Result<Vec<u8>, SlicerV3Error> + Send + Sync>,
    > {
        let is_anti_aliased = self.is_anti_aliased;
        let threshold = self.threshold;
        let total_pixels = self.total_pixels;
        Some(Arc::new(move |_layer_index: u32, runs: &[crate::rle::RleRun]| {
            Ok(encode_goo_rle_from_runs(runs, is_anti_aliased, threshold, total_pixels))
        }))
    }

    fn store_encoded_layer(&mut self, layer_index: u32, bytes: Vec<u8>) {
        self.prepared.push(GooPreparedLayer {
            index: layer_index as usize,
            position_z_mm: (layer_index as f32 + 1.0) * self.layer_height_mm,
            is_bottom: layer_index < self.bottom_layer_count,
            encoded: bytes,
        });
    }
}

fn encode_goo_rle_from_runs(
    runs: &[crate::rle::RleRun],
    is_anti_aliased: bool,
    threshold: u8,
    total_pixels: usize,
) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(runs.len() * 3 + 16);
    encoded.push(GOO_LAYER_MAGIC);

    if runs.is_empty() {
        push_goo_run(&mut encoded, 0x00, total_pixels as u32, 0x00);
    } else {
        for run in runs {
            let value = if is_anti_aliased {
                run.value
            } else if run.value > threshold {
                0xFF
            } else {
                0x00
            };
            let chunk_type = match value {
                0x00 => 0x00,
                0xFF => 0x03,
                _ => 0x01,
            };
            push_goo_run(&mut encoded, chunk_type, run.length, value);
        }
    }

    let sum: u8 = encoded[1..].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    encoded.push(!sum);
    encoded
}

// ── Plugin entry point ────────────────────────────────────────────────────────

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

    fn create_raw_mask_stream_encoder(
        &self,
        job: &SliceJobV3,
    ) -> Result<Option<Box<dyn RawMaskStreamEncoder>>, SlicerV3Error> {
        let timing = parse_timing_model_from_job(job);
        let threshold = parse_threshold_from_job(job);
        let is_anti_aliased = job.produces_grayscale_output();
        let expected_pixels =
            (job.source_width_px as usize).saturating_mul(job.source_height_px as usize);

        let worker_count =
            cap_goo_encode_workers_for_mask_bytes(choose_goo_encode_threads(), expected_pixels);
        let queue_depth = choose_goo_encode_queue_depth(worker_count, expected_pixels);
        let (work_tx, work_rx) = bounded::<(u32, Vec<u8>)>(queue_depth);
        let (result_tx, result_rx) = mpsc::channel::<Result<GooPreparedLayer, SlicerV3Error>>();
        let mut workers = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let work_rx = work_rx.clone();
            let result_tx = result_tx.clone();
            let worker_threshold = threshold;
            let worker_is_anti_aliased = is_anti_aliased;
            let worker_layer_height_mm = job.layer_height_mm;
            let worker_bottom_layer_count = timing.bottom_layer_count;
            let worker_expected_pixels = expected_pixels;

            let handle = thread::spawn(move || loop {
                let Ok((layer_index, raw_mask)) = work_rx.recv() else {
                    break;
                };

                if raw_mask.len() != worker_expected_pixels {
                    let len = raw_mask.len();
                    crate::pipeline::return_mask_to_pool(raw_mask);
                    let _ = result_tx.send(Err(SlicerV3Error::MissingRenderedLayerPayload(
                        format!(
                            "Goo layer {layer_index} size mismatch: expected {} bytes, got {}",
                            worker_expected_pixels, len
                        ),
                    )));
                    continue;
                }

                let prepared = encode_single_goo_layer_from_raw_mask(
                    layer_index as usize,
                    &raw_mask,
                    worker_is_anti_aliased,
                    worker_threshold,
                    worker_layer_height_mm,
                    worker_bottom_layer_count,
                );
                crate::pipeline::return_mask_to_pool(raw_mask);

                if result_tx.send(Ok(prepared)).is_err() {
                    break;
                }
            });

            workers.push(handle);
        }
        drop(result_tx);

        Ok(Some(Box::new(GooRawMaskStreamingEncoder {
            job: job.clone(),
            work_tx: Some(work_tx),
            result_rx,
            workers,
            consumed_layers: 0,
        })))
    }

    fn create_rle_stream_encoder(
        &self,
        job: &SliceJobV3,
    ) -> Result<Option<Box<dyn RleStreamEncoder>>, SlicerV3Error> {
        let timing = parse_timing_model_from_job(job);
        let threshold = parse_threshold_from_job(job);
        let is_anti_aliased = job.produces_grayscale_output();
        let total_pixels =
            (job.source_width_px as usize).saturating_mul(job.source_height_px as usize);
        Ok(Some(Box::new(GooRleStreamingEncoder {
            job: job.clone(),
            is_anti_aliased,
            threshold,
            layer_height_mm: job.layer_height_mm,
            bottom_layer_count: timing.bottom_layer_count,
            total_pixels,
            prepared: Vec::with_capacity(job.total_layers as usize),
        })))
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
            .ok_or_else(|| {
                SlicerV3Error::MissingRenderedLayerPayload(
                    "raw mask layers are required for Goo encoding".to_string(),
                )
            })?;

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

        let total_prepare = raw_masks.len() as u32;
        let total_progress = total_prepare.saturating_add(1).max(1);

        let prepare_progress = on_progress.map(|progress| {
            move |done: u32, total: u32| {
                let mapped = ((done.min(total.max(1)) as u64) * (total_prepare as u64)
                    / (total.max(1) as u64)) as u32;
                progress(mapped, total_progress);
            }
        });

        let prepared = prepare_layers_for_goo_with_progress(
            raw_masks,
            is_anti_aliased,
            threshold,
            job.layer_height_mm,
            timing.bottom_layer_count,
            prepare_progress.as_ref().map(|cb| cb as &dyn Fn(u32, u32)),
        );

        let bytes = build_goo_container_bytes_with_progress(job, &prepared, None)?;

        if bytes.is_empty() {
            return Err(SlicerV3Error::UnsupportedOutput(
                "Goo encoding produced empty payload".to_string(),
            ));
        }

        if let Some(progress) = on_progress {
            progress(total_progress, total_progress);
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
