//! Future GPU / SIMD backends — **traits only** (no kernels in this crate).

#![allow(dead_code)]

/// Optional CUDA path (`gpu-cuda` feature — placeholder).
pub trait GpuVideoAccelerator: Send + Sync {}

/// Default CPU fallback implements the same conceptual hooks where applicable.
pub trait CpuVideoAccelerator: Send + Sync {}

pub trait ColorConvertBackend: Send + Sync {}
pub trait MotionSearchBackend: Send + Sync {}
pub trait TransformBackend: Send + Sync {}
pub trait QuantBackend: Send + Sync {}
