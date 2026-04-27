use std::path::{Path, PathBuf};

use anyhow::Result;
use libsrs_compat::{CompatLayer, ProbeResult};
use libsrs_contract::Packet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineMode {
    Analyze,
    Import,
    Transcode,
}

#[derive(Debug, Clone)]
pub struct PipelineRequest {
    pub input: PathBuf,
    pub output: Option<PathBuf>,
    pub mode: PipelineMode,
}

pub trait NativeTranscoder: Send {
    fn transcode_packet(&mut self, packet: Packet) -> Result<()>;
    fn finalize(&mut self) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct NoopNativeTranscoder {
    packet_count: usize,
}

impl NoopNativeTranscoder {
    pub fn packet_count(&self) -> usize {
        self.packet_count
    }
}

impl NativeTranscoder for NoopNativeTranscoder {
    fn transcode_packet(&mut self, _packet: Packet) -> Result<()> {
        self.packet_count += 1;
        Ok(())
    }

    fn finalize(&mut self) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TranscodePipeline {
    compat: CompatLayer,
}

impl Default for TranscodePipeline {
    fn default() -> Self {
        Self {
            compat: CompatLayer::default(),
        }
    }
}

impl TranscodePipeline {
    pub const fn new(compat: CompatLayer) -> Self {
        Self { compat }
    }

    pub fn analyze_source<P: AsRef<Path>>(&self, input: P) -> Result<ProbeResult> {
        let prober = self.compat.create_prober();
        prober.probe_path(input.as_ref())
    }

    pub fn import_to_native<T: NativeTranscoder, P: AsRef<Path>>(
        &self,
        input: P,
        target: &mut T,
    ) -> Result<usize> {
        let mut ingestor = self.compat.create_ingestor();
        ingestor.open_path(input.as_ref())?;

        let mut total = 0usize;
        while let Some(source_packet) = ingestor.read_packet()? {
            target.transcode_packet(source_packet.packet)?;
            total += 1;
        }

        ingestor.close()?;
        target.finalize()?;
        Ok(total)
    }

    pub fn execute<T: NativeTranscoder>(&self, req: PipelineRequest, target: &mut T) -> Result<()> {
        match req.mode {
            PipelineMode::Analyze => {
                let _ = self.analyze_source(&req.input)?;
                Ok(())
            }
            PipelineMode::Import | PipelineMode::Transcode => {
                let _ = self.import_to_native(&req.input, target)?;
                Ok(())
            }
        }
    }
}
