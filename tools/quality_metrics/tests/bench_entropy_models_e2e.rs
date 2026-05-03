//! `--compare-entropy-models` and `--entropy-model` on golden YUV (no FFmpeg).

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[test]
fn compare_entropy_models_two_ok_rows_on_golden_without_ffmpeg() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let golden = manifest_dir.join("../../samples/bench/golden_64x64_10.yuv");
    let golden = fs::canonicalize(&golden).unwrap_or_else(|e| {
        panic!("canonicalize golden clip {:?}: {e}", golden);
    });
    assert!(golden.is_file(), "missing golden clip at {}", golden.display());

    let out_dir = std::env::temp_dir().join("qm-entropy-bench");
    fs::create_dir_all(&out_dir).unwrap();
    let report_json = out_dir.join("entropy_report.json");
    let report_md = out_dir.join("entropy_report.md");

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
            "--inter-syntax",
            "entropy",
            "--residual-entropy",
            "explicit",
            "--compare-entropy-models",
            "--report-json",
            report_json.to_str().expect("utf-8 path"),
            "--report-md",
            report_md.to_str().expect("utf-8 path"),
        ])
        .status()
        .unwrap_or_else(|e| panic!("spawn {bin}: {e}"));

    assert!(status.success(), "bench_srsv2 exited {:?}", status.code());

    let json = fs::read_to_string(&report_json).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let rows = v["compare_entropy_models"]
        .as_array()
        .expect("compare_entropy_models array");
    assert_eq!(rows.len(), 2, "expected StaticV1 + ContextV1 rows");
    assert!(rows[0]["ok"].as_bool().unwrap(), "row0: {:?}", rows[0]);
    assert!(rows[1]["ok"].as_bool().unwrap(), "row1: {:?}", rows[1]);
    assert_eq!(rows[0]["entropy_model_mode"], "static");
    assert_eq!(rows[1]["entropy_model_mode"], "context");
    assert!(
        rows[0]["fr2_revision_counts"]["rev17"].as_u64().unwrap_or(0) > 0,
        "expected StaticV1 P frames as FR2 rev17 in row0: {:?}",
        rows[0]["fr2_revision_counts"]
    );
    assert!(
        rows[1]["fr2_revision_counts"]["rev23"].as_u64().unwrap_or(0) > 0,
        "expected ContextV1 P frames as FR2 rev23 in row1: {:?}",
        rows[1]["fr2_revision_counts"]
    );

    let md = fs::read_to_string(&report_md).unwrap();
    assert!(md.contains("MV entropy model comparison"));
    assert!(md.contains("| static |"));
    assert!(md.contains("| context |"));
}

#[test]
fn entropy_model_context_without_entropy_inter_syntax_errors() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let golden = manifest_dir.join("../../samples/bench/golden_64x64_10.yuv");
    let golden = fs::canonicalize(&golden).unwrap_or_else(|e| {
        panic!("canonicalize golden clip {:?}: {e}", golden);
    });

    let out_dir = std::env::temp_dir().join("qm-entropy-bench-bad");
    fs::create_dir_all(&out_dir).unwrap();
    let report_json = out_dir.join("bad.json");
    let report_md = out_dir.join("bad.md");

    let bin = env!("CARGO_BIN_EXE_bench_srsv2");
    let output = Command::new(bin)
        .args([
            "--input",
            golden.to_str().expect("utf-8 path"),
            "--width",
            "64",
            "--height",
            "64",
            "--frames",
            "4",
            "--fps",
            "24",
            "--qp",
            "28",
            "--keyint",
            "30",
            "--motion-radius",
            "16",
            "--inter-syntax",
            "compact",
            "--entropy-model",
            "context",
            "--report-json",
            report_json.to_str().expect("utf-8 path"),
            "--report-md",
            report_md.to_str().expect("utf-8 path"),
        ])
        .output()
        .unwrap_or_else(|e| panic!("spawn {bin}: {e}"));

    assert!(!output.status.success());
    let msg = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(
        msg.contains("entropy-model context") && msg.contains("inter-syntax entropy"),
        "unexpected output: {msg}"
    );
}
