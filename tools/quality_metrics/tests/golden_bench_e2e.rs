//! End-to-end check: `bench_srsv2` on the committed golden YUV under `samples/bench/`.
//!
//! Does not invoke ffmpeg (`compare-x264` is off by default).

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct GoldenReport {
    raw_bytes: u64,
    srsv2: Srsv2Detail,
    table: Vec<CodecRow>,
}

#[derive(Debug, Deserialize)]
struct Srsv2Detail {
    keyframes: u32,
    pframes: u32,
}

#[derive(Debug, Deserialize)]
struct CodecRow {
    codec: String,
    bytes: u64,
    psnr_y: f64,
    ssim_y: f64,
}

#[test]
fn bench_srsv2_golden_clip_invariants() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let golden = manifest_dir.join("../../samples/bench/golden_64x64_10.yuv");
    let golden = fs::canonicalize(&golden).unwrap_or_else(|e| {
        panic!("canonicalize golden clip {:?}: {e}", golden);
    });
    assert!(
        golden.is_file(),
        "missing golden clip at {} (generate with gen_synthetic_yuv)",
        golden.display()
    );

    let out_dir = std::env::temp_dir().join("qm-golden-bench");
    fs::create_dir_all(&out_dir).unwrap();
    let report_json = out_dir.join("report.json");
    let report_md = out_dir.join("report.md");

    let bin = env!("CARGO_BIN_EXE_bench_srsv2");
    let status = Command::new(bin)
        .stdout(Stdio::null())
        .args([
            "--input",
            golden.to_str().expect("utf-8 path"),
            "--width",
            "64",
            "--height",
            "64",
            "--frames",
            "10",
            "--fps",
            "24",
            "--qp",
            "28",
            "--keyint",
            "30",
            "--motion-radius",
            "16",
            "--report-json",
            report_json.to_str().expect("utf-8 path"),
            "--report-md",
            report_md.to_str().expect("utf-8 path"),
        ])
        .status()
        .unwrap_or_else(|e| panic!("spawn {bin}: {e}"));

    assert!(status.success(), "bench_srsv2 exited {:?}", status.code());

    let json = fs::read_to_string(&report_json).unwrap();
    let r: GoldenReport = serde_json::from_str(&json).unwrap();

    assert_eq!(r.raw_bytes, 61440, "golden clip byte length mismatch");
    assert!(
        r.srsv2.keyframes >= 1,
        "expected at least one intra frame, got {}",
        r.srsv2.keyframes
    );
    assert!(
        r.srsv2.pframes > 0,
        "expected P-frames with 64×64 (16-aligned), motion search, and keyint > frame count"
    );

    let srsv2 = r
        .table
        .iter()
        .find(|t| t.codec == "SRSV2")
        .expect("SRSV2 codec row");
    assert!(
        srsv2.bytes > 0,
        "encoded SRSV2 payload sum must be non-zero"
    );
    assert!(
        srsv2.psnr_y.is_finite(),
        "PSNR-Y must be finite, got {}",
        srsv2.psnr_y
    );
    assert!(
        srsv2.ssim_y.is_finite(),
        "SSIM-Y must be finite, got {}",
        srsv2.ssim_y
    );
    assert!(
        (0.0..=1.0).contains(&srsv2.ssim_y),
        "SSIM-Y must be in [0,1], got {}",
        srsv2.ssim_y
    );

    assert!(report_md.is_file());
}
