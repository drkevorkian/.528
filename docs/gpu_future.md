# GPU acceleration (future)

Trait stubs live in `libsrs_video::srsv2::gpu_traits`:

- `GpuVideoAccelerator` / `CpuVideoAccelerator`
- `ColorConvertBackend`
- `MotionSearchBackend`
- `TransformBackend`
- `QuantBackend`

Cargo features `gpu-wgpu` and `gpu-cuda` are reserved; no kernels ship until CPU parity tests are authoritative.

Intended offload order once enabled:

1. RGB/BGRA ↔ YCbCr and chroma subsample.
2. Block activity / variance maps.
3. SAD/SATD motion search.
4. Transform + quant/dequant.
5. Deblocking where identical to CPU golden outputs.
