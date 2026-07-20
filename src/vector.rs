use std::fmt;
use std::sync::Arc;

use crate::{Error, Result};

/// Upper bound used to reject accidental or malicious resource exhaustion.
pub const MAX_VECTOR_DIMENSIONS: usize = 65_535;

/// An immutable, cheaply cloneable, contiguous vector of `f32` values.
///
/// The hot loops use plain indexed chunks, which LLVM can auto-vectorize without
/// requiring a CPU-specific target feature or unsafe code.
#[derive(Clone, Debug)]
pub struct Vector {
    values: Arc<[f32]>,
    norm: f64,
}

impl Vector {
    pub fn new(values: Vec<f32>) -> Result<Self> {
        if values.is_empty() {
            return Err(Error::InvalidVectorDimension);
        }
        if values.len() > MAX_VECTOR_DIMENSIONS {
            return Err(Error::VectorDimensionLimit {
                found: values.len(),
                max: MAX_VECTOR_DIMENSIONS,
            });
        }
        if let Some((index, _)) = values
            .iter()
            .enumerate()
            .find(|(_, value)| !value.is_finite())
        {
            return Err(Error::NonFiniteVectorElement { index });
        }
        let squared_norm = values
            .iter()
            .map(|value| {
                let value = f64::from(*value);
                value * value
            })
            .sum::<f64>();
        Ok(Self {
            values: values.into(),
            norm: squared_norm.sqrt(),
        })
    }

    #[inline]
    pub fn dimensions(&self) -> usize {
        self.values.len()
    }

    #[inline]
    pub fn as_slice(&self) -> &[f32] {
        &self.values
    }

    /// Euclidean norm cached when the vector is constructed.
    #[inline]
    pub fn norm(&self) -> f64 {
        self.norm
    }

    /// Return a unit-length copy of this vector.
    pub fn normalized(&self) -> Result<Self> {
        if self.norm == 0.0 {
            return Err(Error::ZeroNorm);
        }
        Self::new(
            self.values
                .iter()
                .map(|value| (f64::from(*value) / self.norm) as f32)
                .collect(),
        )
    }

    /// Squared Euclidean distance. Useful when only relative ordering matters.
    #[inline]
    pub fn squared_l2_distance(&self, other: &Self) -> Result<f32> {
        self.ensure_same_dimensions(other)?;
        let mut sum = 0.0_f32;
        let mut index = 0;

        // A fixed-width loop gives the optimizer a straightforward SIMD target.
        while index + 4 <= self.values.len() {
            let d0 = self.values[index] - other.values[index];
            let d1 = self.values[index + 1] - other.values[index + 1];
            let d2 = self.values[index + 2] - other.values[index + 2];
            let d3 = self.values[index + 3] - other.values[index + 3];
            sum += d0 * d0 + d1 * d1 + d2 * d2 + d3 * d3;
            index += 4;
        }
        while index < self.values.len() {
            let difference = self.values[index] - other.values[index];
            sum += difference * difference;
            index += 1;
        }
        Ok(sum)
    }

    pub fn l2_distance(&self, other: &Self) -> Result<f32> {
        Ok(self.squared_l2_distance(other)?.sqrt())
    }

    #[inline]
    pub fn dot_product(&self, other: &Self) -> Result<f32> {
        self.ensure_same_dimensions(other)?;
        let mut sum = 0.0_f32;
        let mut index = 0;
        while index + 4 <= self.values.len() {
            sum += self.values[index] * other.values[index]
                + self.values[index + 1] * other.values[index + 1]
                + self.values[index + 2] * other.values[index + 2]
                + self.values[index + 3] * other.values[index + 3];
            index += 4;
        }
        while index < self.values.len() {
            sum += self.values[index] * other.values[index];
            index += 1;
        }
        Ok(sum)
    }

    /// Cosine distance in the range `[0, 2]` (subject to floating-point error).
    #[inline]
    pub fn cosine_distance(&self, other: &Self) -> Result<f32> {
        self.ensure_same_dimensions(other)?;
        if self.norm == 0.0 || other.norm == 0.0 {
            return Err(Error::ZeroNorm);
        }
        let mut dot = 0.0_f64;
        let mut index = 0;
        while index + 4 <= self.values.len() {
            dot += f64::from(self.values[index]) * f64::from(other.values[index])
                + f64::from(self.values[index + 1]) * f64::from(other.values[index + 1])
                + f64::from(self.values[index + 2]) * f64::from(other.values[index + 2])
                + f64::from(self.values[index + 3]) * f64::from(other.values[index + 3]);
            index += 4;
        }
        while index < self.values.len() {
            dot += f64::from(self.values[index]) * f64::from(other.values[index]);
            index += 1;
        }
        Ok((1.0 - dot / (self.norm * other.norm)) as f32)
    }

    #[inline]
    fn ensure_same_dimensions(&self, other: &Self) -> Result<()> {
        if self.dimensions() != other.dimensions() {
            return Err(Error::DimensionMismatch {
                left: self.dimensions(),
                right: other.dimensions(),
            });
        }
        Ok(())
    }
}

impl PartialEq for Vector {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
    }
}

impl TryFrom<Vec<f32>> for Vector {
    type Error = Error;

    fn try_from(value: Vec<f32>) -> Result<Self> {
        Self::new(value)
    }
}

impl fmt::Display for Vector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[")?;
        for (index, value) in self.values.iter().enumerate() {
            if index != 0 {
                formatter.write_str(", ")?;
            }
            write!(formatter, "{value}")?;
        }
        formatter.write_str("]")
    }
}
