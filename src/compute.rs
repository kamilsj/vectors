use std::fmt;

#[cfg(feature = "gpu")]
use std::sync::{Mutex, OnceLock};

use crate::engine::{DenseVectorColumn, FastVectorMetric};
use crate::{Error, Result, Vector};

const DEFAULT_GPU_MIN_ELEMENTS: usize = 8 * 1024 * 1024;
const DEFAULT_GPU_CACHE_BYTES: usize = 512 * 1024 * 1024;

/// Device preference used by exact vector scans.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ComputeDevice {
    /// Always use the CPU implementation.
    Cpu,
    /// Use a GPU for sufficiently large compatible scans, with CPU fallback.
    #[default]
    Auto,
    /// Require GPU execution for eligible vector scans.
    Gpu,
}

impl ComputeDevice {
    /// Parse `cpu`, `auto`, or `gpu` without regard to ASCII case.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cpu" => Some(Self::Cpu),
            "auto" => Some(Self::Auto),
            "gpu" => Some(Self::Gpu),
            _ => None,
        }
    }
}

impl fmt::Display for ComputeDevice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Cpu => "cpu",
            Self::Auto => "auto",
            Self::Gpu => "gpu",
        })
    }
}

/// Runtime thresholds and memory limits for vector scan execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComputeConfig {
    pub device: ComputeDevice,
    /// Minimum number of scalar multiply/add operations before `auto` tries a GPU.
    pub gpu_min_elements: usize,
    /// Maximum resident GPU memory used for a cached dense vector column.
    pub gpu_cache_bytes: usize,
}

impl Default for ComputeConfig {
    fn default() -> Self {
        Self {
            device: ComputeDevice::Auto,
            gpu_min_elements: DEFAULT_GPU_MIN_ELEMENTS,
            gpu_cache_bytes: DEFAULT_GPU_CACHE_BYTES,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ComputeRuntime {
    config: ComputeConfig,
    #[cfg(feature = "gpu")]
    gpu: OnceLock<Mutex<GpuState>>,
}

#[cfg(feature = "gpu")]
#[derive(Debug)]
enum GpuState {
    Uninitialized,
    Ready(crate::gpu::GpuExecutor),
    Unavailable(String),
}

impl ComputeRuntime {
    pub(crate) fn new(config: ComputeConfig) -> Self {
        Self {
            config,
            #[cfg(feature = "gpu")]
            gpu: OnceLock::new(),
        }
    }

    pub(crate) fn config(&self) -> &ComputeConfig {
        &self.config
    }

    /// Stream GPU scores into a query-local consumer without allocating a
    /// request-sized result vector. Returns `false` when automatic mode wants
    /// the caller to discard its partial GPU accumulator and run on the CPU.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn scan_dense_column(
        &self,
        column: &DenseVectorColumn,
        rows: Option<&[usize]>,
        metric: FastVectorMetric,
        query: &Vector,
        candidate_count: usize,
        mut consume: impl FnMut(usize, Option<f64>),
    ) -> Result<bool> {
        if self.config.device == ComputeDevice::Cpu || candidate_count == 0 {
            return Ok(false);
        }
        let elements = candidate_count.saturating_mul(column.dimensions);
        if self.config.device == ComputeDevice::Auto && elements < self.config.gpu_min_elements {
            return Ok(false);
        }

        #[cfg(not(feature = "gpu"))]
        {
            let _ = (column, rows, metric, query, &mut consume);
            match self.config.device {
                ComputeDevice::Gpu => Err(Error::GpuUnavailable(
                    "this binary was built without the 'gpu' Cargo feature".into(),
                )),
                ComputeDevice::Cpu | ComputeDevice::Auto => Ok(false),
            }
        }

        #[cfg(feature = "gpu")]
        {
            let state = self.gpu.get_or_init(|| Mutex::new(GpuState::Uninitialized));
            let mut state = state.lock().map_err(|_| Error::LockPoisoned)?;
            if matches!(*state, GpuState::Uninitialized) {
                *state = match crate::gpu::GpuExecutor::new(self.config.gpu_cache_bytes) {
                    Ok(executor) => GpuState::Ready(executor),
                    Err(reason) => GpuState::Unavailable(reason.to_string()),
                };
            }
            let outcome = match &mut *state {
                GpuState::Ready(executor) => {
                    executor.score(column, rows, metric, query, &mut consume)
                }
                GpuState::Unavailable(_) if self.config.device == ComputeDevice::Auto => {
                    return Ok(false)
                }
                GpuState::Unavailable(reason) => return Err(Error::GpuUnavailable(reason.clone())),
                GpuState::Uninitialized => unreachable!("GPU state initialized above"),
            };
            match outcome {
                Ok(()) => Ok(true),
                Err(crate::gpu::GpuError::ZeroNorm) => Err(Error::ZeroNorm),
                Err(crate::gpu::GpuError::InvalidInput(reason)) => Err(Error::InvalidQuery(reason)),
                Err(reason) if reason.is_fallback_safe() => {
                    let reset_device = matches!(reason, crate::gpu::GpuError::Execution(_));
                    let reason = reason.to_string();
                    if reset_device {
                        // A failed submission can mean device loss. Re-open the
                        // adapter on the next eligible query instead of keeping
                        // a permanently broken executor and stale buffers.
                        *state = GpuState::Uninitialized;
                    }
                    if self.config.device == ComputeDevice::Auto {
                        Ok(false)
                    } else {
                        Err(Error::GpuUnavailable(reason))
                    }
                }
                Err(reason) => Err(Error::GpuUnavailable(reason.to_string())),
            }
        }
    }
}

impl Default for ComputeRuntime {
    fn default() -> Self {
        Self::new(ComputeConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_compute_device_names() {
        assert_eq!(ComputeDevice::parse("AUTO"), Some(ComputeDevice::Auto));
        assert_eq!(ComputeDevice::parse(" cpu "), Some(ComputeDevice::Cpu));
        assert_eq!(ComputeDevice::parse("gpu"), Some(ComputeDevice::Gpu));
        assert_eq!(ComputeDevice::parse("accelerator"), None);
    }
}
