mod goo_layout;
mod goo_metadata;
mod goo_preview;
mod goo_types;
mod goo_encoder;
mod goo_v5;
mod goo_v5_rle;

use crate::encoders::FormatEncoder;
use crate::encoders::RawMaskStreamEncoder;
use crate::encoders::RleStreamEncoder;
use crate::engine::SlicerV3Error;
use crate::types::{LayerAreaStatsV3, RenderedLayersV3, SliceJobV3};
use crossbeam_channel::bounded;
use goo_layout::{
    build_goo_container_bytes, build_goo_container_bytes_with_progress,
    encode_goo_rle_from_runs, encode_single_goo_empty_layer,
    encode_single_goo_layer_from_raw_mask, prepare_layers_for_goo_with_progress,
};
use goo_metadata::{
    parse_goo_format_version_hint_from_job, parse_goo_v5_settings_from_job,
    parse_threshold_from_job, parse_timing_model_from_job, GooFormatVersion,
};
use goo_v5::{build_goo_v5_container_bytes, build_goo_v5_container_bytes_with_progress};
use goo_v5_rle::{decode_panel_binary, decode_panel_vuf, encode_panel_from_mask_window, encode_panel_from_runs};
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

pub struct GooPluginEncoder;

fn choose_goo_encode_threads() -> usize {
    let hw = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let env = std::env::var("DF_V3_GOO_ENCODE_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v >= 1)
        .unwrap_or(hw);
    env.clamp(1, hw)
}

fn cap_goo_encode_workers_for_mask_bytes(requested: usize, expected_pixels: usize) -> usize {
    let bytes_per_mask = expected_pixels;
    let mut capped = requested.max(1);

    // Optional override: memory budget for in-flight Goo raw masks (MB).
    // Example: 1024 means allow about 1 GB worth of queued/working masks.
    let budget_override = std::env::var("DF_V3_MAX_GOO_INFLIGHT_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v >= 64)
        .map(|mb| mb.saturating_mul(1024 * 1024));

    if let Some(budget_bytes) = budget_override {
        let allowed = (budget_bytes / bytes_per_mask.max(1)).max(1);
        capped = capped.min(allowed);
    }

    // Each encoder worker can hold a full raw mask plus encoded output buffers.
    // Be conservative for massive layers to prevent allocation failures.
    if budget_override.is_none() {
        if bytes_per_mask >= 48 * 1024 * 1024 {
            capped = capped.min(2);
        } else if bytes_per_mask >= 24 * 1024 * 1024 {
            capped = capped.min(4);
        } else if bytes_per_mask >= 12 * 1024 * 1024 {
            capped = capped.min(8);
        }
    }

    capped.max(1)
}

fn choose_goo_encode_queue_depth(worker_count: usize, expected_pixels: usize) -> usize {
    let bytes_per_mask = expected_pixels;
    if let Some(budget_bytes) = std::env::var("DF_V3_MAX_GOO_INFLIGHT_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v >= 64)
        .map(|mb| mb.saturating_mul(1024 * 1024))
    {
        let allowed = (budget_bytes / bytes_per_mask.max(1)).max(1);
        return allowed.min((worker_count.saturating_mul(2)).max(1));
    }

    if bytes_per_mask >= 48 * 1024 * 1024 {
        1
    } else if bytes_per_mask >= 24 * 1024 * 1024 {
        2
    } else if bytes_per_mask >= 12 * 1024 * 1024 {
        3
    } else {
        (worker_count.saturating_mul(3)).clamp(3, 24)
    }
}

struct GooRawMaskStreamingEncoder {
    job: SliceJobV3,
    work_tx: Option<crossbeam_channel::Sender<(u32, Vec<u8>)>>,
    result_rx: mpsc::Receiver<Result<goo_types::GooPreparedLayer, SlicerV3Error>>,
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

        // Close producer channel and let workers drain outstanding tasks.
        let _ = self.work_tx.take();

        while let Some(handle) = self.workers.pop() {
            if handle.join().is_err() {
                return Err(SlicerV3Error::UnsupportedOutput(
                    "Goo streaming worker panicked".to_string(),
                ));
            }
        }

        let expected_layers = self.consumed_layers as usize;
        let mut ordered: Vec<Option<goo_types::GooPreparedLayer>> =
            Vec::with_capacity(expected_layers);
        ordered.resize_with(expected_layers, || None);

        for _ in 0..expected_layers {
            let msg = self.result_rx.recv().map_err(|_| {
                SlicerV3Error::MissingRenderedLayerPayload(
                    "Goo streaming worker results ended unexpectedly".to_string(),
                )
            })?;

            let prepared = msg?;
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

        build_goo_container_bytes(&self.job, &prepared)
    }
}

/// Sequential RLE streaming encoder: receives `Vec<RleRun>` per layer (already
/// rasterized by `rasterize_layer_rle`), converts directly to Goo RLE, and
/// assembles the container in `finalize_to_bytes` — zero pixel-buffer overhead.
struct GooRleStreamingEncoder {
    job: SliceJobV3,
    is_anti_aliased: bool,
    threshold: u8,
    layer_height_mm: f32,
    bottom_layer_count: u32,
    total_pixels: usize,
    prepared: Vec<goo_types::GooPreparedLayer>,
}

impl GooRleStreamingEncoder {
    fn prepared_layer(&self, layer_index: u32, encoded: Vec<u8>) -> goo_types::GooPreparedLayer {
        goo_types::GooPreparedLayer {
            index: layer_index as usize,
            position_z_mm: (layer_index as f32 + 1.0) * self.layer_height_mm,
            is_bottom: layer_index < self.bottom_layer_count,
            encoded,
        }
    }
}

impl RleStreamEncoder for GooRleStreamingEncoder {
    fn consume_rle_layer(
        &mut self,
        layer_index: u32,
        runs: Vec<crate::rle::RleRun>,
    ) -> Result<(), SlicerV3Error> {
        let encoded = encode_goo_rle_from_runs(
            &runs,
            self.is_anti_aliased,
            self.threshold,
            self.total_pixels,
        );
        let layer = self.prepared_layer(layer_index, encoded);
        self.prepared.push(layer);
        Ok(())
    }

    fn finalize_to_bytes(mut self: Box<Self>) -> Result<Vec<u8>, SlicerV3Error> {
        if self.prepared.is_empty() {
            return Err(SlicerV3Error::MissingRenderedLayerPayload(
                "no rendered layers were provided for Goo RLE encoding".to_string(),
            ));
        }
        self.prepared.sort_unstable_by_key(|p| p.index);
        build_goo_container_bytes(&self.job, &self.prepared)
    }

    fn parallel_encode_fn(
        &self,
    ) -> Option<
        Arc<dyn Fn(u32, &[crate::rle::RleRun]) -> Result<Vec<u8>, SlicerV3Error> + Send + Sync>,
    > {
        let is_anti_aliased = self.is_anti_aliased;
        let threshold = self.threshold;
        let total_pixels = self.total_pixels;

        Some(Arc::new(
            move |_layer_index: u32, runs: &[crate::rle::RleRun]| {
                Ok(encode_goo_rle_from_runs(
                    runs,
                    is_anti_aliased,
                    threshold,
                    total_pixels,
                ))
            },
        ))
    }

    fn store_encoded_layer(&mut self, layer_index: u32, bytes: Vec<u8>) {
        let layer = self.prepared_layer(layer_index, bytes);
        self.prepared.push(layer);
    }
}

// ── GOO V5.1 block-partitioned streaming encoder ─────────────────────────────

/// Streaming encoder for the V5.1 container: declares the two half-screen
/// column blocks via `rle_blocks`, encodes each panel with the configured
/// grammar (VUF default, binary selectable via `goo.v5RleMode`), and
/// assembles the little-endian container in `finalize_to_bytes`.
struct GooV5RleStreamingEncoder {
    job: SliceJobV3,
    settings: goo_types::GooV5Settings,
    threshold: u8,
    is_anti_aliased: bool,
    blocks: Vec<crate::rle::RleBlockSpec>,
    full_width: usize,
    height: usize,
    /// encoded[layer][block] — framed panel blocks, left then right.
    encoded: Vec<Vec<Option<Vec<u8>>>>,
}

impl GooV5RleStreamingEncoder {
    fn new(job: &SliceJobV3) -> Self {
        let full_width = job.source_width_px;
        let half = full_width / 2;
        Self {
            job: job.clone(),
            settings: parse_goo_v5_settings_from_job(job),
            threshold: parse_threshold_from_job(job),
            is_anti_aliased: job.produces_grayscale_output(),
            blocks: vec![
                crate::rle::make_rle_block(0, half),
                crate::rle::make_rle_block(half, full_width - half),
            ],
            full_width: full_width as usize,
            height: job.source_height_px as usize,
            encoded: Vec::new(),
        }
    }

    fn store(&mut self, layer: usize, block: usize, bytes: Vec<u8>) {
        if self.encoded.len() <= layer {
            self.encoded
                .resize_with(layer + 1, || vec![None; goo_types::GOO_V5_PARTITION_COUNT]);
        }
        self.encoded[layer][block] = Some(bytes);
    }

    fn block_pixels(&self, block: usize) -> usize {
        self.blocks
            .get(block)
            .map(|spec| spec.width as usize)
            .unwrap_or(0)
            * self.height
    }
}

impl RleStreamEncoder for GooV5RleStreamingEncoder {
    fn consume_rle_layer(
        &mut self,
        layer_index: u32,
        runs: Vec<crate::rle::RleRun>,
    ) -> Result<(), SlicerV3Error> {
        // Whole-layer fallback (e.g. the 3DAA pipeline delivers full-width
        // streams): split at the seam and encode both panels.
        let specs = self.blocks.clone();
        for (block_index, spec) in specs.iter().enumerate() {
            let crop_right = (self.full_width as u32)
                .saturating_sub(spec.start_col)
                .saturating_sub(spec.width);
            let window =
                crate::rle::crop_rle_columns(&runs, self.full_width as u32, spec.start_col, crop_right);
            let bytes = encode_panel_from_runs(
                &window,
                &self.settings,
                self.threshold,
                self.is_anti_aliased,
                self.block_pixels(block_index),
            );
            self.store(layer_index as usize, block_index, bytes);
        }
        Ok(())
    }

    fn finalize_to_bytes(self: Box<Self>) -> Result<Vec<u8>, SlicerV3Error> {
        if self.encoded.is_empty() {
            return Err(SlicerV3Error::MissingRenderedLayerPayload(
                "no rendered layers were provided for Goo V5 encoding".to_string(),
            ));
        }
        let job = self.job;
        let mut prepared = Vec::with_capacity(self.encoded.len());
        for (index, blocks) in self.encoded.into_iter().enumerate() {
            let mut layer_blocks = Vec::with_capacity(blocks.len());
            for (block_index, block) in blocks.into_iter().enumerate() {
                let Some(block) = block else {
                    return Err(SlicerV3Error::MissingRenderedLayerPayload(format!(
                        "Goo V5 layer {index} block {block_index} missing from streaming output"
                    )));
                };
                layer_blocks.push(block);
            }
            prepared.push(goo_types::GooV5PreparedLayer {
                index,
                blocks: layer_blocks,
            });
        }
        build_goo_v5_container_bytes(&job, &prepared)
    }

    fn rle_blocks(&self, _job: &SliceJobV3) -> Option<Vec<crate::rle::RleBlockSpec>> {
        Some(self.blocks.clone())
    }

    fn consume_rle_block(
        &mut self,
        layer_index: u32,
        block_index: u32,
        runs: Vec<crate::rle::RleRun>,
    ) -> Result<(), SlicerV3Error> {
        let bytes = encode_panel_from_runs(
            &runs,
            &self.settings,
            self.threshold,
            self.is_anti_aliased,
            self.block_pixels(block_index as usize),
        );
        self.store(layer_index as usize, block_index as usize, bytes);
        Ok(())
    }

    fn parallel_encode_block_fn(
        &self,
    ) -> Option<
        Arc<dyn Fn(u32, u32, &[crate::rle::RleRun]) -> Result<Vec<u8>, SlicerV3Error> + Send + Sync>,
    > {
        let settings = self.settings.clone();
        let threshold = self.threshold;
        let is_anti_aliased = self.is_anti_aliased;
        let height = self.height;
        let widths: Vec<usize> = self.blocks.iter().map(|b| b.width as usize).collect();

        Some(Arc::new(
            move |_layer_index: u32, block_index: u32, runs: &[crate::rle::RleRun]| {
                let pixels = widths.get(block_index as usize).copied().unwrap_or(0) * height;
                Ok(encode_panel_from_runs(
                    runs,
                    &settings,
                    threshold,
                    is_anti_aliased,
                    pixels,
                ))
            },
        ))
    }

    fn store_encoded_block(&mut self, layer_index: u32, block_index: u32, bytes: Vec<u8>) {
        self.store(layer_index as usize, block_index as usize, bytes);
    }
}

/// Raw-mask batch fallback for V5.1: split each full-width mask at the seam
/// and encode both panels per layer.
fn encode_goo_v5_container_from_raw_masks(
    job: &SliceJobV3,
    raw_masks: &[Vec<u8>],
    on_progress: Option<&dyn Fn(u32, u32)>,
) -> Result<Vec<u8>, SlicerV3Error> {
    let settings = parse_goo_v5_settings_from_job(job);
    let threshold = parse_threshold_from_job(job);
    let is_anti_aliased = job.produces_grayscale_output();
    let full_width = job.source_width_px as usize;
    let height = job.source_height_px as usize;
    let half = full_width / 2;
    let windows = [(0usize, half), (half, full_width - half)];

    let total_prepare = raw_masks.len() as u32;
    let total_progress = total_prepare.saturating_add(1).max(1);

    let mut prepared = Vec::with_capacity(raw_masks.len());
    for (index, mask) in raw_masks.iter().enumerate() {
        let blocks = windows
            .iter()
            .map(|&(start_col, window_width)| {
                encode_panel_from_mask_window(
                    mask,
                    full_width,
                    height,
                    start_col,
                    window_width,
                    &settings,
                    threshold,
                    is_anti_aliased,
                )
            })
            .collect();
        prepared.push(goo_types::GooV5PreparedLayer { index, blocks });
        if let Some(progress) = on_progress {
            progress(index as u32 + 1, total_progress);
        }
    }

    let bytes = build_goo_v5_container_bytes_with_progress(job, &prepared, None)?;
    if let Some(progress) = on_progress {
        progress(total_progress, total_progress);
    }
    Ok(bytes)
}

pub fn create_plugin_encoder() -> Vec<Box<dyn FormatEncoder>> {
    vec![Box::new(GooPluginEncoder)]
}

// Parsing and layout logic live in `goo_metadata.rs` and `goo_layout.rs`.

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
        // V5.1 jobs stream through the block-partitioned RLE encoder; batch
        // raw-mask callers fall back to `encode_container_from_rendered_layers`.
        if parse_goo_format_version_hint_from_job(job) == Some(GooFormatVersion::V51) {
            return Ok(None);
        }

        let timing = parse_timing_model_from_job(job);
        let threshold = parse_threshold_from_job(job);
        let expected_pixels =
            (job.source_width_px as usize).saturating_mul(job.source_height_px as usize);

        let worker_count =
            cap_goo_encode_workers_for_mask_bytes(choose_goo_encode_threads(), expected_pixels);
        let queue_depth = choose_goo_encode_queue_depth(worker_count, expected_pixels);
        let (work_tx, work_rx) = bounded::<(u32, Vec<u8>)>(queue_depth);
        let (result_tx, result_rx) =
            mpsc::channel::<Result<goo_types::GooPreparedLayer, SlicerV3Error>>();
        let mut workers = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let work_rx = work_rx.clone();
            let result_tx = result_tx.clone();
            let worker_threshold = threshold;
            let worker_is_anti_aliased = job.produces_grayscale_output();
            let worker_layer_height_mm = job.layer_height_mm;
            let worker_bottom_layer_count = timing.bottom_layer_count;
            let worker_expected_pixels = expected_pixels;

            let handle = thread::spawn(move || loop {
                let task = work_rx.recv();

                let Ok((layer_index, raw_mask)) = task else {
                    break;
                };

                if raw_mask.is_empty() {
                    let prepared = encode_single_goo_empty_layer(
                        layer_index as usize,
                        worker_expected_pixels,
                        worker_layer_height_mm,
                        worker_bottom_layer_count,
                    );
                    crate::pipeline::return_mask_to_pool(raw_mask);

                    if result_tx.send(Ok(prepared)).is_err() {
                        break;
                    }
                    continue;
                }

                if raw_mask.len() != worker_expected_pixels {
                    let len = raw_mask.len();
                    crate::pipeline::return_mask_to_pool(raw_mask);
                    let _ =
                        result_tx.send(Err(SlicerV3Error::MissingRenderedLayerPayload(format!(
                            "Goo layer {layer_index} size mismatch: expected {} bytes, got {}",
                            worker_expected_pixels, len
                        ))));
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
        if parse_goo_format_version_hint_from_job(job) == Some(GooFormatVersion::V51) {
            return Ok(Some(Box::new(GooV5RleStreamingEncoder::new(job))));
        }

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
        let layers = rendered_layers
            .raw_mask_layers
            .as_ref()
            .map(|v| v.len() as u32)
            .unwrap_or(0);
        layers.saturating_mul(2).saturating_add(1).max(1)
    }

    fn encode_container_from_rendered_layers_with_progress(
        &self,
        job: &SliceJobV3,
        rendered_layers: &RenderedLayersV3,
        _layer_area_stats: &[LayerAreaStatsV3],
        on_progress: Option<&dyn Fn(u32, u32)>,
    ) -> Result<Vec<u8>, SlicerV3Error> {
        let Some(raw_masks) = rendered_layers.raw_mask_layers.as_ref() else {
            return Err(SlicerV3Error::MissingRenderedLayerPayload(
                "raw mask layers are required for Goo encoding".to_string(),
            ));
        };

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

        if parse_goo_format_version_hint_from_job(job) == Some(GooFormatVersion::V51) {
            return encode_goo_v5_container_from_raw_masks(job, raw_masks, on_progress);
        }

        let timing = parse_timing_model_from_job(job);
        let threshold = parse_threshold_from_job(job);
        let is_anti_aliased = job.produces_grayscale_output();

        let total_prepare = raw_masks.len() as u32;
        let total_layout = raw_masks.len() as u32;
        let total_progress = total_prepare
            .saturating_add(total_layout)
            .saturating_add(1)
            .max(1);

        let prepare_progress = on_progress.map(|progress| {
            move |done: u32, total: u32| {
                let safe_total = total.max(1);
                let mapped = ((done.min(safe_total) as u64) * (total_prepare as u64)
                    / (safe_total as u64)) as u32;
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

        let encoded_bytes: usize = prepared.iter().map(|l| l.encoded.len()).sum();
        if encoded_bytes == 0 {
            return Err(SlicerV3Error::UnsupportedOutput(
                "Goo encoding produced empty payload".to_string(),
            ));
        }

        let layout_progress = on_progress.map(|progress| {
            move |done: u32, total: u32| {
                let safe_total = total.max(1);
                let mapped = ((done.min(safe_total) as u64) * (total_layout as u64)
                    / (safe_total as u64)) as u32;
                progress(total_prepare.saturating_add(mapped), total_progress);
            }
        });

        let bytes = build_goo_container_bytes_with_progress(
            job,
            &prepared,
            layout_progress.as_ref().map(|cb| cb as &dyn Fn(u32, u32)),
        )?;

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

    fn read_layer_preview_png(
        &self,
        path: &Path,
        layer_number: u32,
    ) -> Result<Vec<u8>, SlicerV3Error> {
        self::read_layer_preview_png(path, layer_number).map_err(SlicerV3Error::LayerPreview)
    }
}

/// Reads a single layer preview PNG from a Goo binary file.
/// `layer_number` is 1-based.
pub fn read_layer_preview_png(path: &Path, layer_number: u32) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};

    if layer_number == 0 {
        return Err("Layer number must be >= 1".to_string());
    }

    let mut file =
        std::fs::File::open(path).map_err(|e| format!("Failed opening Goo file: {e}"))?;

    // Validate the 8-byte file magic that follows the 4-byte version string.
    let mut head = [0u8; 12];
    file.read_exact(&mut head)
        .map_err(|e| format!("Goo header read failed: {e}"))?;
    if head[4..12] != goo_types::GOO_FILE_MAGIC {
        return Err("Goo file magic mismatch".to_string());
    }

    // Version dispatch: V5.1 uses the LE DRAM-table layout.
    if &head[0..4] == goo_types::GOO_V5_MAGIC {
        return read_goo_v5_layer_preview_png(&mut file, layer_number);
    }

    // Fixed post-preview numeric block: LayerCount, ResolutionX, ResolutionY.
    file.seek(SeekFrom::Start(goo_types::GOO_LAYER_COUNT_OFFSET))
        .map_err(|e| format!("Goo header seek failed: {e}"))?;

    let mut block = [0u8; 8];
    file.read_exact(&mut block)
        .map_err(|e| format!("Goo layer count read failed: {e}"))?;
    let layer_count = u32::from_be_bytes(block[0..4].try_into().unwrap());
    let width_px = u16::from_be_bytes(block[4..6].try_into().unwrap()) as u32;
    let height_px = u16::from_be_bytes(block[6..8].try_into().unwrap()) as u32;

    if width_px == 0 || height_px == 0 {
        return Err(format!(
            "Goo file reports invalid dimensions {width_px}×{height_px}"
        ));
    }
    if layer_number > layer_count {
        return Err(format!(
            "Layer {layer_number} out of range (file has {layer_count} layers)"
        ));
    }

    file.seek(SeekFrom::Start(goo_types::GOO_LAYER_DEF_ADDRESS_OFFSET))
        .map_err(|e| format!("Goo layer def address seek failed: {e}"))?;
    let mut addr = [0u8; 4];
    file.read_exact(&mut addr)
        .map_err(|e| format!("Goo layer def address read failed: {e}"))?;
    let mut record_offset = u32::from_be_bytes(addr) as u64;

    // Layer records are stored inline (no offset table): each is a fixed
    // 70-byte definition ending in DataLength, then the RLE payload, then a
    // trailing CRLF. Walk records up to the requested layer.
    let layer_index = layer_number - 1;
    let mut def = [0u8; goo_types::GOO_LAYER_DEF_SIZE];
    for _ in 0..layer_index {
        file.seek(SeekFrom::Start(record_offset))
            .map_err(|e| format!("Goo layer def seek failed: {e}"))?;
        file.read_exact(&mut def)
            .map_err(|e| format!("Goo layer def read failed: {e}"))?;
        let data_len = u32::from_be_bytes(def[66..70].try_into().unwrap()) as u64;
        record_offset += goo_types::GOO_LAYER_DEF_SIZE as u64 + data_len + 2;
    }

    file.seek(SeekFrom::Start(record_offset))
        .map_err(|e| format!("Goo layer def seek failed: {e}"))?;
    file.read_exact(&mut def)
        .map_err(|e| format!("Goo layer def read failed: {e}"))?;
    let data_len = u32::from_be_bytes(def[66..70].try_into().unwrap()) as usize;

    let mut rle_bytes = vec![0u8; data_len];
    file.read_exact(&mut rle_bytes)
        .map_err(|e| format!("Goo layer RLE read failed: {e}"))?;

    let expected_pixels = width_px as usize * height_px as usize;
    let pixels = decode_goo_rle(&rle_bytes, expected_pixels)?;
    encode_pixels_as_grayscale_png(width_px, height_px, &pixels)
}

/// Reads a single layer preview from a GOO V5.1 file and returns it as a
/// grayscale PNG. The caller has already validated the 12-byte header and
/// positioned the cursor at byte 12.
fn read_goo_v5_layer_preview_png(
    file: &mut std::fs::File,
    layer_number: u32,
) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};

    // ── Numeric block (LE, at GOO_V5_PARAMS_OFFSET = 195 310) ──────────
    file.seek(SeekFrom::Start(goo_types::GOO_V5_PARAMS_OFFSET as u64))
        .map_err(|e| format!("Goo V5 params seek failed: {e}"))?;

    let mut block = [0u8; 8];
    file.read_exact(&mut block)
        .map_err(|e| format!("Goo V5 params read failed: {e}"))?;
    let layer_count = u32::from_le_bytes(block[0..4].try_into().unwrap());
    let width_px = u16::from_le_bytes(block[4..6].try_into().unwrap()) as u32;
    let height_px = u16::from_le_bytes(block[6..8].try_into().unwrap()) as u32;

    if width_px == 0 || height_px == 0 {
        return Err(format!(
            "Goo V5 file reports invalid dimensions {width_px}×{height_px}"
        ));
    }
    if layer_number > layer_count {
        return Err(format!(
            "Layer {layer_number} out of range (file has {layer_count} layers)"
        ));
    }

    // ── Preamble: pixel_bit_width ──────────────────────────────────────
    file.seek(SeekFrom::Start(
        goo_types::GOO_V5_PREAMBLE_OFFSET as u64 + 14,
    ))
    .map_err(|e| format!("Goo V5 preamble seek failed: {e}"))?;
    let mut pixel_bit_width = [0u8; 1];
    file.read_exact(&mut pixel_bit_width)
        .map_err(|e| format!("Goo V5 preamble read failed: {e}"))?;
    let pixel_bit_width = pixel_bit_width[0];

    // ── Compute structural offsets ─────────────────────────────────────
    let l = layer_count as u64;
    let ldt_start = goo_types::GOO_V5_LDT_START as u64; // 195 493
    let t1_end = ldt_start + l * 8;
    let defs_base = ldt_start + l * 24 + goo_types::GOO_V5_RDT_PAD_SIZE as u64;
    let rle_start = defs_base + l * (goo_types::GOO_V5_LAYER_DEF_SIZE as u64);

    let total_blocks = (l * 2) as usize; // 2 panels per layer

    // ── Read T2 / IEDT block sizes ─────────────────────────────────────
    file.seek(SeekFrom::Start(t1_end))
        .map_err(|e| format!("Goo V5 IEDT seek failed: {e}"))?;
    let mut t2_buf = vec![0u8; total_blocks * 8];
    file.read_exact(&mut t2_buf)
        .map_err(|e| format!("Goo V5 IEDT read failed: {e}"))?;

    let mut block_sizes = Vec::with_capacity(total_blocks);
    for entry in t2_buf.chunks_exact(8) {
        let page_size =
            u32::from_le_bytes(entry[4..8].try_into().unwrap());
        block_sizes.push((page_size >> 8) as usize);
    }

    // ── Locate target layer's two panels ───────────────────────────────
    let layer_index = (layer_number - 1) as usize;
    let first_block = layer_index * 2;
    let panel_offset: usize = block_sizes[..first_block].iter().sum();
    let left_size = block_sizes[first_block];
    let right_size = block_sizes[first_block + 1];
    let total_read = left_size + right_size;

    file.seek(SeekFrom::Start(rle_start + panel_offset as u64))
        .map_err(|e| format!("Goo V5 RLE seek failed: {e}"))?;
    let mut rle_buf = vec![0u8; total_read];
    file.read_exact(&mut rle_buf)
        .map_err(|e| format!("Goo V5 RLE read failed: {e}"))?;

    let left_block = &rle_buf[..left_size];
    let right_block = &rle_buf[left_size..];

    // ── Decode panels ──────────────────────────────────────────────────
    let half_width = (width_px as usize) / 2;
    let panel_pixels = half_width * (height_px as usize);
    let max_level: u8 = if pixel_bit_width >= 8 {
        255
    } else {
        (1u16 << pixel_bit_width) as u8 - 1
    };

    let decode_panels = |left: &[u8], right: &[u8]| -> Result<(Vec<u8>, Vec<u8>), String> {
        // Try VUF first (default / superset); fall back to binary.
        match decode_panel_vuf(left, panel_pixels, max_level) {
            Ok(lp) => match decode_panel_vuf(right, panel_pixels, max_level) {
                Ok(rp) => return Ok((lp, rp)),
                Err(_) => {}
            },
            Err(_) => {}
        }
        // Binary fallback
        let lp = decode_panel_binary(left, panel_pixels)?;
        let rp = decode_panel_binary(right, panel_pixels)?;
        Ok((lp, rp))
    };

    let (left_pixels, right_pixels) = decode_panels(left_block, right_block)
        .map_err(|e| format!("Goo V5 layer {layer_number} panel decode failed: {e}"))?;

    // ── Interleave half-screen panels into full-width pixel buffer ─────
    let full_pixels = (width_px as usize) * (height_px as usize);
    let mut stacked = Vec::with_capacity(full_pixels);
    for row in 0..(height_px as usize) {
        let r0 = row * half_width;
        let r1 = (row + 1) * half_width;
        stacked.extend_from_slice(&left_pixels[r0..r1]);
        stacked.extend_from_slice(&right_pixels[r0..r1]);
    }

    encode_pixels_as_grayscale_png(width_px, height_px, &stacked)
}

/// Decodes Goo run-length encoded layer data (0x55 magic + chunks +
/// one's-complement checksum) into a flat grayscale pixel buffer.
fn decode_goo_rle(data: &[u8], expected_pixels: usize) -> Result<Vec<u8>, String> {
    if data.len() < 2 || data[0] != goo_types::GOO_LAYER_MAGIC {
        return Err("Goo layer RLE missing 0x55 magic".to_string());
    }

    let payload = &data[1..data.len() - 1];
    let sum: u8 = payload.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    if !sum != data[data.len() - 1] {
        return Err(format!(
            "Goo layer RLE checksum mismatch: expected {:#04x}, got {:#04x}",
            !sum,
            data[data.len() - 1]
        ));
    }

    let mut pixels = Vec::with_capacity(expected_pixels);
    let mut previous_color: u8 = 0;
    let mut i = 0usize;

    while i < payload.len() && pixels.len() < expected_pixels {
        let byte0 = payload[i];
        i += 1;
        let chunk_type = byte0 >> 6;

        if chunk_type == 0b10 {
            // Difference chunk: bits [3:0] hold the delta from the previous
            // pixel, bit 5 the sign, bit 4 an extended one-byte stride.
            let diff = byte0 & 0x0F;
            let color = if byte0 & 0x20 != 0 {
                previous_color.wrapping_sub(diff)
            } else {
                previous_color.wrapping_add(diff)
            };
            let stride = if byte0 & 0x10 != 0 {
                if i >= payload.len() {
                    break;
                }
                let s = payload[i] as u32;
                i += 1;
                s
            } else {
                1
            };

            let remaining = expected_pixels - pixels.len();
            for _ in 0..(stride as usize).min(remaining) {
                pixels.push(color);
            }
            previous_color = color;
            continue;
        }

        // Gray chunks carry the color byte immediately after the chunk byte,
        // before any extended run-length bytes.
        let color = match chunk_type {
            0b00 => 0x00,
            0b11 => 0xFF,
            _ => {
                if i >= payload.len() {
                    break;
                }
                let c = payload[i];
                i += 1;
                c
            }
        };

        let stride_bits = (byte0 >> 4) & 0x3;
        let ext_len = stride_bits as usize;
        if i + ext_len > payload.len() {
            break;
        }
        let mut run_len = (byte0 & 0x0F) as u32;
        for k in 0..ext_len {
            run_len |= (payload[i + k] as u32) << (4 + 8 * (ext_len - 1 - k));
        }
        i += ext_len;

        let remaining = expected_pixels - pixels.len();
        for _ in 0..(run_len as usize).min(remaining) {
            pixels.push(color);
        }
        previous_color = color;
    }

    pixels.resize(expected_pixels, 0);
    Ok(pixels)
}

/// Encodes a flat grayscale pixel buffer as an 8-bit grayscale PNG.
fn encode_pixels_as_grayscale_png(
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("Goo PNG header write failed: {e}"))?;
    writer
        .write_image_data(pixels)
        .map_err(|e| format!("Goo PNG data write failed: {e}"))?;
    drop(writer);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::goo_layout::{encode_single_goo_layer_from_raw_mask, goo_rle_encode};
    use super::goo_metadata::{parse_threshold_from_metadata, parse_timing_model_from_metadata};
    use super::goo_types::{
        GOO_FILE_MAGIC, GOO_FILE_VERSION, GOO_HEADER_SIZE, GOO_LAYER_COUNT_OFFSET,
        GOO_LAYER_DEF_ADDRESS_OFFSET, GOO_LAYER_DEF_SIZE,
    };
    use super::{build_goo_container_bytes, decode_goo_rle, read_layer_preview_png};
    use crate::types::SliceJobV3;

    fn make_test_job() -> SliceJobV3 {
        SliceJobV3 {
            output_format: ".goo".to_string(),
            source_width_px: 4,
            source_height_px: 4,
            width_px: 4,
            height_px: 4,
            build_width_mm: 10.0,
            build_depth_mm: 20.0,
            layer_height_mm: 0.05,
            total_layers: 2,
            anti_aliasing_level: "Off".to_string(),
            metadata_json: "{}".to_string(),
            ..Default::default()
        }
    }

    fn make_prepared_layers(job: &SliceJobV3, bottom_layer_count: u32) -> Vec<super::goo_types::GooPreparedLayer> {
        let pixels = (job.source_width_px * job.source_height_px) as usize;
        let mask = vec![255u8; pixels];
        (0..2)
            .map(|index| {
                encode_single_goo_layer_from_raw_mask(
                    index,
                    &mask,
                    false,
                    127,
                    job.layer_height_mm,
                    bottom_layer_count,
                )
            })
            .collect()
    }

    #[test]
    fn metadata_threshold_defaults_when_missing_or_invalid() {
        assert_eq!(parse_threshold_from_metadata("{}"), 127);
        assert_eq!(parse_threshold_from_metadata("not-json"), 127);
    }

    #[test]
    fn metadata_threshold_reads_supported_paths() {
        let direct = r#"{ "goo": { "binaryThreshold": 180 } }"#;
        let nested = r#"{ "export": { "goo": { "binaryThreshold": 200 } } }"#;
        let material = r#"{ "material": { "binaryThreshold": 90 } }"#;

        assert_eq!(parse_threshold_from_metadata(direct), 180);
        assert_eq!(parse_threshold_from_metadata(nested), 200);
        assert_eq!(parse_threshold_from_metadata(material), 90);
    }

    #[test]
    fn metadata_timing_prefers_goo_over_material() {
        let meta = r#"{
            "material": {
                "liftDistanceMm": 4.0,
                "liftSpeedMmMin": 40.0,
                "retractSpeedMmMin": 120.0,
                "bottomLayerCount": 4
            },
            "goo": {
                "liftDistanceMm": 7.5,
                "liftSpeedMmMin": 65.0,
                "retractSpeedMmMin": 190.0,
                "bottomLayerCount": 8
            }
        }"#;

        let timing = parse_timing_model_from_metadata(meta);
        assert!((timing.lift_distance_mm - 7.5).abs() < f32::EPSILON);
        assert!((timing.lift_speed_mm_min - 65.0).abs() < f32::EPSILON);
        assert!((timing.retract_speed_mm_min - 190.0).abs() < f32::EPSILON);
        assert_eq!(timing.bottom_layer_count, 8);
    }

    #[test]
    fn metadata_timing_reads_shared_ctb_namespace() {
        // The differential material settings contract writes some GOO fields
        // under `ctb.*` (see plugins/elegoo/materialSettings/*.json).
        let meta = r#"{
            "ctb": {
                "waitTimeBeforeCureSec": 1.5,
                "waitTimeAfterCureSec": 0.5,
                "bottomWaitTimeAfterLiftSec": 2.0
            }
        }"#;

        let timing = parse_timing_model_from_metadata(meta);
        assert!((timing.wait_time_before_cure_sec - 1.5).abs() < f32::EPSILON);
        assert!((timing.wait_time_after_cure_sec - 0.5).abs() < f32::EPSILON);
        assert!((timing.bottom_wait_time_after_lift_sec - 2.0).abs() < f32::EPSILON);
        assert_eq!(timing.delay_mode, 1);
    }

    #[test]
    fn metadata_timing_simple_mode_zeroes_two_stage_fields() {
        let meta = r#"{
            "printer": {
                "settingsMode": "simple"
            },
            "goo": {
                "liftDistanceMm": 6.0,
                "liftSpeedMmMin": 60.0,
                "retractDistanceMm": 4.0,
                "retractSpeedMmMin": 150.0,
                "liftDistance2Mm": 2.5,
                "liftSpeed2MmMin": 75.0,
                "retractDistance2Mm": 1.25,
                "retractSpeed2MmMin": 110.0,
                "bottomRetractDistance2Mm": 0.8,
                "bottomRetractSpeed2MmMin": 90.0
            }
        }"#;

        let timing = parse_timing_model_from_metadata(meta);
        assert!((timing.lift_distance2_mm - 0.0).abs() < f32::EPSILON);
        assert!((timing.lift_speed2_mm_min - 0.0).abs() < f32::EPSILON);
        assert!((timing.retract_distance2_mm - 0.0).abs() < f32::EPSILON);
        assert!((timing.retract_speed2_mm_min - 0.0).abs() < f32::EPSILON);
        assert!((timing.bottom_retract_distance2_mm - 0.0).abs() < f32::EPSILON);
        assert!((timing.bottom_retract_speed2_mm_min - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn metadata_timing_tilting_mode_overrides_motion() {
        let meta = r#"{
            "printer": {
                "settingsMode": "tilting"
            },
            "material": {
                "layerHeightMm": 0.1,
                "liftDistanceMm": 6.0,
                "liftSpeedMmMin": 80.0,
                "retractSpeedMmMin": 200.0
            }
        }"#;

        let timing = parse_timing_model_from_metadata(meta);
        assert!((timing.lift_speed_mm_min - 60.0).abs() < f32::EPSILON);
        assert!((timing.bottom_lift_speed_mm_min - 60.0).abs() < f32::EPSILON);
        assert!((timing.retract_speed_mm_min - 150.0).abs() < f32::EPSILON);
        assert!((timing.bottom_retract_speed_mm_min - 150.0).abs() < f32::EPSILON);
        assert!((timing.lift_distance_mm - 0.1).abs() < f32::EPSILON);
        assert!((timing.bottom_lift_distance_mm - 0.1).abs() < f32::EPSILON);
    }

    #[test]
    fn metadata_pwm_percent_maps_to_byte_range() {
        let meta = r#"{ "goo": { "projectorPwmPercent": 50.0 } }"#;
        let timing = parse_timing_model_from_metadata(meta);
        assert_eq!(timing.light_pwm, 128);
        // Bottom PWM inherits the normal percentage when unset.
        assert_eq!(timing.bottom_light_pwm, 128);

        let full = parse_timing_model_from_metadata("{}");
        assert_eq!(full.light_pwm, 255);
        assert_eq!(full.bottom_light_pwm, 255);
        assert_eq!(full.delay_mode, 0);
    }

    #[test]
    fn container_header_is_exact_size_and_versioned() {
        let job = make_test_job();
        let prepared = make_prepared_layers(&job, 1);

        let bytes = build_goo_container_bytes(&job, &prepared).expect("container should build");

        assert_eq!(&bytes[0..4], GOO_FILE_VERSION);
        assert_eq!(&bytes[4..12], &GOO_FILE_MAGIC);

        let layer_count_off = GOO_LAYER_COUNT_OFFSET as usize;
        let layer_count =
            u32::from_be_bytes(bytes[layer_count_off..layer_count_off + 4].try_into().unwrap());
        assert_eq!(layer_count, 2);

        let addr_off = GOO_LAYER_DEF_ADDRESS_OFFSET as usize;
        let layer_def_address =
            u32::from_be_bytes(bytes[addr_off..addr_off + 4].try_into().unwrap());
        assert_eq!(layer_def_address, GOO_HEADER_SIZE);

        // Footer: three padding bytes then the file magic.
        let len = bytes.len();
        assert_eq!(&bytes[len - 8..], &GOO_FILE_MAGIC);
        assert_eq!(&bytes[len - 11..len - 8], &[0u8, 0, 0]);
    }

    #[test]
    fn container_layer_defs_use_bottom_then_normal_timing() {
        let mut job = make_test_job();
        job.metadata_json = r#"{
            "material": {
                "normalExposureSec": 2.0,
                "bottomExposureSec": 20.0,
                "bottomLayerCount": 1
            }
        }"#
        .to_string();

        let prepared = make_prepared_layers(&job, 1);
        let layer0_len = prepared[0].encoded.len();

        let bytes = build_goo_container_bytes(&job, &prepared).expect("container should build");

        // ExposureTime sits at layer def offset +10 (Pause 2 + PausePositionZ 4 + PositionZ 4).
        let def0 = GOO_HEADER_SIZE as usize;
        let exposure0 =
            f32::from_be_bytes(bytes[def0 + 10..def0 + 14].try_into().unwrap());
        assert!((exposure0 - 20.0).abs() < f32::EPSILON);

        let def1 = def0 + GOO_LAYER_DEF_SIZE + layer0_len + 2;
        let exposure1 =
            f32::from_be_bytes(bytes[def1 + 10..def1 + 14].try_into().unwrap());
        assert!((exposure1 - 2.0).abs() < f32::EPSILON);

        // DataLength is the final field of each definition, followed by the
        // RLE payload and a trailing CRLF.
        let data_len0 =
            u32::from_be_bytes(bytes[def0 + 66..def0 + 70].try_into().unwrap()) as usize;
        assert_eq!(data_len0, layer0_len);
        assert_eq!(
            &bytes[def0 + GOO_LAYER_DEF_SIZE + data_len0..def0 + GOO_LAYER_DEF_SIZE + data_len0 + 2],
            &[0x0D, 0x0A]
        );
    }

    #[test]
    fn goo_rle_roundtrip_through_decoder() {
        let mut pixels = vec![0x80u8; 20];
        pixels.extend_from_slice(&[0x00; 5]);
        pixels.extend_from_slice(&[0xFF; 7]);
        pixels.extend_from_slice(&[0x42; 300]);

        let encoded = goo_rle_encode(&pixels);
        let decoded = decode_goo_rle(&encoded, pixels.len()).expect("decode should succeed");
        assert_eq!(decoded, pixels);
    }

    #[test]
    fn goo_rle_decoder_rejects_bad_checksum() {
        let mut encoded = goo_rle_encode(&[0xFF; 16]);
        let last = encoded.len() - 1;
        encoded[last] ^= 0xFF;
        assert!(decode_goo_rle(&encoded, 16).is_err());
    }

    // ── GOO V5.1 ──────────────────────────────────────────────────────────

    use super::goo_metadata::{parse_goo_format_version_hint, GooFormatVersion};
    use super::goo_types::{
        GooV5PreparedLayer, GOO_V5_FOOTER_BLOB_SIZE, GOO_V5_LDT_MARKER,
        GOO_V5_LDT_START, GOO_V5_MAGIC, GOO_V5_PARAMS_OFFSET, GOO_V5_PREAMBLE_OFFSET,
    };
    use super::goo_v5::{build_goo_v5_container_bytes, write_goo_v5_t2};
    use super::goo_v5_rle::{decode_panel_binary, decode_panel_vuf, encode_panel_from_mask_window};

    fn u32_le(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn f32_le(bytes: &[u8], offset: usize) -> f32 {
        f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn make_v5_test_job() -> SliceJobV3 {
        let mut job = make_test_job();
        job.format_version = Some("v5.1".to_string());
        job.metadata_json = r#"{
            "material": {
                "normalExposureSec": 2.0,
                "bottomExposureSec": 20.0,
                "bottomLayerCount": 1
            }
        }"#
        .to_string();
        job
    }

    fn make_v5_prepared(job: &SliceJobV3, masks: &[Vec<u8>]) -> Vec<GooV5PreparedLayer> {
        let settings = super::goo_metadata::parse_goo_v5_settings_from_job(job);
        let full_width = job.source_width_px as usize;
        let height = job.source_height_px as usize;
        let half = full_width / 2;
        masks
            .iter()
            .enumerate()
            .map(|(index, mask)| GooV5PreparedLayer {
                index,
                blocks: [(0usize, half), (half, full_width - half)]
                    .iter()
                    .map(|&(start, w)| {
                        encode_panel_from_mask_window(
                            mask, full_width, height, start, w, &settings, 127, false,
                        )
                    })
                    .collect(),
            })
            .collect()
    }

    #[test]
    fn goo_v5_hint_parses_supported_tokens() {
        assert_eq!(
            parse_goo_format_version_hint(Some("v5.1")),
            Some(GooFormatVersion::V51)
        );
        assert_eq!(
            parse_goo_format_version_hint(Some("V5")),
            Some(GooFormatVersion::V51)
        );
        assert_eq!(
            parse_goo_format_version_hint(Some("goo-v5.1")),
            Some(GooFormatVersion::V51)
        );
        assert_eq!(
            parse_goo_format_version_hint(Some("v1.2")),
            Some(GooFormatVersion::V12)
        );
        assert_eq!(parse_goo_format_version_hint(None), None);
        assert_eq!(parse_goo_format_version_hint(Some("unknown")), None);
    }

    #[test]
    fn goo_v5_container_layout_tables_and_md5() {
        let job = make_v5_test_job();
        // Layer 0: solid white. Layer 1: white band crossing the seam.
        let mask0 = vec![255u8; 16];
        let mut mask1 = vec![0u8; 16];
        for row in 0..4 {
            mask1[row * 4 + 1] = 255;
            mask1[row * 4 + 2] = 255;
        }
        let prepared = make_v5_prepared(&job, &[mask0.clone(), mask1.clone()]);
        let bytes = build_goo_v5_container_bytes(&job, &prepared).expect("container");

        // Optional dump for external reference-decoder validation.
        if let Ok(path) = std::env::var("GOO_V5_TEST_DUMP") {
            let _ = std::fs::write(path, &bytes);
        }

        // Header + identity strings (§16 — printer-validated).
        assert_eq!(&bytes[0..4], GOO_V5_MAGIC);
        assert_eq!(u32_le(&bytes, 4), 7);
        assert_eq!(&bytes[8..12], b"DLP\0");
        assert_eq!(&bytes[12..28], b"ELEGOO SatelLite");
        assert_eq!(&bytes[92..108], b"ELEGOO Jupiter 2");
        assert_eq!(&bytes[124..140], b"ELEGOO Jupiter 2");
        assert_eq!(&bytes[156..163], b"Normal\0");

        // Printer parameters.
        let params = GOO_V5_PARAMS_OFFSET as usize;
        assert_eq!(u32_le(&bytes, params), 2); // layer count
        assert_eq!(
            u16::from_le_bytes(bytes[params + 4..params + 6].try_into().unwrap()),
            4
        ); // resolution X
        assert_eq!(u32_le(&bytes, 195_470), GOO_V5_PREAMBLE_OFFSET); // offset of layer content

        // Preamble (§4.1).
        let pre = GOO_V5_PREAMBLE_OFFSET as usize;
        let ldt_start = GOO_V5_LDT_START as usize;
        let l = 2usize;
        let iedt = ldt_start + l * 8;
        let rdt_marker = ldt_start + l * 24 + 1;
        let defs_base = ldt_start + l * 24 + 14;
        assert_eq!(bytes[pre], 2); // PartitionCount
        assert_eq!(u32_le(&bytes, pre + 1), GOO_V5_LDT_MARKER);
        assert_eq!(u32_le(&bytes, pre + 5), iedt as u32);
        assert_eq!(u32_le(&bytes, pre + 9), rdt_marker as u32);
        assert_eq!(bytes[pre + 14], 8); // PixelBitWidth (VUF default)
        assert_eq!(bytes[pre + 15], 0xA1); // LDT magic

        // T1: [offset_to_def][66] per layer.
        assert_eq!(u32_le(&bytes, ldt_start), defs_base as u32);
        assert_eq!(u32_le(&bytes, ldt_start + 4), 66);
        assert_eq!(u32_le(&bytes, ldt_start + 8), (defs_base + 66) as u32);

        // T2: addr0 formula, page_size = block_bytes × 256, −162 after block 0.
        let block_sizes: Vec<usize> = prepared
            .iter()
            .flat_map(|p| p.blocks.iter().map(|b| b.len()))
            .collect();
        let addr0: u64 = 0x3A2 + 50_049_024 + 23_040 * 2;
        assert_eq!(u32_le(&bytes, iedt), addr0 as u32);
        assert_eq!(u32_le(&bytes, iedt + 4), (block_sizes[0] * 256) as u32);
        let addr1 = addr0 + (block_sizes[0] as u64) * 256 - 162;
        assert_eq!(u32_le(&bytes, iedt + 8), (addr1 & 0xFFFF_FFFF) as u32);

        // RDT pad: 00 A3 count=1 [end_of_rle][blob size].
        let pad = ldt_start + l * 24;
        let total_rle: usize = block_sizes.iter().sum();
        let end_of_rle = defs_base + l * 66 + total_rle;
        assert_eq!(bytes[pad], 0);
        assert_eq!(bytes[pad + 1], 0xA3);
        assert_eq!(u32_le(&bytes, pad + 2), 1);
        assert_eq!(u32_le(&bytes, pad + 6), end_of_rle as u32);
        assert_eq!(u32_le(&bytes, pad + 10), GOO_V5_FOOTER_BLOB_SIZE as u32);

        // Layer defs: bottom layer uses bottom exposure, CRLF-terminated.
        assert!((f32_le(&bytes, defs_base + 6) - 0.05).abs() < 1e-6); // position Z
        assert!((f32_le(&bytes, defs_base + 10) - 20.0).abs() < f32::EPSILON);
        assert_eq!(&bytes[defs_base + 64..defs_base + 66], &[0x0D, 0x0A]);
        assert!((f32_le(&bytes, defs_base + 66 + 10) - 2.0).abs() < f32::EPSILON);

        // Decode every panel via the T2 sizes and reassemble the layers.
        let rle_start = defs_base + l * 66;
        let mut pos = rle_start;
        let mut panels = Vec::new();
        for size in &block_sizes {
            panels.push(decode_panel_vuf(&bytes[pos..pos + size], 8, 255).expect("panel"));
            pos += size;
        }
        assert_eq!(pos, end_of_rle);
        for (layer, mask) in [(0usize, &mask0), (1usize, &mask1)] {
            let (left, right) = (&panels[layer * 2], &panels[layer * 2 + 1]);
            let mut stacked = Vec::with_capacity(16);
            for row in 0..4 {
                stacked.extend_from_slice(&left[row * 2..row * 2 + 2]);
                stacked.extend_from_slice(&right[row * 2..row * 2 + 2]);
            }
            let expected: Vec<u8> = mask
                .iter()
                .map(|&p| if p > 127 { 255 } else { 0 })
                .collect();
            assert_eq!(stacked, expected, "layer {layer} reassembly");
        }

        // Footer: profile blob + trailing lowercase MD5 over everything else.
        let blob_start = bytes.len() - 32 - GOO_V5_FOOTER_BLOB_SIZE;
        assert_eq!(blob_start, end_of_rle);
        assert_eq!(bytes[blob_start], 0x66);
        assert_eq!(&bytes[blob_start + 1..blob_start + 8], b"Normal\0");
        assert!((f32_le(&bytes, bytes.len() - 42) - 1.15).abs() < f32::EPSILON);
        assert_eq!(u32_le(&bytes, bytes.len() - 38), 0);
        assert_eq!(&bytes[bytes.len() - 34..bytes.len() - 32], &[0x0D, 0x0A]);

        use md5::{Digest, Md5};
        let digest = Md5::digest(&bytes[..bytes.len() - 32]);
        let mut hex = String::new();
        for byte in digest {
            use std::fmt::Write;
            let _ = write!(hex, "{:02x}", byte);
        }
        assert_eq!(&bytes[bytes.len() - 32..], hex.as_bytes());
    }

    #[test]
    fn goo_v5_binary_mode_selectable_via_metadata() {
        let mut job = make_v5_test_job();
        job.metadata_json = r#"{ "goo": { "v5RleMode": "binary" } }"#.to_string();

        let mut mask = vec![0u8; 16];
        mask[5] = 255;
        mask[6] = 255;
        let prepared = make_v5_prepared(&job, &[mask]);
        let bytes = build_goo_v5_container_bytes(&job, &prepared).expect("container");

        // PixelBitWidth defaults to 3 on the binary path.
        assert_eq!(bytes[GOO_V5_PREAMBLE_OFFSET as usize + 14], 3);

        // The panels decode with the binary grammar.
        let l = 1usize;
        let defs_base = GOO_V5_LDT_START as usize + l * 24 + 14;
        let iedt = GOO_V5_LDT_START as usize + l * 8;
        let size0 = (u32_le(&bytes, iedt + 4) / 256) as usize;
        let rle_start = defs_base + l * 66;
        // mask[5] = row 1, col 1 → left-panel index row 1 × 2 + 1 = 3.
        let left = decode_panel_binary(&bytes[rle_start..rle_start + size0], 8).expect("panel");
        assert_eq!(left, vec![0, 0, 0, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn goo_v5_t2_wrap_carries_into_page_size() {
        // Cumulative addresses crossing 2³² must keep clean low-32 addresses
        // while the wrap count rides in page_size (spec §5.1).
        let layer_count = 4u32;
        let sizes = vec![10_000_000usize; 8];
        let mut t2 = Vec::new();
        write_goo_v5_t2(&mut t2, &sizes, layer_count);
        assert_eq!(t2.len(), 8 * 8);

        let addr0: u128 = 0x3A2 + 50_049_024 + 23_040 * (layer_count as u128);
        let mut full: u128 = addr0;
        let mut saw_wrap = false;
        for (i, &size) in sizes.iter().enumerate() {
            let base = (size as u128) * 256;
            let addr = u32_le(&t2, i * 8);
            let page_size = u32_le(&t2, i * 8 + 4);
            assert_eq!(addr, (full & 0xFFFF_FFFF) as u32, "entry {i} addr");
            let wraps = (full >> 32) as u32;
            if wraps > 0 {
                saw_wrap = true;
            }
            assert_eq!(
                page_size,
                ((base as u64 + wraps as u64) & 0xFFFF_FFFF) as u32,
                "entry {i} page_size"
            );
            full += base;
            if i == 0 {
                full -= 162;
            }
        }
        assert!(saw_wrap, "test sizes should force at least one 2^32 wrap");
    }

    // ── V5.1 read_layer_preview_png round-trip tests ─────────────────────

    /// Decode a grayscale PNG payload into (width, height, pixels).
    fn decode_png_to_pixels(png_bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder
            .read_info()
            .map_err(|e| format!("PNG read_info: {e}"))?;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader
            .next_frame(&mut buf)
            .map_err(|e| format!("PNG next_frame: {e}"))?;
        let pixels = buf[..info.buffer_size()].to_vec();
        Ok((info.width, info.height, pixels))
    }

    #[test]
    fn goo_v5_read_layer_preview_png_vuf_round_trip() {
        let job = make_v5_test_job();
        // Layer 0: solid white. Layer 1: white band crossing the seam.
        let mask0 = vec![255u8; 16];
        let mut mask1 = vec![0u8; 16];
        for row in 0..4 {
            mask1[row * 4 + 1] = 255;
            mask1[row * 4 + 2] = 255;
        }
        let prepared = make_v5_prepared(&job, &[mask0.clone(), mask1.clone()]);
        let bytes = build_goo_v5_container_bytes(&job, &prepared).expect("container");

        let dir = std::env::temp_dir();
        let path = dir.join("goo_v5_preview_test_vuf.goo");
        std::fs::write(&path, &bytes).expect("write");

        // Layer 1
        let png1 = read_layer_preview_png(&path, 1).expect("layer 1 preview");
        let (w, h, pixels) = decode_png_to_pixels(&png1).expect("decode png");
        assert_eq!(w, 4);
        assert_eq!(h, 4);
        let expected: Vec<u8> = mask0.iter().map(|&p| if p > 127 { 255 } else { 0 }).collect();
        assert_eq!(pixels, expected, "layer 1 (solid white)");

        // Layer 2
        let png2 = read_layer_preview_png(&path, 2).expect("layer 2 preview");
        let (w2, h2, pixels2) = decode_png_to_pixels(&png2).expect("decode png");
        assert_eq!(w2, 4);
        assert_eq!(h2, 4);
        let expected2: Vec<u8> = mask1.iter().map(|&p| if p > 127 { 255 } else { 0 }).collect();
        assert_eq!(pixels2, expected2, "layer 2 (cross-seam band)");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn goo_v5_read_layer_preview_png_binary_round_trip() {
        let mut job = make_v5_test_job();
        job.metadata_json = r#"{
            "material": {
                "normalExposureSec": 2.0,
                "bottomExposureSec": 20.0,
                "bottomLayerCount": 1
            },
            "goo": { "v5RleMode": "binary" }
        }"#
        .to_string();

        let mut mask = vec![0u8; 16];
        mask[5] = 255;
        mask[6] = 255;
        let prepared = make_v5_prepared(&job, &[mask.clone()]);
        let bytes = build_goo_v5_container_bytes(&job, &prepared).expect("container");

        let dir = std::env::temp_dir();
        let path = dir.join("goo_v5_preview_test_binary.goo");
        std::fs::write(&path, &bytes).expect("write");

        let png = read_layer_preview_png(&path, 1).expect("binary layer preview");
        let (w, h, pixels) = decode_png_to_pixels(&png).expect("decode png");
        assert_eq!(w, 4);
        assert_eq!(h, 4);
        let expected: Vec<u8> =
            mask.iter().map(|&p| if p > 127 { 255 } else { 0 }).collect();
        assert_eq!(pixels, expected, "binary-mode layer 1");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn goo_v5_read_layer_preview_png_version_dispatch() {
        // Build both a V1.2 and V5.1 file with the same simple mask; both
        // should produce the same pixel output when decoded via
        // read_layer_preview_png.
        let pixels = (4 * 4) as usize;
        let mask = vec![255u8; pixels];
        let dir = std::env::temp_dir();

        // ── V1.2 ──────────────────────────────────────────────────────
        let job_v12 = make_test_job();
        let prepared_v12 = make_prepared_layers(&job_v12, 1);
        let bytes_v12 = build_goo_container_bytes(&job_v12, &prepared_v12).expect("v12");
        let path_v12 = dir.join("goo_preview_dispatch_v12.goo");
        std::fs::write(&path_v12, &bytes_v12).expect("write v12");

        let png_v12 = read_layer_preview_png(&path_v12, 1).expect("v12 layer 1");
        let (w12, h12, px12) = decode_png_to_pixels(&png_v12).expect("v12 png");
        assert_eq!((w12, h12), (4, 4));
        assert!(px12.iter().all(|&p| p == 255), "v12 all white");

        // ── V5.1 ──────────────────────────────────────────────────────
        let job_v51 = make_v5_test_job();
        let prepared_v51 = make_v5_prepared(&job_v51, &[mask.clone()]);
        let bytes_v51 = build_goo_v5_container_bytes(&job_v51, &prepared_v51).expect("v51");
        let path_v51 = dir.join("goo_preview_dispatch_v51.goo");
        std::fs::write(&path_v51, &bytes_v51).expect("write v51");

        let png_v51 = read_layer_preview_png(&path_v51, 1).expect("v51 layer 1");
        let (w51, h51, px51) = decode_png_to_pixels(&png_v51).expect("v51 png");
        assert_eq!((w51, h51), (4, 4));
        assert!(px51.iter().all(|&p| p == 255), "v51 all white");

        // Both versions produce the identical pixel buffer.
        assert_eq!(px12, px51);

        let _ = std::fs::remove_file(&path_v12);
        let _ = std::fs::remove_file(&path_v51);
    }

    #[test]
    fn goo_v5_read_layer_preview_png_error_cases() {
        let job = make_v5_test_job();
        let mask = vec![0u8; 16];
        let prepared = make_v5_prepared(&job, &[mask]);
        let bytes = build_goo_v5_container_bytes(&job, &prepared).expect("container");

        let dir = std::env::temp_dir();
        let path = dir.join("goo_v5_preview_errors.goo");
        std::fs::write(&path, &bytes).expect("write");

        // Layer 0
        assert!(read_layer_preview_png(&path, 0).is_err());

        // Out-of-range layer
        assert!(read_layer_preview_png(&path, 99).is_err());

        let _ = std::fs::remove_file(&path);

        // Non-existent file
        let bad = dir.join("goo_v5_nonexistent.goo");
        assert!(read_layer_preview_png(&bad, 1).is_err());
    }

    #[test]
    fn goo_v5_read_layer_preview_png_empty_layer() {
        // All-black layer: the encoder emits a single black run per panel.
        let job = make_v5_test_job();
        let mask = vec![0u8; 16];
        let prepared = make_v5_prepared(&job, &[mask.clone()]);
        let bytes = build_goo_v5_container_bytes(&job, &prepared).expect("container");

        let dir = std::env::temp_dir();
        let path = dir.join("goo_v5_preview_empty.goo");
        std::fs::write(&path, &bytes).expect("write");

        let png = read_layer_preview_png(&path, 1).expect("empty layer");
        let (w, h, pixels) = decode_png_to_pixels(&png).expect("decode png");
        assert_eq!((w, h), (4, 4));
        assert!(pixels.iter().all(|&p| p == 0), "all-black layer");

        let _ = std::fs::remove_file(&path);
    }
}
