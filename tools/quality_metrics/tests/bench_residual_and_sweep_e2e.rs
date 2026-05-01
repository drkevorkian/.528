//! Residual comparison and sweep paths for `bench_srsv2` (no FFmpeg).

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use quality_metrics::synthetic::{write_yuv420p8_clip, SyntheticClipSpec, SyntheticPattern};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct CompareEntry {
    label: String,
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

fn golden_yuv_path() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fs::canonicalize(manifest_dir.join("../../samples/bench/golden_64x64_10.yuv"))
        .expect("canonicalize golden YUV")
}

#[test]
fn compare_residual_modes_on_golden_without_ffmpeg() {
    let golden = golden_yuv_path();
    let out_dir = std::env::temp_dir().join("qm-compare-residual");
    fs::create_dir_all(&out_dir).unwrap();
    let report_json = out_dir.join("cmp.json");
    let report_md = out_dir.join("cmp.md");

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
            "--compare-residual-modes",
            "--report-json",
            report_json.to_str().expect("utf-8 path"),
            "--report-md",
            report_md.to_str().expect("utf-8 path"),
        ])
        .status()
        .expect("spawn bench_srsv2");

    assert!(
        status.success(),
        "bench_srsv2 compare-residual {:?}",
        status.code()
    );

    let json = fs::read_to_string(&report_json).unwrap();
    let v: Value = serde_json::from_str(&json).unwrap();
    let arr = v["compare_residual_modes"]
        .as_array()
        .expect("compare_residual_modes array");
    assert_eq!(arr.len(), 3);
    let parsed: Vec<CompareEntry> =
        serde_json::from_value(v["compare_residual_modes"].clone()).unwrap();
    assert_eq!(parsed[0].label, "SRSV2-explicit");
    assert_eq!(parsed[1].label, "SRSV2-auto");
    assert_eq!(parsed[2].label, "SRSV2-rans");
    assert!(parsed[0].ok, "explicit");
    assert!(parsed[1].ok, "auto");
    assert!(parsed[2].ok, "forced rans on golden should encode");
    assert!(report_md.is_file());
}

#[test]
fn compare_residual_modes_rans_row_can_fail_without_aborting() {
    let out_dir = std::env::temp_dir().join("qm-rans-fail");
    fs::create_dir_all(&out_dir).unwrap();
    let yuv = out_dir.join("noise.yuv");
    let meta = out_dir.join("noise.json");
    let spec = SyntheticClipSpec {
        width: 64,
        height: 64,
        fps_num: 30,
        fps_den: 1,
        frames: 5,
        pattern: SyntheticPattern::Noise,
        seed: 99,
        allow_large: false,
    };
    write_yuv420p8_clip(&spec, &yuv, &meta).expect("write synthetic noise");

    let report_json = out_dir.join("cmp.json");
    let report_md = out_dir.join("cmp.md");
    let bin = env!("CARGO_BIN_EXE_bench_srsv2");
    let status = Command::new(bin)
        .stdout(Stdio::null())
        .args([
            "--input",
            yuv.to_str().expect("utf-8 path"),
            "--width",
            "64",
            "--height",
            "64",
            "--frames",
            "5",
            "--fps",
            "30",
            "--qp",
            "1",
            "--min-qp",
            "1",
            "--max-qp",
            "51",
            "--keyint",
            "30",
            "--motion-radius",
            "0",
            "--compare-residual-modes",
            "--report-json",
            report_json.to_str().expect("utf-8 path"),
            "--report-md",
            report_md.to_str().expect("utf-8 path"),
        ])
        .status()
        .expect("spawn bench_srsv2");

    assert!(status.success());
    let json = fs::read_to_string(&report_json).unwrap();
    let v: Value = serde_json::from_str(&json).unwrap();
    let arr = v["compare_residual_modes"].as_array().unwrap();
    assert_eq!(arr.len(), 3);
    let parsed: Vec<CompareEntry> =
        serde_json::from_value(v["compare_residual_modes"].clone()).unwrap();
    assert!(parsed[0].ok);
    assert!(parsed[1].ok);
    assert!(!parsed[2].ok);
    let err = parsed[2].error.as_ref().expect("rans error text");
    assert!(
        err.contains("rANS") || err.contains("rans"),
        "unexpected error: {err}"
    );
}

#[test]
fn sweep_emits_expected_grid_size() {
    let golden = golden_yuv_path();
    let out_dir = std::env::temp_dir().join("qm-sweep");
    fs::create_dir_all(&out_dir).unwrap();
    let report_json = out_dir.join("sweep.json");
    let report_md = out_dir.join("sweep.md");

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
            "--sweep",
            "--report-json",
            report_json.to_str().expect("utf-8 path"),
            "--report-md",
            report_md.to_str().expect("utf-8 path"),
        ])
        .status()
        .expect("spawn bench_srsv2 sweep");

    assert!(status.success());
    let json = fs::read_to_string(&report_json).unwrap();
    let v: Value = serde_json::from_str(&json).unwrap();
    let sweep = v["sweep"].as_array().expect("sweep array");
    assert_eq!(sweep.len(), 24, "4 QPs × 2 residual modes × 3 motion radii");
    assert!(report_md.is_file());
}
