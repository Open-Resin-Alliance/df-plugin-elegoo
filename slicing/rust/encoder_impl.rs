mod goo_layout;
mod goo_metadata;
mod goo_preview;
mod goo_types;
mod goo_encoder;

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
use goo_metadata::{parse_threshold_from_job, parse_timing_model_from_job};
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
    use super::{build_goo_container_bytes, decode_goo_rle};
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
}
