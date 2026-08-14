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
    values: Arc<Vec<f32>>,
    offset: usize,
    dimensions: usize,
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
        let dimensions = values.len();
        Ok(Self {
            values: Arc::new(values),
            offset: 0,
            dimensions,
            norm: squared_norm.sqrt(),
        })
    }

    pub(crate) fn from_shared(
        values: Arc<Vec<f32>>,
        offset: usize,
        dimensions: usize,
        norm: f64,
    ) -> Self {
        debug_assert!(dimensions > 0);
        debug_assert!(offset
            .checked_add(dimensions)
            .is_some_and(|end| end <= values.len()));
        Self {
            values,
            offset,
            dimensions,
            norm,
        }
    }

    #[inline]
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    #[inline]
    pub fn as_slice(&self) -> &[f32] {
        &self.values[self.offset..self.offset + self.dimensions]
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
            self.as_slice()
                .iter()
                .map(|value| (f64::from(*value) / self.norm) as f32)
                .collect(),
        )
    }

    /// Squared Euclidean distance. Useful when only relative ordering matters.
    #[inline]
    pub fn squared_l2_distance(&self, other: &Self) -> Result<f32> {
        self.ensure_same_dimensions(other)?;
        Ok(squared_l2(self.as_slice(), other.as_slice()))
    }

    #[inline]
    pub(crate) fn squared_l2_distance_slice(&self, other: &[f32]) -> Result<f32> {
        self.ensure_slice_dimensions(other)?;
        Ok(squared_l2(self.as_slice(), other))
    }

    pub fn l2_distance(&self, other: &Self) -> Result<f32> {
        Ok(self.squared_l2_distance(other)?.sqrt())
    }

    #[inline]
    pub fn dot_product(&self, other: &Self) -> Result<f32> {
        self.ensure_same_dimensions(other)?;
        Ok(dot_product(self.as_slice(), other.as_slice()))
    }

    #[inline]
    pub(crate) fn dot_product_slice(&self, other: &[f32]) -> Result<f32> {
        self.ensure_slice_dimensions(other)?;
        Ok(dot_product(self.as_slice(), other))
    }

    /// Cosine distance in the range `[0, 2]` (subject to floating-point error).
    #[inline]
    pub fn cosine_distance(&self, other: &Self) -> Result<f32> {
        self.ensure_same_dimensions(other)?;
        self.cosine_distance_slice(other.as_slice(), other.norm)
    }

    #[inline]
    pub(crate) fn cosine_distance_slice(&self, other: &[f32], other_norm: f64) -> Result<f32> {
        self.ensure_slice_dimensions(other)?;
        if self.norm == 0.0 || other_norm == 0.0 {
            return Err(Error::ZeroNorm);
        }
        let dot = dot_product_f64(self.as_slice(), other);
        Ok((1.0 - dot / (self.norm * other_norm)) as f32)
    }

    #[inline]
    fn ensure_slice_dimensions(&self, other: &[f32]) -> Result<()> {
        if self.dimensions() != other.len() {
            return Err(Error::DimensionMismatch {
                left: self.dimensions(),
                right: other.len(),
            });
        }
        Ok(())
    }

    #[inline]
    fn ensure_same_dimensions(&self, other: &Self) -> Result<()> {
        self.ensure_slice_dimensions(other.as_slice())
    }
}

#[inline]
fn squared_l2(left: &[f32], right: &[f32]) -> f32 {
    let mut sum = 0.0_f32;
    let mut index = 0;

    // A fixed-width loop gives the optimizer a straightforward SIMD target.
    while index + 4 <= left.len() {
        let d0 = left[index] - right[index];
        let d1 = left[index + 1] - right[index + 1];
        let d2 = left[index + 2] - right[index + 2];
        let d3 = left[index + 3] - right[index + 3];
        sum += d0 * d0 + d1 * d1 + d2 * d2 + d3 * d3;
        index += 4;
    }
    while index < left.len() {
        let difference = left[index] - right[index];
        sum += difference * difference;
        index += 1;
    }
    sum
}

#[inline]
fn dot_product(left: &[f32], right: &[f32]) -> f32 {
    let mut sum = 0.0_f32;
    let mut index = 0;
    while index + 4 <= left.len() {
        sum += left[index] * right[index]
            + left[index + 1] * right[index + 1]
            + left[index + 2] * right[index + 2]
            + left[index + 3] * right[index + 3];
        index += 4;
    }
    while index < left.len() {
        sum += left[index] * right[index];
        index += 1;
    }
    sum
}

#[inline]
fn dot_product_f64(left: &[f32], right: &[f32]) -> f64 {
    let mut dot = 0.0_f64;
    let mut index = 0;
    while index + 4 <= left.len() {
        dot += f64::from(left[index]) * f64::from(right[index])
            + f64::from(left[index + 1]) * f64::from(right[index + 1])
            + f64::from(left[index + 2]) * f64::from(right[index + 2])
            + f64::from(left[index + 3]) * f64::from(right[index + 3]);
        index += 4;
    }
    while index < left.len() {
        dot += f64::from(left[index]) * f64::from(right[index]);
        index += 1;
    }
    dot
}

impl PartialEq for Vector {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
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
        for (index, value) in self.as_slice().iter().enumerate() {
            if index != 0 {
                formatter.write_str(", ")?;
            }
            write!(formatter, "{value}")?;
        }
        formatter.write_str("]")
    }
}
