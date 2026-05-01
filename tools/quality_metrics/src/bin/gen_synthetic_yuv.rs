//! CLI: deterministic YUV420p8 clips + JSON metadata.

use std::path::PathBuf;

use clap::Parser;
use quality_metrics::synthetic::{
    write_yuv420p8_clip, SyntheticClipSpec, SyntheticError, SyntheticPattern,
};

#[derive(Parser, Debug)]
#[command(name = "gen_synthetic_yuv")]
struct Args {
    #[arg(long)]
    pattern: String,
    #[arg(long)]
    width: u32,
    #[arg(long)]
    height: u32,
    #[arg(long)]
    frames: u32,
    /// Frames per second (integer). Written as `fps_num=fps`, `fps_den=1`.
    #[arg(long)]
    fps: u32,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    #[arg(long)]
    out: PathBuf,
    #[arg(long)]
    meta: PathBuf,
    #[arg(long, default_value_t = false)]
    allow_large: bool,
}

fn main() -> Result<(), SyntheticError> {
    let a = Args::parse();
    let pattern = SyntheticPattern::parse_cli(&a.pattern)
        .ok_or_else(|| SyntheticError::UnknownPattern(a.pattern.clone()))?;

    let spec = SyntheticClipSpec {
        width: a.width,
        height: a.height,
        fps_num: a.fps.max(1),
        fps_den: 1,
        frames: a.frames.max(1),
        pattern,
        seed: a.seed,
        allow_large: a.allow_large,
    };

    let meta = write_yuv420p8_clip(&spec, &a.out, &a.meta)?;
    println!(
        "pattern={} {}x{} frames={} bytes={} meta={}",
        a.pattern,
        meta.width,
        meta.height,
        meta.frames,
        meta.yuv_bytes,
        a.meta.display()
    );
    Ok(())
}
