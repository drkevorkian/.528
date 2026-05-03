//! CLI: deterministic YUV420p8 clips + JSON metadata.

use std::path::PathBuf;

use clap::Parser;
use quality_metrics::synthetic::{
    write_yuv420p8_clip, SyntheticClipSpec, SyntheticError, SyntheticPattern,
};

#[derive(Parser, Debug)]
#[command(name = "gen_synthetic_yuv")]
struct Args {
    /// When set to **`tiny`**, writes a fixed multi-clip corpus under **`--out-dir`** (ignores single-clip `--pattern` / dimensions).
    #[arg(long)]
    preset_corpus: Option<String>,
    /// Output directory for **`--preset-corpus`** mode.
    #[arg(long)]
    out_dir: Option<PathBuf>,
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

fn write_preset_corpus_tiny(
    dir: &std::path::Path,
    fps: u32,
    seed: u64,
) -> Result<(), SyntheticError> {
    /// `(file_tag, pattern_cli, width, height, frames)` — all dimensions even; suitable for SRSV2 bench (16-aligned heights/widths).
    const ROWS: &[(&str, &str, u32, u32, u32)] = &[
        ("flat", "flat", 64, 64, 16),
        ("gradient", "gradient", 64, 64, 16),
        ("moving_square", "moving-square", 128, 128, 24),
        ("scrolling_bars", "scrolling-bars", 128, 128, 24),
        ("checker", "checker", 64, 64, 16),
        ("noise", "noise", 64, 64, 16),
        ("scene_cut", "scene-cut", 128, 128, 24),
    ];
    let fps_num = fps.max(1);
    for &(tag, pat_s, w, h, frames) in ROWS {
        let pattern = SyntheticPattern::parse_cli(pat_s).expect("preset corpus pattern");
        let spec = SyntheticClipSpec {
            width: w,
            height: h,
            fps_num,
            fps_den: 1,
            frames: frames.max(1),
            pattern,
            seed,
            allow_large: false,
        };
        let out = dir.join(format!("tiny_{tag}_{w}x{h}.yuv"));
        let meta = dir.join(format!("tiny_{tag}_{w}x{h}.json"));
        let m = write_yuv420p8_clip(&spec, &out, &meta)?;
        println!(
            "preset=tiny pattern={pat_s} {}x{} frames={} bytes={} out={} meta={}",
            m.width,
            m.height,
            m.frames,
            m.yuv_bytes,
            out.display(),
            meta.display()
        );
    }
    Ok(())
}

fn main() -> Result<(), SyntheticError> {
    let a = Args::parse();

    if let Some(pc) = &a.preset_corpus {
        if pc != "tiny" {
            return Err(SyntheticError::UnknownPattern(format!(
                "unsupported --preset-corpus {pc} (only \"tiny\" is defined)"
            )));
        }
        let Some(dir) = &a.out_dir else {
            return Err(SyntheticError::PresetCorpusRequiresOutDir);
        };
        std::fs::create_dir_all(dir)?;
        return write_preset_corpus_tiny(dir, a.fps, a.seed);
    }

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
