use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::mem::{size_of, size_of_val};
use std::sync::mpsc;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::engine::{DenseVectorColumn, FastVectorMetric};
use crate::Vector;

const WORKGROUP_SIZE: u32 = 128;
const MAX_READBACK_BYTES: usize = 32 * 1024 * 1024;
const REQUIRED_STORAGE_BINDINGS: u32 = 6;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuParameters {
    dimensions: u32,
    candidate_count: u32,
    row_count: u32,
    metric: u32,
    indexed: u32,
    query_norm: f32,
    dispatch_width: u32,
    candidate_offset: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuResult {
    score: f32,
    state: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GpuError {
    Unavailable(String),
    Limit(String),
    Execution(String),
    InvalidInput(String),
    ZeroNorm,
}

impl GpuError {
    pub(crate) fn is_fallback_safe(&self) -> bool {
        matches!(
            self,
            Self::Unavailable(_) | Self::Limit(_) | Self::Execution(_)
        )
    }
}

impl fmt::Display for GpuError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(reason) => formatter.write_str(reason),
            Self::Limit(reason) => write!(formatter, "GPU device limit: {reason}"),
            Self::Execution(reason) => write!(formatter, "GPU execution failed: {reason}"),
            Self::InvalidInput(reason) => write!(formatter, "invalid GPU scan input: {reason}"),
            Self::ZeroNorm => formatter.write_str("cosine distance is undefined for a zero vector"),
        }
    }
}

#[derive(Debug)]
struct CachedColumnShard {
    first_row: usize,
    row_count: usize,
    values: wgpu::Buffer,
    norms: wgpu::Buffer,
    present: wgpu::Buffer,
    cosine_supported: bool,
}

#[derive(Debug)]
struct CachedColumn {
    storage_id: u64,
    dimensions: usize,
    row_count: usize,
    rows_per_shard: usize,
    shards: Vec<CachedColumnShard>,
    bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DispatchShape {
    x: u32,
    y: u32,
    width: u32,
}

#[derive(Clone, Copy, Debug)]
struct ColumnLayout {
    bytes: usize,
    rows_per_shard: usize,
}

#[derive(Debug)]
pub(crate) struct GpuExecutor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    adapter_name: String,
    max_cache_bytes: usize,
    max_buffer_size: u64,
    max_storage_binding_size: u64,
    max_uniform_binding_size: u64,
    max_compute_workgroups: u32,
    max_candidates_per_batch: usize,
    cached_bytes: usize,
    cached: VecDeque<CachedColumn>,
}

impl GpuExecutor {
    pub(crate) fn new(max_cache_bytes: usize) -> Result<Self, GpuError> {
        if max_cache_bytes == 0 {
            return Err(GpuError::Unavailable(
                "GPU cache size must be greater than zero".into(),
            ));
        }
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .map_err(|error| {
            GpuError::Unavailable(format!("no compatible hardware adapter: {error}"))
        })?;
        let adapter_name = adapter.get_info().name;
        let limits = adapter.limits();
        validate_adapter_limits(&limits).map_err(GpuError::Unavailable)?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("vectors compute device"),
            required_features: wgpu::Features::empty(),
            required_limits: limits.clone(),
            ..Default::default()
        }))
        .map_err(|error| {
            GpuError::Unavailable(format!("cannot open adapter '{adapter_name}': {error}"))
        })?;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vectors exact scan"),
            source: wgpu::ShaderSource::Wgsl(include_str!("vector_scan.wgsl").into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("vectors exact scan"),
            layout: None,
            module: &shader,
            entry_point: Some("scan"),
            compilation_options: Default::default(),
            cache: None,
        });
        let max_buffer_size = limits.max_buffer_size;
        let max_storage_binding_size = limits.max_storage_buffer_binding_size;
        let max_output_bytes = MAX_READBACK_BYTES
            .min(usize_from_u64(max_buffer_size))
            .min(usize_from_u64(max_storage_binding_size));
        let max_candidates_per_batch = max_output_bytes / size_of::<GpuResult>();
        if max_candidates_per_batch == 0 {
            return Err(GpuError::Unavailable(
                "adapter cannot allocate a non-empty GPU result buffer".into(),
            ));
        }
        Ok(Self {
            device,
            queue,
            pipeline,
            adapter_name,
            max_cache_bytes,
            max_buffer_size,
            max_storage_binding_size,
            max_uniform_binding_size: limits.max_uniform_buffer_binding_size,
            max_compute_workgroups: limits.max_compute_workgroups_per_dimension,
            max_candidates_per_batch,
            cached_bytes: 0,
            cached: VecDeque::new(),
        })
    }

    /// Stream one exact score per candidate to `consume` in bounded batches.
    ///
    /// An error may occur after earlier batches have reached the callback. The
    /// caller must therefore keep GPU results in a query-local accumulator and
    /// discard that accumulator before taking an automatic CPU fallback.
    pub(crate) fn score(
        &mut self,
        column: &DenseVectorColumn,
        rows: Option<&[usize]>,
        metric: FastVectorMetric,
        query: &Vector,
        mut consume: impl FnMut(usize, Option<f64>),
    ) -> Result<(), GpuError> {
        let candidate_count = rows.map_or(column.row_count, <[usize]>::len);
        if candidate_count == 0 {
            return Ok(());
        }
        if query.dimensions() != column.dimensions {
            return Err(GpuError::InvalidInput(format!(
                "query has {} dimensions but the column has {}",
                query.dimensions(),
                column.dimensions
            )));
        }
        if let Some(rows) = rows {
            if let Some(row) = rows.iter().find(|row| **row >= column.row_count) {
                return Err(GpuError::InvalidInput(format!(
                    "candidate row {row} is outside a column with {} rows",
                    column.row_count
                )));
            }
        }

        self.ensure_cached(column)?;
        let cached = self
            .cached
            .front()
            .ok_or_else(|| GpuError::Execution("column cache entry disappeared".into()))?;

        if matches!(metric, FastVectorMetric::Cosine) {
            let query_norm = query.norm() as f32;
            if !query_norm.is_finite() {
                return Err(GpuError::Limit(
                    "query norm exceeds the shader's f32 numeric range".into(),
                ));
            }
            let unsupported_shard = match rows {
                Some(rows) => rows.iter().any(|row| {
                    let shard = row / cached.rows_per_shard;
                    !cached.shards[shard].cosine_supported
                }),
                None => cached.shards.iter().any(|shard| !shard.cosine_supported),
            };
            if unsupported_shard {
                return Err(GpuError::Limit(
                    "a stored vector norm exceeds the shader's f32 numeric range".into(),
                ));
            }
        }

        if !self.fits_storage_buffer(size_of_val(query.as_slice())) {
            return Err(GpuError::Limit(
                "query vector exceeds the maximum storage-buffer binding".into(),
            ));
        }
        let query_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("vectors query vector"),
                contents: bytemuck::cast_slice(query.as_slice()),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let placeholder_candidates =
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("vectors unindexed candidate placeholder"),
                    contents: bytemuck::cast_slice(&[0_u32]),
                    usage: wgpu::BufferUsages::STORAGE,
                });

        match rows {
            None => {
                for shard in &cached.shards {
                    let mut offset = 0;
                    while offset < shard.row_count {
                        let count = (shard.row_count - offset).min(self.max_candidates_per_batch);
                        self.scan_batch(
                            shard,
                            None,
                            offset,
                            count,
                            metric,
                            query,
                            &query_buffer,
                            &placeholder_candidates,
                            &mut consume,
                        )?;
                        offset += count;
                    }
                }
            }
            Some(rows) => {
                // Group only one bounded input window at a time. This avoids a
                // second request-sized allocation for very large indexed scans.
                for window in rows.chunks(self.max_candidates_per_batch) {
                    let mut by_shard: BTreeMap<usize, Vec<u32>> = BTreeMap::new();
                    for row in window {
                        let shard_index = row / cached.rows_per_shard;
                        let local_row = row - cached.shards[shard_index].first_row;
                        let local_row = u32::try_from(local_row).map_err(|_| {
                            GpuError::InvalidInput(format!(
                                "candidate row {row} exceeds a GPU shard's address space"
                            ))
                        })?;
                        by_shard.entry(shard_index).or_default().push(local_row);
                    }
                    for (shard_index, candidates) in by_shard {
                        let candidate_buffer =
                            self.device
                                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                                    label: Some("vectors indexed candidate rows"),
                                    contents: bytemuck::cast_slice(&candidates),
                                    usage: wgpu::BufferUsages::STORAGE,
                                });
                        self.scan_batch(
                            &cached.shards[shard_index],
                            Some(&candidates),
                            0,
                            candidates.len(),
                            metric,
                            query,
                            &query_buffer,
                            &candidate_buffer,
                            &mut consume,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_batch(
        &self,
        shard: &CachedColumnShard,
        candidates: Option<&[u32]>,
        candidate_offset: usize,
        candidate_count: usize,
        metric: FastVectorMetric,
        query: &Vector,
        query_buffer: &wgpu::Buffer,
        candidate_buffer: &wgpu::Buffer,
        consume: &mut impl FnMut(usize, Option<f64>),
    ) -> Result<(), GpuError> {
        if candidate_count == 0 || candidate_count > self.max_candidates_per_batch {
            return Err(GpuError::InvalidInput(format!(
                "GPU batch contains {candidate_count} candidates"
            )));
        }
        if candidates.is_some_and(|rows| rows.len() != candidate_count) {
            return Err(GpuError::InvalidInput(
                "candidate buffer length does not match the GPU batch".into(),
            ));
        }
        let candidate_count_u32 = u32::try_from(candidate_count)
            .map_err(|_| GpuError::Limit("candidate batch exceeds u32 address space".into()))?;
        let candidate_offset_u32 = u32::try_from(candidate_offset)
            .map_err(|_| GpuError::Limit("candidate offset exceeds u32 address space".into()))?;
        let dimensions = u32::try_from(query.dimensions())
            .map_err(|_| GpuError::Limit("vector dimensions exceed u32 address space".into()))?;
        let row_count = u32::try_from(shard.row_count)
            .map_err(|_| GpuError::Limit("GPU shard exceeds u32 address space".into()))?;
        let dispatch = dispatch_shape(candidate_count_u32, self.max_compute_workgroups)?;
        let parameters = GpuParameters {
            dimensions,
            candidate_count: candidate_count_u32,
            row_count,
            metric: metric.gpu_code(),
            indexed: u32::from(candidates.is_some()),
            query_norm: query.norm() as f32,
            dispatch_width: dispatch.width,
            candidate_offset: candidate_offset_u32,
        };
        if !self.fits_uniform_buffer(size_of::<GpuParameters>()) {
            return Err(GpuError::Limit(
                "scan parameters exceed the maximum uniform-buffer binding".into(),
            ));
        }
        let parameter_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("vectors scan parameters"),
                contents: bytemuck::bytes_of(&parameters),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let output_bytes = candidate_count
            .checked_mul(size_of::<GpuResult>())
            .ok_or_else(|| GpuError::Limit("GPU output size overflow".into()))?;
        if !self.fits_storage_buffer(output_bytes) || !self.fits_buffer(output_bytes) {
            return Err(GpuError::Limit(
                "result batch exceeds the maximum GPU buffer size".into(),
            ));
        }
        if candidates.is_some() && !self.fits_storage_buffer(candidate_count * size_of::<u32>()) {
            return Err(GpuError::Limit(
                "candidate batch exceeds the maximum storage-buffer binding".into(),
            ));
        }
        let output = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vectors scan output"),
            size: output_bytes as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vectors scan readback"),
            size: output_bytes as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let layout = self.pipeline.get_bind_group_layout(0);
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vectors scan bindings"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: shard.values.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: shard.norms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: shard.present.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: candidate_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: query_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: parameter_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: output.as_entire_binding(),
                },
            ],
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vectors exact scan"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("vectors exact scan"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(dispatch.x, dispatch.y, 1);
        }
        encoder.copy_buffer_to_buffer(&output, 0, &readback, 0, output_bytes as u64);
        let submission = self.queue.submit([encoder.finish()]);

        let (sender, receiver) = mpsc::sync_channel(1);
        readback.map_async(wgpu::MapMode::Read, .., move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: None,
            })
            .map_err(|error| {
                GpuError::Execution(format!(
                    "adapter '{}' did not complete: {error}",
                    self.adapter_name
                ))
            })?;
        receiver
            .recv()
            .map_err(|_| GpuError::Execution("readback callback was dropped".into()))?
            .map_err(|error| GpuError::Execution(format!("readback failed: {error}")))?;

        let mapped = readback.get_mapped_range(..);
        let decoded = (|| {
            let raw: &[GpuResult] = bytemuck::try_cast_slice(&mapped).map_err(|error| {
                GpuError::Execution(format!("result buffer has an invalid layout: {error}"))
            })?;
            if raw.len() != candidate_count {
                return Err(GpuError::Execution(format!(
                    "GPU returned {} scores for {candidate_count} candidates",
                    raw.len()
                )));
            }
            for (candidate, result) in raw.iter().enumerate() {
                match result.state {
                    0 => {
                        let row = candidates.map_or(candidate_offset + candidate, |rows| {
                            rows[candidate] as usize
                        });
                        consume(shard.first_row + row, None);
                    }
                    1 if result.score.is_finite() => {
                        let row = candidates.map_or(candidate_offset + candidate, |rows| {
                            rows[candidate] as usize
                        });
                        consume(shard.first_row + row, Some(f64::from(result.score)));
                    }
                    2 => return Err(GpuError::ZeroNorm),
                    1 => {
                        return Err(GpuError::Execution(
                            "shader returned a non-finite vector score".into(),
                        ))
                    }
                    state => {
                        return Err(GpuError::Execution(format!(
                            "shader returned unknown result state {state}"
                        )))
                    }
                }
            }
            Ok(())
        })();
        drop(mapped);
        readback.unmap();
        decoded
    }

    fn ensure_cached(&mut self, column: &DenseVectorColumn) -> Result<(), GpuError> {
        if let Some(position) = self.cached.iter().position(|cached| {
            cached.storage_id == column.storage_id
                && cached.dimensions == column.dimensions
                && cached.row_count == column.row_count
        }) {
            if position != 0 {
                let cached = self.cached.remove(position).ok_or_else(|| {
                    GpuError::Execution("LRU cache entry disappeared during promotion".into())
                })?;
                self.cached.push_front(cached);
            }
            return Ok(());
        }

        let layout = self.column_layout(column)?;
        if layout.bytes > self.max_cache_bytes {
            return Err(GpuError::Limit(format!(
                "dense column needs {} bytes but the cache limit is {} bytes",
                layout.bytes, self.max_cache_bytes
            )));
        }
        while self.cached_bytes > self.max_cache_bytes - layout.bytes {
            let evicted = self.cached.pop_back().ok_or_else(|| {
                GpuError::Execution("GPU cache accounting is inconsistent".into())
            })?;
            self.cached_bytes = self
                .cached_bytes
                .checked_sub(evicted.bytes)
                .ok_or_else(|| GpuError::Execution("GPU cache byte counter underflowed".into()))?;
        }

        let cached = self.upload_column(column, layout)?;
        self.cached_bytes = self
            .cached_bytes
            .checked_add(cached.bytes)
            .ok_or_else(|| GpuError::Execution("GPU cache byte counter overflowed".into()))?;
        self.cached.push_front(cached);
        debug_assert!(self.cached_bytes <= self.max_cache_bytes);
        Ok(())
    }

    fn column_layout(&self, column: &DenseVectorColumn) -> Result<ColumnLayout, GpuError> {
        if column.dimensions == 0 || column.row_count == 0 {
            return Err(GpuError::InvalidInput(
                "dense vector column must have rows and dimensions".into(),
            ));
        }
        validate_dense_chunks(column)?;
        let bytes_per_row = column
            .dimensions
            .checked_mul(size_of::<f32>())
            .ok_or_else(|| GpuError::Limit("vector row size overflow".into()))?;
        let values_bytes = column
            .row_count
            .checked_mul(bytes_per_row)
            .ok_or_else(|| GpuError::Limit("dense vector column size overflow".into()))?;
        let metadata_bytes = column
            .row_count
            .checked_mul(size_of::<f32>() + size_of::<u32>())
            .ok_or_else(|| GpuError::Limit("dense vector metadata size overflow".into()))?;
        let bytes = values_bytes
            .checked_add(metadata_bytes)
            .ok_or_else(|| GpuError::Limit("GPU cache size overflow".into()))?;
        let max_storage =
            usize_from_u64(self.max_storage_binding_size).min(usize_from_u64(self.max_buffer_size));
        let rows_per_shard = (max_storage / bytes_per_row)
            .min(max_storage / size_of::<f32>())
            .min(max_storage / size_of::<u32>())
            .min(u32::MAX as usize);
        if rows_per_shard == 0 {
            return Err(GpuError::Limit(format!(
                "one {}-dimension vector exceeds the maximum storage-buffer binding",
                column.dimensions
            )));
        }
        Ok(ColumnLayout {
            bytes,
            rows_per_shard,
        })
    }

    fn upload_column(
        &self,
        column: &DenseVectorColumn,
        layout: ColumnLayout,
    ) -> Result<CachedColumn, GpuError> {
        let mut shards = Vec::with_capacity(column.row_count.div_ceil(layout.rows_per_shard));
        let mut chunk_index = 0;
        let mut chunk_row = 0;
        let mut first_row = 0;
        while first_row < column.row_count {
            let row_count = (column.row_count - first_row).min(layout.rows_per_shard);
            let values_bytes = checked_row_bytes(row_count, column.dimensions)?;
            let norms_bytes = row_count
                .checked_mul(size_of::<f32>())
                .ok_or_else(|| GpuError::Limit("norm buffer size overflow".into()))?;
            let present_bytes = row_count
                .checked_mul(size_of::<u32>())
                .ok_or_else(|| GpuError::Limit("presence buffer size overflow".into()))?;
            let values = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("vectors dense column shard"),
                size: values_bytes as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let norms = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("vectors dense norm shard"),
                size: norms_bytes as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let present = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("vectors dense presence shard"),
                size: present_bytes as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });

            let mut written = 0;
            let mut cosine_supported = true;
            while written < row_count {
                let chunk = column.chunks.get(chunk_index).ok_or_else(|| {
                    GpuError::InvalidInput("dense vector chunks ended before row_count".into())
                })?;
                let available = chunk.row_count - chunk_row;
                let take = available.min(row_count - written);
                let value_start = chunk_row * column.dimensions;
                let value_end = value_start + take * column.dimensions;
                let values_offset = checked_row_bytes(written, column.dimensions)?;
                self.queue.write_buffer(
                    &values,
                    values_offset as u64,
                    bytemuck::cast_slice(&chunk.values[value_start..value_end]),
                );

                let mut packed_norms = Vec::with_capacity(take);
                let mut packed_present = Vec::with_capacity(take);
                for local_row in chunk_row..chunk_row + take {
                    let norm = chunk.norms[local_row];
                    if !norm.is_finite() || norm < 0.0 {
                        return Err(GpuError::InvalidInput(format!(
                            "dense vector row {} has invalid cached norm {norm}",
                            chunk.first_row + local_row
                        )));
                    }
                    let norm = norm as f32;
                    cosine_supported &= norm.is_finite();
                    packed_norms.push(norm);
                    packed_present.push(u32::from(chunk.present[local_row]));
                }
                self.queue.write_buffer(
                    &norms,
                    (written * size_of::<f32>()) as u64,
                    bytemuck::cast_slice(&packed_norms),
                );
                self.queue.write_buffer(
                    &present,
                    (written * size_of::<u32>()) as u64,
                    bytemuck::cast_slice(&packed_present),
                );
                written += take;
                chunk_row += take;
                if chunk_row == chunk.row_count {
                    chunk_index += 1;
                    chunk_row = 0;
                }
            }
            shards.push(CachedColumnShard {
                first_row,
                row_count,
                values,
                norms,
                present,
                cosine_supported,
            });
            first_row += row_count;
        }
        if chunk_index != column.chunks.len() || chunk_row != 0 {
            return Err(GpuError::InvalidInput(
                "dense vector chunks contain rows beyond row_count".into(),
            ));
        }
        Ok(CachedColumn {
            storage_id: column.storage_id,
            dimensions: column.dimensions,
            row_count: column.row_count,
            rows_per_shard: layout.rows_per_shard,
            shards,
            bytes: layout.bytes,
        })
    }

    fn fits_buffer(&self, bytes: usize) -> bool {
        u64::try_from(bytes).is_ok_and(|bytes| bytes > 0 && bytes <= self.max_buffer_size)
    }

    fn fits_storage_buffer(&self, bytes: usize) -> bool {
        u64::try_from(bytes).is_ok_and(|bytes| {
            bytes > 0 && bytes <= self.max_buffer_size && bytes <= self.max_storage_binding_size
        })
    }

    fn fits_uniform_buffer(&self, bytes: usize) -> bool {
        u64::try_from(bytes).is_ok_and(|bytes| {
            bytes > 0 && bytes <= self.max_buffer_size && bytes <= self.max_uniform_binding_size
        })
    }
}

fn validate_adapter_limits(limits: &wgpu::Limits) -> Result<(), String> {
    if limits.max_compute_workgroup_size_x < WORKGROUP_SIZE
        || limits.max_compute_invocations_per_workgroup < WORKGROUP_SIZE
    {
        return Err(format!(
            "adapter supports fewer than {WORKGROUP_SIZE} compute invocations per workgroup"
        ));
    }
    if limits.max_compute_workgroups_per_dimension == 0 {
        return Err("adapter reports no compute workgroups".into());
    }
    if limits.max_storage_buffers_per_shader_stage < REQUIRED_STORAGE_BINDINGS {
        return Err(format!(
            "adapter exposes {} storage buffers per shader stage; {REQUIRED_STORAGE_BINDINGS} are required",
            limits.max_storage_buffers_per_shader_stage
        ));
    }
    if limits.max_uniform_buffer_binding_size < size_of::<GpuParameters>() as u64 {
        return Err("adapter uniform-buffer binding is too small for scan parameters".into());
    }
    Ok(())
}

fn validate_dense_chunks(column: &DenseVectorColumn) -> Result<(), GpuError> {
    let mut expected_row = 0;
    for chunk in &column.chunks {
        let expected_values = chunk
            .row_count
            .checked_mul(column.dimensions)
            .ok_or_else(|| GpuError::InvalidInput("dense chunk size overflow".into()))?;
        if chunk.row_count == 0
            || chunk.first_row != expected_row
            || chunk.values.len() != expected_values
            || chunk.norms.len() != chunk.row_count
            || chunk.present.len() != chunk.row_count
        {
            return Err(GpuError::InvalidInput(format!(
                "dense vector chunk at row {} is not contiguous",
                chunk.first_row
            )));
        }
        expected_row = expected_row
            .checked_add(chunk.row_count)
            .ok_or_else(|| GpuError::InvalidInput("dense row count overflow".into()))?;
    }
    if expected_row != column.row_count {
        return Err(GpuError::InvalidInput(format!(
            "dense chunks contain {expected_row} rows but the column reports {}",
            column.row_count
        )));
    }
    Ok(())
}

fn checked_row_bytes(rows: usize, dimensions: usize) -> Result<usize, GpuError> {
    rows.checked_mul(dimensions)
        .and_then(|values| values.checked_mul(size_of::<f32>()))
        .ok_or_else(|| GpuError::Limit("dense vector buffer size overflow".into()))
}

fn dispatch_shape(candidate_count: u32, max_workgroups: u32) -> Result<DispatchShape, GpuError> {
    if candidate_count == 0 {
        return Err(GpuError::InvalidInput(
            "cannot dispatch an empty candidate batch".into(),
        ));
    }
    let max_x = max_workgroups.min(u32::MAX / WORKGROUP_SIZE);
    if max_x == 0 {
        return Err(GpuError::Limit(
            "adapter cannot address a compute workgroup".into(),
        ));
    }
    let total_workgroups = candidate_count.div_ceil(WORKGROUP_SIZE);
    let x = total_workgroups.min(max_x);
    let y = total_workgroups.div_ceil(x);
    if y > max_workgroups {
        return Err(GpuError::Limit(format!(
            "{candidate_count} candidates exceed the two-dimensional dispatch grid"
        )));
    }
    Ok(DispatchShape {
        x,
        y,
        width: x * WORKGROUP_SIZE,
    })
}

fn usize_from_u64(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_uses_second_dimension_after_x_is_full() {
        let shape = dispatch_shape(1_025, 4).unwrap();
        assert_eq!(
            shape,
            DispatchShape {
                x: 4,
                y: 3,
                width: 512
            }
        );
    }

    #[test]
    fn dispatch_rejects_candidates_beyond_the_two_dimensional_grid() {
        let error = dispatch_shape(2_049, 4).unwrap_err();
        assert!(matches!(error, GpuError::Limit(_)));
    }

    #[test]
    fn dispatch_handles_partial_workgroups() {
        assert_eq!(
            dispatch_shape(129, 65_535).unwrap(),
            DispatchShape {
                x: 2,
                y: 1,
                width: 256,
            }
        );
    }
}
