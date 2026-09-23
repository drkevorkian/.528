use std::sync::{
    atomic::{AtomicU8, AtomicU64, Ordering},
    Arc, Mutex,
};

use eframe::{
    egui,
    egui_wgpu::{self, wgpu},
};
use libsrs_app_services::{MAX_VIDEO_PIXELS, MAX_VIDEO_SIDE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GpuPresenterHealth {
    Healthy,
    SurfaceRecovering,
    BackendUnavailable,
}

#[derive(Debug)]
pub(crate) struct GpuPresentationHealth {
    state: AtomicU8,
    recovery_generation: AtomicU64,
    surface_error_count: AtomicU64,
}

impl Default for GpuPresentationHealth {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(GpuPresenterHealth::Healthy as u8),
            recovery_generation: AtomicU64::new(0),
            surface_error_count: AtomicU64::new(0),
        }
    }
}

impl GpuPresentationHealth {
    pub(crate) fn state(&self) -> GpuPresenterHealth {
        match self.state.load(Ordering::Acquire) {
            value if value == GpuPresenterHealth::Healthy as u8 => GpuPresenterHealth::Healthy,
            value if value == GpuPresenterHealth::SurfaceRecovering as u8 => {
                GpuPresenterHealth::SurfaceRecovering
            }
            _ => GpuPresenterHealth::BackendUnavailable,
        }
    }

    pub(crate) fn recovery_generation(&self) -> u64 {
        self.recovery_generation.load(Ordering::Acquire)
    }

    pub(crate) fn surface_error_count(&self) -> u64 {
        self.surface_error_count.load(Ordering::Relaxed)
    }

    fn mark_surface_recovering(&self) {
        self.surface_error_count.fetch_add(1, Ordering::Relaxed);
        self.recovery_generation.fetch_add(1, Ordering::AcqRel);
        self.state
            .store(GpuPresenterHealth::SurfaceRecovering as u8, Ordering::Release);
    }

    fn note_transient_surface_error(&self) {
        self.surface_error_count.fetch_add(1, Ordering::Relaxed);
    }

    fn mark_backend_unavailable(&self) {
        self.surface_error_count.fetch_add(1, Ordering::Relaxed);
        self.recovery_generation.fetch_add(1, Ordering::AcqRel);
        self.state
            .store(GpuPresenterHealth::BackendUnavailable as u8, Ordering::Release);
    }

    fn mark_presenter_unavailable(&self) {
        self.recovery_generation.fetch_add(1, Ordering::AcqRel);
        self.state
            .store(GpuPresenterHealth::BackendUnavailable as u8, Ordering::Release);
    }

    fn mark_healthy_after_fresh_paint(&self, upload_generation: u64) {
        if self.state() == GpuPresenterHealth::SurfaceRecovering
            && self.recovery_generation() == upload_generation
        {
            self.state
                .store(GpuPresenterHealth::Healthy as u8, Ordering::Release);
        }
    }
}

pub(crate) fn handle_surface_error(
    health: &GpuPresentationHealth,
    error: wgpu::SurfaceError,
) -> egui_wgpu::SurfaceErrorAction {
    match error {
        wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost => {
            health.mark_surface_recovering();
            egui_wgpu::SurfaceErrorAction::RecreateSurface
        }
        wgpu::SurfaceError::Timeout => {
            health.note_transient_surface_error();
            egui_wgpu::SurfaceErrorAction::SkipFrame
        }
        wgpu::SurfaceError::OutOfMemory => {
            health.mark_backend_unavailable();
            egui_wgpu::SurfaceErrorAction::SkipFrame
        }
        _ => {
            health.mark_backend_unavailable();
            egui_wgpu::SurfaceErrorAction::SkipFrame
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GpuSubmitOutcome {
    Accepted,
    StaleGeneration,
}

#[derive(Debug)]
pub(crate) struct GpuSubmitFailure {
    pub(crate) message: String,
    pub(crate) gray8: Vec<u8>,
}


#[derive(Debug)]
struct PendingUpload {
    generation: u64,
    recovery_generation: u64,
    width: u32,
    height: u32,
    gray8: Vec<u8>,
}

#[derive(Debug)]
struct PresenterShared {
    current_generation: u64,
    pending: Option<PendingUpload>,
    replaced_pending_frames: u64,
    health: GpuPresenterHealth,
}

impl Default for PresenterShared {
    fn default() -> Self {
        Self {
            current_generation: 0,
            pending: None,
            replaced_pending_frames: 0,
            health: GpuPresenterHealth::Healthy,
        }
    }
}

impl PresenterShared {
    fn set_generation(&mut self, generation: u64) {
        self.current_generation = generation;
        self.pending = None;
    }

    fn mark_backend_unavailable(&mut self) {
        self.health = GpuPresenterHealth::BackendUnavailable;
        self.pending = None;
    }

    fn submit(
        &mut self,
        generation: u64,
        recovery_generation: u64,
        width: u32,
        height: u32,
        gray8: Vec<u8>,
    ) -> Result<GpuSubmitOutcome, GpuSubmitFailure> {
        if let Err(message) = validate_upload(width, height, gray8.len()) {
            return Err(GpuSubmitFailure { message, gray8 });
        }
        if generation != self.current_generation {
            return Ok(GpuSubmitOutcome::StaleGeneration);
        }

        if self.pending.replace(PendingUpload {
            generation,
            recovery_generation,
            width,
            height,
            gray8,
        }).is_some()
        {
            self.replaced_pending_frames = self.replaced_pending_frames.saturating_add(1);
        }
        Ok(GpuSubmitOutcome::Accepted)
    }

    fn take_pending(&mut self) -> Option<PendingUpload> {
        let upload = self.pending.take()?;
        if upload.generation == self.current_generation {
            Some(upload)
        } else {
            None
        }
    }
}

pub(crate) struct GpuVideoPresenter {
    shared: Arc<Mutex<PresenterShared>>,
    health: Arc<GpuPresentationHealth>,
}

impl GpuVideoPresenter {
    pub(crate) fn from_creation_context(
        cc: &eframe::CreationContext<'_>,
        health: Arc<GpuPresentationHealth>,
    ) -> Option<Self> {
        let Some(render_state) = cc.wgpu_render_state.as_ref() else {
            health.mark_presenter_unavailable();
            return None;
        };
        let resources =
            GpuVideoResources::new(&render_state.device, render_state.target_format);

        render_state
            .renderer
            .write()
            .callback_resources
            .insert(resources);

        Some(Self {
            shared: Arc::new(Mutex::new(PresenterShared::default())),
            health,
        })
    }

    pub(crate) fn health(&self) -> Result<GpuPresenterHealth, String> {
        let shared_health = self
            .shared
            .lock()
            .map(|shared| shared.health)
            .map_err(|_| "GPU presenter state lock poisoned".to_string())?;
        if shared_health == GpuPresenterHealth::BackendUnavailable {
            Ok(shared_health)
        } else {
            Ok(self.health.state())
        }
    }

    pub(crate) fn surface_error_count(&self) -> u64 {
        self.health.surface_error_count()
    }

    pub(crate) fn set_generation(&self, generation: u64) -> Result<(), String> {
        self.shared
            .lock()
            .map_err(|_| "GPU presenter state lock poisoned".to_string())?
            .set_generation(generation);
        Ok(())
    }

    pub(crate) fn submit_frame(
        &self,
        generation: u64,
        width: u32,
        height: u32,
        gray8: Vec<u8>,
    ) -> Result<GpuSubmitOutcome, GpuSubmitFailure> {
        let mut shared = match self.shared.lock() {
            Ok(shared) => shared,
            Err(_) => {
                return Err(GpuSubmitFailure {
                    message: "GPU presenter state lock poisoned".to_string(),
                    gray8,
                });
            }
        };
        shared.submit(
            generation,
            self.health.recovery_generation(),
            width,
            height,
            gray8,
        )
    }

    pub(crate) fn paint(&self, ui: &mut egui::Ui, size: egui::Vec2) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
        ui.painter()
            .add(egui_wgpu::Callback::new_paint_callback(
                rect,
                VideoPaintCallback {
                    shared: Arc::clone(&self.shared),
                    health: Arc::clone(&self.health),
                },
            ));
        response
    }
}

fn validate_upload(width: u32, height: u32, len: usize) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("GPU upload dimensions must be non-zero".to_string());
    }
    if width > MAX_VIDEO_SIDE || height > MAX_VIDEO_SIDE {
        return Err(format!(
            "GPU upload dimensions {width}x{height} exceed side cap {MAX_VIDEO_SIDE}"
        ));
    }

    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| "GPU upload pixel-count overflow".to_string())?;
    if pixels > MAX_VIDEO_PIXELS {
        return Err(format!(
            "GPU upload pixel count {pixels} exceeds cap {MAX_VIDEO_PIXELS}"
        ));
    }
    let expected = usize::try_from(pixels)
        .map_err(|_| "GPU upload pixel count does not fit usize".to_string())?;
    if len != expected {
        return Err(format!(
            "GPU upload byte length {len} does not match {width}x{height} ({expected} bytes)"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AllocationDecision {
    Reuse,
    Reallocate,
}

fn allocation_decision(existing: Option<(u32, u32)>, incoming: (u32, u32)) -> AllocationDecision {
    if existing == Some(incoming) {
        AllocationDecision::Reuse
    } else {
        AllocationDecision::Reallocate
    }
}

struct VideoPaintCallback {
    shared: Arc<Mutex<PresenterShared>>,
    health: Arc<GpuPresentationHealth>,
}

impl egui_wgpu::CallbackTrait for VideoPaintCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let upload = self
            .shared
            .lock()
            .ok()
            .and_then(|mut shared| shared.take_pending());
        let Some(upload) = upload else {
            return Vec::new();
        };
        let Some(resources) = callback_resources.get_mut::<GpuVideoResources>() else {
            self.health.mark_presenter_unavailable();
            if let Ok(mut shared) = self.shared.lock() {
                shared.mark_backend_unavailable();
            }
            return Vec::new();
        };

        resources.upload(device, queue, upload);
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(resources) = callback_resources.get::<GpuVideoResources>() else {
            self.health.mark_presenter_unavailable();
            if let Ok(mut shared) = self.shared.lock() {
                shared.mark_backend_unavailable();
            }
            return;
        };
        if let Some(upload_generation) = resources.paint(render_pass) {
            self.health
                .mark_healthy_after_fresh_paint(upload_generation);
        }
    }
}

struct GpuVideoResources {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    texture: Option<wgpu::Texture>,
    bind_group: Option<wgpu::BindGroup>,
    dimensions: Option<(u32, u32)>,
    last_upload_recovery_generation: Option<u64>,
}

impl GpuVideoResources {
    fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("srs-player-video-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("video_presenter.wgsl").into()),
        });
        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("srs-player-video-bind-group-layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("srs-player-video-pipeline-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("srs-player-video-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("srs-player-video-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        Self {
            pipeline,
            bind_group_layout,
            sampler,
            texture: None,
            bind_group: None,
            dimensions: None,
            last_upload_recovery_generation: None,
        }
    }

    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, upload: PendingUpload) {
        let incoming = (upload.width, upload.height);
        self.last_upload_recovery_generation = Some(upload.recovery_generation);
        if allocation_decision(self.dimensions, incoming) == AllocationDecision::Reallocate {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("srs-player-video-r8"),
                size: wgpu::Extent3d {
                    width: upload.width,
                    height: upload.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("srs-player-video-bind-group"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.texture = Some(texture);
            self.bind_group = Some(bind_group);
            self.dimensions = Some(incoming);
        }

        let Some(texture) = self.texture.as_ref() else {
            return;
        };
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &upload.gray8,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(upload.width),
                rows_per_image: Some(upload.height),
            },
            wgpu::Extent3d {
                width: upload.width,
                height: upload.height,
                depth_or_array_layers: 1,
            },
        );
    }

    fn paint(&self, render_pass: &mut wgpu::RenderPass<'_>) -> Option<u64> {
        let bind_group = self.bind_group.as_ref()?;
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.draw(0..4, 0..1);
        self.last_upload_recovery_generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_surface_enters_recovery_and_requests_reconfigure() {
        let health = GpuPresentationHealth::default();
        let action = handle_surface_error(&health, wgpu::SurfaceError::Lost);

        assert!(matches!(
            action,
            egui_wgpu::SurfaceErrorAction::RecreateSurface
        ));
        assert_eq!(health.state(), GpuPresenterHealth::SurfaceRecovering);
        assert_eq!(health.surface_error_count(), 1);
        assert_eq!(health.recovery_generation(), 1);
    }

    #[test]
    fn timeout_skips_frame_without_disabling_presenter() {
        let health = GpuPresentationHealth::default();
        let action = handle_surface_error(&health, wgpu::SurfaceError::Timeout);

        assert!(matches!(action, egui_wgpu::SurfaceErrorAction::SkipFrame));
        assert_eq!(health.state(), GpuPresenterHealth::Healthy);
        assert_eq!(health.surface_error_count(), 1);
        assert_eq!(health.recovery_generation(), 0);
    }

    #[test]
    fn out_of_memory_marks_gpu_presenter_unavailable() {
        let health = GpuPresentationHealth::default();
        let action = handle_surface_error(&health, wgpu::SurfaceError::OutOfMemory);

        assert!(matches!(action, egui_wgpu::SurfaceErrorAction::SkipFrame));
        assert_eq!(health.state(), GpuPresenterHealth::BackendUnavailable);
        assert_eq!(health.surface_error_count(), 1);
    }

    #[test]
    fn surface_recovery_requires_fresh_matching_generation_paint() {
        let health = GpuPresentationHealth::default();
        health.mark_surface_recovering();
        let generation = health.recovery_generation();

        health.mark_healthy_after_fresh_paint(generation.saturating_sub(1));
        assert_eq!(health.state(), GpuPresenterHealth::SurfaceRecovering);

        health.mark_healthy_after_fresh_paint(generation);
        assert_eq!(health.state(), GpuPresenterHealth::Healthy);
    }

    #[test]
    fn backend_unavailable_health_discards_pending_upload() {
        let mut shared = PresenterShared::default();
        shared.set_generation(5);
        shared.submit(5, 0, 2, 2, vec![1; 4]).expect("submit");
        assert!(shared.pending.is_some());

        shared.mark_backend_unavailable();

        assert_eq!(shared.health, GpuPresenterHealth::BackendUnavailable);
        assert!(shared.pending.is_none());
    }

    #[test]
    fn same_dimensions_reuse_gpu_texture_allocation() {
        assert_eq!(
            allocation_decision(Some((1920, 1080)), (1920, 1080)),
            AllocationDecision::Reuse
        );
    }

    #[test]
    fn resolution_change_reallocates_gpu_texture_once_needed() {
        assert_eq!(
            allocation_decision(Some((1920, 1080)), (1280, 720)),
            AllocationDecision::Reallocate
        );
        assert_eq!(
            allocation_decision(None, (1280, 720)),
            AllocationDecision::Reallocate
        );
    }

    #[test]
    fn stale_generation_never_enters_gpu_upload_slot() {
        let mut shared = PresenterShared::default();
        shared.set_generation(8);
        assert_eq!(
            shared
                .submit(7, 0, 2, 2, vec![0; 4])
                .expect("validated stale submit"),
            GpuSubmitOutcome::StaleGeneration
        );
        assert!(shared.pending.is_none());
    }

    #[test]
    fn newer_frame_replaces_pending_upload_without_growing_queue() {
        let mut shared = PresenterShared::default();
        shared.set_generation(4);
        assert_eq!(
            shared.submit(4, 0, 2, 2, vec![1; 4]).expect("first"),
            GpuSubmitOutcome::Accepted
        );
        assert_eq!(
            shared.submit(4, 0, 2, 2, vec![2; 4]).expect("second"),
            GpuSubmitOutcome::Accepted
        );
        assert_eq!(shared.replaced_pending_frames, 1);
        assert_eq!(shared.pending.as_ref().expect("pending").gray8, vec![2; 4]);
    }

    #[test]
    fn hostile_gpu_upload_dimensions_are_rejected_before_allocation() {
        assert!(validate_upload(0, 1, 0).is_err());
        assert!(validate_upload(MAX_VIDEO_SIDE + 1, 1, 0).is_err());
        assert!(validate_upload(2, 2, 3).is_err());
        assert!(validate_upload(2, 2, 4).is_ok());
    }
}
