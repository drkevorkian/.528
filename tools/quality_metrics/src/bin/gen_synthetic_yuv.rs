//! CLI: deterministic planar YUV420p8 clips + JSON metadata.

use std::path::PathBuf;

use clap::Parser;
use quality_metrics::synthetic::{GenerateOptions, SyntheticError, SyntheticPattern};

#[derive(Parser, Debug)]
#[command(name = "gen_synthetic_yuv")]
struct Args {
    #[arg(long, default_value = "noise")]
    pattern: String,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    #[arg(long)]
    frames: Option<u32>,
    #[arg(long, default_value_t = 30)]
    fps: u32,
    #[arg(long)]
    allow_large: bool,
    #[arg(long)]
    out_yuv: PathBuf,
    #[arg(long)]
    out_meta: PathBuf,
}

fn main() -> Result<(), SyntheticError> {
    let a = Args::parse();
    let pattern = SyntheticPattern::parse(&a.pattern)
        .ok_or_else(|| SyntheticError::UnknownPattern(a.pattern.clone()))?;
    let opts = GenerateOptions {
        pattern,
        seed: a.seed,
        frames: a.frames,
        fps: a.fps,
        allow_large: a.allow_large,
    };
    quality_metrics::synthetic::write_yuv_with_meta(&a.out_yuv, &a.out_meta, &opts)?;
    Ok(())
}
