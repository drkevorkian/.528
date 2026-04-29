//! Structured errors for SRSV2 — never panic on hostile bitstreams.

#[derive(Debug, thiserror::Error)]
pub enum SrsV2Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid magic or container tag")]
    BadMagic,
    #[error("unsupported SRSV2 bitstream version {0}")]
    UnsupportedVersion(u8),
    #[error("syntax error: {0}")]
    Syntax(&'static str),
    #[error("dimensions invalid: {width}x{height}")]
    Dimensions { width: u32, height: u32 },
    #[error("allocation limit exceeded ({context})")]
    AllocationLimit { context: &'static str },
    #[error("integer overflow in {0}")]
    Overflow(&'static str),
    #[error("truncated bitstream")]
    Truncated,
    #[error("reserved / unsupported feature: {0}")]
    Unsupported(&'static str),
    #[error("profile limits exceeded: {0}")]
    LimitExceeded(&'static str),
    #[error("decode mismatch (internal)")]
    Internal,
}

impl SrsV2Error {
    pub const fn syntax(msg: &'static str) -> Self {
        Self::Syntax(msg)
    }
}
