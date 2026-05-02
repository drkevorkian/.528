//! Validation of `bench_srsv2` motion/skip/AQ reporting (no FFmpeg).

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;

fn golden_yuv_path() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fs::canonicalize(manifest_dir.join("../../samples/bench/golden_64x64_10.yuv"))
        .expect("canonicalize golden YUV")
}

fn run_bench_json(extra: &[&str]) -> Value {
    let golden = golden_yuv_path();
    let out_dir = std::env::temp_dir().join(format!(
        "qm-bench-val-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&out_dir).unwrap();
    let report_json = out_dir.join("rep.json");
    let report_md = out_dir.join("rep.md");

    let mut args: Vec<String> = vec![
        "--input".into(),
        golden.to_string_lossy().into_owned(),
        "--width".into(),
        "64".into(),
        "--height".into(),
        "64".into(),
        "--frames".into(),
        "10".into(),
        "--fps".into(),
        "24".into(),
        "--qp".into(),
        "28".into(),
        "--keyint".into(),
        "30".into(),
        "--motion-radius".into(),
        "8".into(),
        "--residual-entropy".into(),
        "auto".into(),
        "--report-json".into(),
        report_json.to_string_lossy().into_owned(),
        "--report-md".into(),
        report_md.to_string_lossy().into_owned(),
    ];
    for s in extra {
        args.push((*s).to_string());
    }

    let bin = env!("CARGO_BIN_EXE_bench_srsv2");
    let status = Command::new(bin)
        .stdout(Stdio::null())
        .args(&args)
        .status()
        .expect("spawn bench_srsv2");
    assert!(status.success(), "bench {:?}", status.code());
    let json = fs::read_to_string(&report_json).unwrap();
    serde_json::from_str(&json).unwrap()
}

#[test]
fn bench_skip_disabled_reports_zero_skip_subblocks() {
    let v = run_bench_json(&["--enable-skip-blocks", "false"]);
    let motion = &v["srsv2"]["motion"];
    assert_eq!(motion["skip_subblocks_total"], 0);
}

#[test]
fn bench_motion_none_reports_zero_nonzero_mv_macroblocks() {
    let v = run_bench_json(&["--motion-search", "none"]);
    let motion = &v["srsv2"]["motion"];
    assert_eq!(motion["nonzero_motion_macroblocks_total"], 0);
}

#[test]
fn bench_diamond_fewer_sad_evals_than_exhaustive_on_golden() {
    let v_dia = run_bench_json(&["--motion-search", "diamond"]);
    let v_exh = run_bench_json(&["--motion-search", "exhaustive-small"]);
    let sad_dia = v_dia["srsv2"]["motion"]["sad_evaluations_total"]
        .as_u64()
        .unwrap();
    let sad_exh = v_exh["srsv2"]["motion"]["sad_evaluations_total"]
        .as_u64()
        .unwrap();
    assert!(
        sad_dia < sad_exh,
        "expected diamond cheaper than exhaustive: {sad_dia} vs {sad_exh}"
    );
}

#[test]
fn bench_aq_activity_populates_report() {
    let v = run_bench_json(&["--aq", "activity", "--aq-strength", "6"]);
    let aq = &v["srsv2"]["aq"];
    assert_eq!(aq["mode"], "activity");
    assert_eq!(aq["aq_enabled"], true);
}

#[test]
fn sweep_extended_adds_two_rows() {
    let golden = golden_yuv_path();
    let out_dir = std::env::temp_dir().join("qm-sweep-ext");
    fs::create_dir_all(&out_dir).unwrap();
    let report_json = out_dir.join("sweep_ext.json");
    let report_md = out_dir.join("sweep_ext.md");
    let bin = env!("CARGO_BIN_EXE_bench_srsv2");
    let status = Command::new(bin)
        .stdout(Stdio::null())
        .args([
            "--input",
            golden.to_str().expect("utf8"),
            "--width",
            "64",
            "--height",
            "64",
            "--frames",
            "10",
            "--fps",
            "24",
            "--keyint",
            "30",
            "--motion-radius",
            "16",
            "--sweep",
            "--sweep-extended",
            "--report-json",
            report_json.to_str().unwrap(),
            "--report-md",
            report_md.to_str().unwrap(),
        ])
        .status()
        .expect("spawn");
    assert!(status.success());
    let json = fs::read_to_string(&report_json).unwrap();
    let v: Value = serde_json::from_str(&json).unwrap();
    let sweep = v["sweep"].as_array().unwrap();
    assert_eq!(
        sweep.len(),
        26,
        "24 base grid rows + 2 extended AQ/motion variants"
    );
    let variants: Vec<_> = sweep
        .iter()
        .filter_map(|r| r["sweep_variant"].as_str())
        .collect();
    assert!(
        variants.contains(&"extended-aq-motion"),
        "expected extended variant rows"
    );
}

#[test]
fn bench_deblock_section_default_off() {
    let v = run_bench_json(&[]);
    let d = &v["srsv2"]["deblock"];
    assert_eq!(d["loop_filter_mode"], "off");
    assert_eq!(d["deblock_strength_effective"], 0);
    assert_eq!(d["deblock_strength_byte"], 0);
}

#[test]
fn bench_deblock_simple_reports_respin_objective_metrics() {
    let v = run_bench_json(&["--loop-filter", "simple", "--deblock-strength", "40"]);
    let d = &v["srsv2"]["deblock"];
    assert_eq!(d["loop_filter_mode"], "simple");
    assert_eq!(d["deblock_strength_byte"], 40);
    assert_eq!(d["deblock_strength_effective"], 40);
    assert!(d["psnr_y_filter_disabled_respin"].is_number());
    assert!(d["ssim_y_filter_disabled_respin"].is_number());
}
