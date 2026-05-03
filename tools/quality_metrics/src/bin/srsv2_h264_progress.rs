//! Block 6: generate `var/bench/corpus_tiny` (optional), run `bench_srsv2` compare modes, write
//! `var/bench/srsv2_h264_progress_summary.{md,json}` with engineering-only aggregates.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Parser;
use quality_metrics::synthetic::SyntheticClipMetadata;
use serde::Serialize;
use serde_json::Value;

#[derive(Parser, Debug)]
#[command(name = "srsv2_h264_progress")]
struct Args {
    /// Repository root (contains `Cargo.toml`, `target/`).
    #[arg(long, default_value = ".")]
    repo_root: PathBuf,

    /// Synthetic corpus directory (e.g. `var/bench/corpus_tiny`).
    #[arg(long, default_value = "var/bench/corpus_tiny")]
    corpus_dir: PathBuf,

    #[arg(long, default_value = "var/bench/srsv2_h264_progress_summary.md")]
    out_md: PathBuf,

    #[arg(long, default_value = "var/bench/srsv2_h264_progress_summary.json")]
    out_json: PathBuf,

    /// Skip `gen_synthetic_yuv` (corpus must already exist).
    #[arg(long, default_value_t = false)]
    skip_corpus: bool,

    /// Skip `cargo build` / assume `bench_srsv2` already built in `target/release` or `target/debug`.
    #[arg(long, default_value_t = false)]
    skip_build: bool,

    /// Run `--compare-x264` once on the first 128×128 corpus clip when `ffmpeg` is on PATH.
    #[arg(long, default_value_t = true)]
    try_x264: bool,
}

#[derive(Debug, Serialize)]
struct ClipBench {
    clip: String,
    width: u32,
    height: u32,
    frames: u32,
    entropy_json: PathBuf,
    partition_costs_json: PathBuf,
    sweep_json: PathBuf,
    b_modes_json: PathBuf,
}

#[derive(Debug, Serialize, Default)]
struct AggregateEntropy {
    clips_compared: u32,
    context_lower_total_bytes: u32,
    static_lower_total_bytes: u32,
    equal_bytes: u32,
    /// Sum of (context row.bytes - static row.bytes) when both ok; negative means context smaller.
    sum_delta_total_bytes: i128,
}

#[derive(Debug, Serialize, Default)]
struct AggregatePartition {
    clips: u32,
    sum_rejected_by_rdo: u64,
    sum_rejected_by_header_cost: u64,
    auto_fast_rdo_beats_fixed16x16_clips: u32,
    /// Clips where fixed16x16 total bytes < auto-fast-rdo total bytes.
    fixed_beats_auto_rdo_clips: u32,
}

#[derive(Debug, Serialize, Default)]
struct AggregateB {
    clips: u32,
    half_lower_bytes_than_int: u32,
    weighted_lower_bytes_than_int: u32,
    sum_weighted_mb: u64,
    sum_halfpel_selected: u64,
}

#[derive(Debug, Serialize, Default)]
struct BottleneckEvidence {
    /// Dominant bucket name across weighted clips (see markdown).
    dominant: String,
    avg_share_mv_header: f64,
    avg_share_residual: f64,
    avg_share_partition_map: f64,
    avg_share_partition_mv: f64,
    avg_share_partition_residual: f64,
}

#[derive(Debug, Serialize)]
struct ProgressSummary {
    note: &'static str,
    repo_root: String,
    corpus_dir: String,
    ffmpeg_on_path: bool,
    ran_x264: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    x264_sample_json: Option<PathBuf>,
    clips: Vec<ClipBench>,
    entropy: AggregateEntropy,
    partition: AggregatePartition,
    b_modes: AggregateB,
    sweep_rows_total: u64,
    sweep_pareto_smallest_bytes_threshold_hits: u32,
    x264_vs_srsv2_note: String,
    bottleneck: BottleneckEvidence,
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn bench_exe(repo: &Path, release: bool) -> PathBuf {
    let profile = if release { "release" } else { "debug" };
    let mut p = repo.join("target").join(profile).join("bench_srsv2");
    if cfg!(windows) {
        p.set_extension("exe");
    }
    p
}

fn cargo_build_bench(repo: &Path, release: bool) -> Result<()> {
    let mut c = Command::new("cargo");
    c.current_dir(repo);
    let mut args = vec!["build", "-p", "quality_metrics", "--bin", "bench_srsv2"];
    if release {
        args.push("--release");
    }
    c.args(&args);
    let st = c.status().context("spawn cargo build")?;
    if !st.success() {
        bail!("cargo build bench_srsv2 failed with {st}");
    }
    Ok(())
}

fn run_gen_corpus(repo: &Path, out_dir: &Path) -> Result<()> {
    let mut c = Command::new("cargo");
    c.current_dir(repo).args([
        "run",
        "-p",
        "quality_metrics",
        "--bin",
        "gen_synthetic_yuv",
        "--",
        "--preset-corpus",
        "tiny",
        "--out-dir",
    ]);
    c.arg(out_dir);
    c.args([
        "--seed",
        "528",
        "--pattern",
        "flat",
        "--width",
        "2",
        "--height",
        "2",
        "--frames",
        "1",
        "--fps",
        "30",
        "--out",
        "var/bench/__gen_dummy.yuv",
        "--meta",
        "var/bench/__gen_dummy.json",
    ]);
    let st = c.status().context("spawn gen_synthetic_yuv")?;
    if !st.success() {
        bail!("gen_synthetic_yuv failed with {st}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_bench(
    exe: &Path,
    repo: &Path,
    yuv: &Path,
    w: u32,
    h: u32,
    frames: u32,
    fps: u32,
    extra: &[&str],
    json_out: &Path,
    md_out: &Path,
) -> Result<()> {
    if let Some(p) = json_out.parent() {
        fs::create_dir_all(p).ok();
    }
    let mut c = Command::new(exe);
    c.current_dir(repo)
        .arg("--input")
        .arg(yuv)
        .arg("--width")
        .arg(w.to_string())
        .arg("--height")
        .arg(h.to_string())
        .arg("--frames")
        .arg(frames.to_string())
        .arg("--fps")
        .arg(fps.to_string())
        .arg("--report-json")
        .arg(json_out)
        .arg("--report-md")
        .arg(md_out);
    for e in extra {
        c.arg(e);
    }
    let st = c
        .status()
        .with_context(|| format!("bench_srsv2 {:?}", c.get_args()))?;
    if !st.success() {
        bail!("bench_srsv2 failed ({st}) for {}", yuv.display());
    }
    Ok(())
}

fn read_json(path: &Path) -> Result<Value> {
    let s = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(serde_json::from_str(&s)?)
}

fn val_u64(v: &Value, path: &[&str]) -> u64 {
    let mut cur = v;
    for p in path {
        cur = match cur.get(*p) {
            Some(x) => x,
            None => return 0,
        };
    }
    cur.as_u64().unwrap_or(0)
}

fn row_bytes_by_label(report: &Value, label_sub: &str) -> Option<u64> {
    let table = report.get("table")?.as_array()?;
    for row in table {
        let codec = row.get("codec")?.as_str()?;
        if codec.contains(label_sub) {
            return row.get("bytes")?.as_u64();
        }
    }
    None
}

fn ingest_entropy_clip(agg: &mut AggregateEntropy, report: &Value) {
    let Some(arr) = report
        .get("compare_entropy_models")
        .and_then(|x| x.as_array())
    else {
        return;
    };
    let mut static_b: Option<u64> = None;
    let mut context_b: Option<u64> = None;
    for e in arr {
        if !e.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
            continue;
        }
        let mode = e
            .get("entropy_model_mode")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let b = e
            .get("row")
            .and_then(|r| r.get("bytes"))
            .and_then(|x| x.as_u64());
        let Some(b) = b else { continue };
        match mode {
            "static" => static_b = Some(b),
            "context" => context_b = Some(b),
            _ => {}
        }
    }
    let (Some(s), Some(c)) = (static_b, context_b) else {
        return;
    };
    agg.clips_compared += 1;
    agg.sum_delta_total_bytes += i128::from(c) - i128::from(s);
    match c.cmp(&s) {
        std::cmp::Ordering::Less => agg.context_lower_total_bytes += 1,
        std::cmp::Ordering::Greater => agg.static_lower_total_bytes += 1,
        std::cmp::Ordering::Equal => agg.equal_bytes += 1,
    }
}

fn ingest_partition_clip(agg: &mut AggregatePartition, report: &Value) {
    let Some(arr) = report
        .get("compare_partition_costs")
        .and_then(|x| x.as_array())
    else {
        return;
    };
    let mut rdo_rej = 0u64;
    let mut hdr_rej = 0u64;
    let mut fixed_bytes: Option<u64> = None;
    let mut auto_rdo_bytes: Option<u64> = None;
    for e in arr {
        if !e.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
            continue;
        }
        let label = e.get("label").and_then(|x| x.as_str()).unwrap_or("");
        let det = e.get("details").unwrap_or(&Value::Null);
        if label.contains("auto-fast-rdo") {
            rdo_rej += val_u64(det, &["partition", "partition_rejected_by_rdo"]);
            hdr_rej += val_u64(det, &["partition", "partition_rejected_by_header_cost"]);
            auto_rdo_bytes = e
                .get("row")
                .and_then(|r| r.get("bytes"))
                .and_then(|x| x.as_u64());
        }
        if label.contains("pc-fixed16x16") {
            fixed_bytes = e
                .get("row")
                .and_then(|r| r.get("bytes"))
                .and_then(|x| x.as_u64());
        }
    }
    agg.sum_rejected_by_rdo += rdo_rej;
    agg.sum_rejected_by_header_cost += hdr_rej;
    if let (Some(f), Some(a)) = (fixed_bytes, auto_rdo_bytes) {
        agg.clips += 1;
        if a < f {
            agg.auto_fast_rdo_beats_fixed16x16_clips += 1;
        } else if f < a {
            agg.fixed_beats_auto_rdo_clips += 1;
        }
    }
}

fn ingest_b_clip(agg: &mut AggregateB, report: &Value) {
    let Some(arr) = report.get("compare_b_modes").and_then(|x| x.as_array()) else {
        return;
    };
    let mut int_b: Option<u64> = None;
    let mut half_b: Option<u64> = None;
    let mut wgt_b: Option<u64> = None;
    let mut wmb = 0u64;
    let mut hsel = 0u64;
    for e in arr {
        if e.get("error").and_then(|x| x.as_str()).is_some() {
            continue;
        }
        let mode = e.get("mode").and_then(|x| x.as_str()).unwrap_or("");
        let b = e
            .get("row")
            .and_then(|r| r.get("bytes"))
            .and_then(|x| x.as_u64());
        let Some(b) = b else { continue };
        match mode {
            "SRSV2-B-int" => int_b = Some(b),
            "SRSV2-B-half" => half_b = Some(b),
            "SRSV2-B-weighted" => wgt_b = Some(b),
            _ => {}
        }
        let bb = e.get("b_blend").cloned().unwrap_or(Value::Null);
        wmb += val_u64(&bb, &["b_weighted_macroblocks"]);
        hsel += val_u64(&bb, &["b_subpel_blocks_selected"]);
    }
    let (Some(i), Some(h), Some(w)) = (int_b, half_b, wgt_b) else {
        return;
    };
    agg.clips += 1;
    if h < i {
        agg.half_lower_bytes_than_int += 1;
    }
    if w < i {
        agg.weighted_lower_bytes_than_int += 1;
    }
    agg.sum_weighted_mb += wmb;
    agg.sum_halfpel_selected += hsel;
}

fn ingest_bottleneck(ev: &mut BottleneckEvidence, report: &Value) {
    // Use first successful auto-fast-rdo row details if present.
    let Some(arr) = report
        .get("compare_partition_costs")
        .and_then(|x| x.as_array())
    else {
        return;
    };
    for e in arr {
        let label = e.get("label").and_then(|x| x.as_str()).unwrap_or("");
        if !label.contains("auto-fast-rdo")
            || !e.get("ok").and_then(|x| x.as_bool()).unwrap_or(false)
        {
            continue;
        }
        let det = match e.get("details") {
            Some(d) => d,
            None => continue,
        };
        let mv_e = val_u64(det, &["mv_entropy_bytes"]);
        let mv_c = val_u64(det, &["mv_compact_bytes"]);
        let hdr = val_u64(det, &["inter_header_bytes"]);
        let res = val_u64(det, &["inter_residual_bytes"]);
        let pm = val_u64(det, &["partition", "partition_map_bytes"]);
        let pmv = val_u64(det, &["partition", "partition_mv_bytes"]);
        let pres = val_u64(det, &["partition", "partition_residual_bytes"]);
        let total = (mv_e + mv_c + hdr + res + pm + pmv + pres) as f64;
        if total <= 0.0 {
            continue;
        }
        ev.avg_share_mv_header += (mv_e + mv_c + hdr) as f64 / total;
        ev.avg_share_residual += res as f64 / total;
        ev.avg_share_partition_map += pm as f64 / total;
        ev.avg_share_partition_mv += pmv as f64 / total;
        ev.avg_share_partition_residual += pres as f64 / total;
        break;
    }
}

fn finalize_bottleneck(ev: &mut BottleneckEvidence, n: u32) {
    if n == 0 {
        ev.dominant = "insufficient_data".into();
        return;
    }
    let nf = n as f64;
    ev.avg_share_mv_header /= nf;
    ev.avg_share_residual /= nf;
    ev.avg_share_partition_map /= nf;
    ev.avg_share_partition_mv /= nf;
    ev.avg_share_partition_residual /= nf;
    let mut pairs = [
        ("mv_header_compact_entropy", ev.avg_share_mv_header),
        ("inter_residual", ev.avg_share_residual),
        ("partition_map", ev.avg_share_partition_map),
        ("partition_mv", ev.avg_share_partition_mv),
        ("partition_residual", ev.avg_share_partition_residual),
    ];
    pairs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ev.dominant = pairs[0].0.to_string();
}

fn discover_yuvs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut v = Vec::new();
    for ent in fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let ent = ent?;
        let p = ent.path();
        if p.extension().and_then(|s| s.to_str()) == Some("yuv") {
            v.push(p);
        }
    }
    v.sort();
    Ok(v)
}

fn meta_for_yuv(yuv: &Path) -> Result<SyntheticClipMetadata> {
    let mut j = yuv.to_path_buf();
    j.set_extension("json");
    let s = fs::read_to_string(&j).with_context(|| format!("read meta {}", j.display()))?;
    Ok(serde_json::from_str(&s)?)
}

fn main() -> Result<()> {
    let a = Args::parse();
    let repo = a
        .repo_root
        .canonicalize()
        .context("repo_root canonicalize")?;
    let corpus = if a.corpus_dir.is_absolute() {
        a.corpus_dir.clone()
    } else {
        repo.join(&a.corpus_dir)
    };
    let out_md = if a.out_md.is_absolute() {
        a.out_md.clone()
    } else {
        repo.join(&a.out_md)
    };
    let out_json = if a.out_json.is_absolute() {
        a.out_json.clone()
    } else {
        repo.join(&a.out_json)
    };

    if !a.skip_corpus {
        fs::create_dir_all(&corpus).ok();
        run_gen_corpus(&repo, &corpus)?;
    }

    let release = true;
    if !a.skip_build {
        cargo_build_bench(&repo, release)?;
    }
    let exe = bench_exe(&repo, release);
    if !exe.is_file() {
        bail!(
            "missing bench binary at {} (run without --skip-build)",
            exe.display()
        );
    }

    let ffmpeg = ffmpeg_available();
    let mut clips_out: Vec<ClipBench> = Vec::new();
    let mut agg_e = AggregateEntropy::default();
    let mut agg_p = AggregatePartition::default();
    let mut agg_b = AggregateB::default();
    let mut sweep_rows: u64 = 0;
    let mut pareto_hits: u32 = 0;
    let mut bottleneck = BottleneckEvidence::default();
    let mut bottleneck_samples = 0u32;
    let mut ran_x264 = false;
    let mut x264_sample: Option<PathBuf> = None;
    let mut x264_note = String::new();

    let yuvs = discover_yuvs(&corpus)?;
    if yuvs.is_empty() {
        bail!("no .yuv under {}", corpus.display());
    }

    let runs_dir = corpus.parent().unwrap_or(&corpus).join("block6_runs");
    fs::create_dir_all(&runs_dir)?;

    for yuv in &yuvs {
        let stem = yuv.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
        let meta = meta_for_yuv(yuv).with_context(|| format!("meta for {}", yuv.display()))?;
        let w = meta.width;
        let h = meta.height;
        let frames = meta.frames;
        let fps = if meta.fps > 0 {
            meta.fps
        } else {
            meta.fps_num.max(1) / meta.fps_den.max(1)
        };

        let base = runs_dir.join(stem);
        let je = base.join("compare_entropy_models.json");
        let me = base.join("compare_entropy_models.md");
        run_bench(
            &exe,
            &repo,
            yuv,
            w,
            h,
            frames,
            fps,
            &["--inter-syntax", "entropy", "--compare-entropy-models"],
            &je,
            &me,
        )?;
        let ve = read_json(&je)?;
        ingest_entropy_clip(&mut agg_e, &ve);

        let jp = base.join("compare_partition_costs.json");
        let mp = base.join("compare_partition_costs.md");
        run_bench(
            &exe,
            &repo,
            yuv,
            w,
            h,
            frames,
            fps,
            &["--compare-partition-costs"],
            &jp,
            &mp,
        )?;
        let vp = read_json(&jp)?;
        ingest_partition_clip(&mut agg_p, &vp);
        ingest_bottleneck(&mut bottleneck, &vp);
        bottleneck_samples += 1;

        let js = base.join("sweep_quality_bitrate.json");
        let ms = base.join("sweep_quality_bitrate.md");
        run_bench(
            &exe,
            &repo,
            yuv,
            w,
            h,
            frames,
            fps,
            &[
                "--sweep-quality-bitrate",
                "--sweep-ssim-threshold",
                "0.90",
                "--sweep-byte-budget",
                "100000000",
            ],
            &js,
            &ms,
        )?;
        let vs = read_json(&js)?;
        if let Some(n) = vs.get("emitted_rows").and_then(|x| x.as_u64()) {
            sweep_rows += n;
        }
        if let Some(p) = vs
            .get("pareto")
            .and_then(|p| p.get("smallest_bytes_ssim_ge_threshold"))
        {
            if !p.is_null() {
                pareto_hits += 1;
            }
        }

        let jb = base.join("compare_b_modes.json");
        let mb = base.join("compare_b_modes.md");
        run_bench(
            &exe,
            &repo,
            yuv,
            w,
            h,
            frames,
            fps,
            &[
                "--compare-b-modes",
                "--reference-frames",
                "2",
                "--bframes",
                "1",
            ],
            &jb,
            &mb,
        )?;
        let vb = read_json(&jb)?;
        ingest_b_clip(&mut agg_b, &vb);

        if a.try_x264 && ffmpeg && w >= 128 && h >= 128 && !ran_x264 {
            let jxx = runs_dir.join("_sample_x264_compare_b.json");
            let mxx = runs_dir.join("_sample_x264_compare_b.md");
            if run_bench(
                &exe,
                &repo,
                yuv,
                w,
                h,
                frames,
                fps,
                &[
                    "--compare-b-modes",
                    "--reference-frames",
                    "2",
                    "--bframes",
                    "1",
                    "--compare-x264",
                ],
                &jxx,
                &mxx,
            )
            .is_ok()
            {
                ran_x264 = true;
                x264_sample = Some(jxx.clone());
                if let Ok(vx) = read_json(&jxx) {
                    let bint = row_bytes_by_label(&vx, "SRSV2-B-int").unwrap_or(0);
                    let x4 = row_bytes_by_label(&vx, "x264").unwrap_or(0);
                    let ps_b = vx
                        .get("table")
                        .and_then(|t| t.as_array())
                        .and_then(|rows| {
                            rows.iter().find(|r| {
                                r.get("codec").and_then(|c| c.as_str()) == Some("SRSV2-B-int")
                            })
                        })
                        .and_then(|r| r.get("psnr_y"))
                        .and_then(|x| x.as_f64())
                        .unwrap_or(0.0);
                    let px = vx
                        .get("table")
                        .and_then(|t| t.as_array())
                        .and_then(|rows| {
                            rows.iter()
                                .find(|r| r.get("codec").and_then(|c| c.as_str()) == Some("x264"))
                        })
                        .and_then(|r| r.get("psnr_y"))
                        .and_then(|x| x.as_f64())
                        .unwrap_or(0.0);
                    x264_note = format!(
                        "clip={stem} srsv2_B_int_bytes={bint} x264_bytes={x4} psnr_y_B_int={ps_b:.2} psnr_y_x264={px:.2} (CRF vs fixed-QP SRSV2; not bitrate-matched)"
                    );
                }
            }
        }

        clips_out.push(ClipBench {
            clip: stem.to_string(),
            width: w,
            height: h,
            frames,
            entropy_json: je,
            partition_costs_json: jp,
            sweep_json: js,
            b_modes_json: jb,
        });
    }

    finalize_bottleneck(&mut bottleneck, bottleneck_samples);

    let summary = ProgressSummary {
        note: "Engineering measurement only; synthetic corpus; not a product claim.",
        repo_root: repo.display().to_string(),
        corpus_dir: corpus.display().to_string(),
        ffmpeg_on_path: ffmpeg,
        ran_x264,
        x264_sample_json: x264_sample,
        clips: clips_out,
        entropy: agg_e,
        partition: agg_p,
        b_modes: agg_b,
        sweep_rows_total: sweep_rows,
        sweep_pareto_smallest_bytes_threshold_hits: pareto_hits,
        x264_vs_srsv2_note: x264_note,
        bottleneck,
    };

    if let Some(p) = out_json.parent() {
        fs::create_dir_all(p).ok();
    }
    fs::write(&out_json, serde_json::to_string_pretty(&summary)?)?;

    let md = render_md(&summary);
    if let Some(p) = out_md.parent() {
        fs::create_dir_all(p).ok();
    }
    fs::write(&out_md, md)?;
    println!("Wrote {}", out_json.display());
    println!("Wrote {}", out_md.display());
    Ok(())
}

fn render_md(s: &ProgressSummary) -> String {
    let mut o = String::new();
    o.push_str("# SRSV2 progress summary (Block 6)\n\n");
    o.push_str(&format!("{}\n\n", s.note));
    o.push_str(&format!("- **Repo:** `{}`\n", s.repo_root));
    o.push_str(&format!("- **Corpus:** `{}`\n", s.corpus_dir));
    o.push_str(&format!("- **ffmpeg on PATH:** {}\n", s.ffmpeg_on_path));
    o.push_str(&format!("- **Ran x264 compare:** {}\n\n", s.ran_x264));

    o.push_str("## 1. ContextV1 vs StaticV1 (total compressed bytes, entropy-model compare)\n\n");
    o.push_str(&format!(
        "- Clips with both rows OK: **{}**\n",
        s.entropy.clips_compared
    ));
    o.push_str(&format!(
        "- Clips where **context** total bytes < static: **{}**\n",
        s.entropy.context_lower_total_bytes
    ));
    o.push_str(&format!(
        "- Clips where **static** total bytes < context: **{}**\n",
        s.entropy.static_lower_total_bytes
    ));
    o.push_str(&format!(
        "- Clips tie on total bytes: **{}**\n",
        s.entropy.equal_bytes
    ));
    o.push_str(&format!(
        "- Σ(context_bytes − static_bytes) over compared clips: **{}** (negative ⇒ ContextV1 smaller overall)\n\n",
        s.entropy.sum_delta_total_bytes
    ));

    o.push_str("## 2. RDO Fast vs bad partitions (partition cost compare)\n\n");
    o.push_str(&format!(
        "- Clips with fixed16×16 vs auto-fast-rdo pairing: **{}**\n",
        s.partition.clips
    ));
    o.push_str(&format!(
        "- Σ `partition_rejected_by_rdo` (auto-fast-rdo rows): **{}**\n",
        s.partition.sum_rejected_by_rdo
    ));
    o.push_str(&format!(
        "- Σ `partition_rejected_by_header_cost`: **{}**\n\n",
        s.partition.sum_rejected_by_header_cost
    ));

    o.push_str("## 3. AutoFast (rdo cost model) vs fixed16×16 (same compare run)\n\n");
    o.push_str(&format!(
        "- Clips where **auto-fast-rdo** total bytes **<** fixed16×16: **{}**\n",
        s.partition.auto_fast_rdo_beats_fixed16x16_clips
    ));
    o.push_str(&format!(
        "- Clips where **fixed16×16** total bytes **<** auto-fast-rdo: **{}**\n\n",
        s.partition.fixed_beats_auto_rdo_clips
    ));

    o.push_str("## 4. B-half and B-weighted vs B-int\n\n");
    o.push_str(&format!(
        "- Clips with full B row set OK: **{}**\n",
        s.b_modes.clips
    ));
    o.push_str(&format!(
        "- Clips where **B-half** total bytes < B-int: **{}**\n",
        s.b_modes.half_lower_bytes_than_int
    ));
    o.push_str(&format!(
        "- Clips where **B-weighted** total bytes < B-int: **{}**\n",
        s.b_modes.weighted_lower_bytes_than_int
    ));
    o.push_str(&format!(
        "- Σ `b_weighted_macroblocks` over clips: **{}**\n",
        s.b_modes.sum_weighted_mb
    ));
    o.push_str(&format!(
        "- Σ `b_subpel_blocks_selected` over clips: **{}**\n\n",
        s.b_modes.sum_halfpel_selected
    ));

    o.push_str("## 5. SRSV2 vs x264 (optional)\n\n");
    if s.ran_x264 {
        o.push_str(&format!("{}\n", s.x264_vs_srsv2_note));
        if let Some(p) = &s.x264_sample_json {
            o.push_str(&format!("- Artifact: `{}`\n", p.display()));
        }
        o.push_str(
            "- **Answer (matched quality/bitrate):** not evaluated here; bench uses fixed SRSV2 QP vs libx264 CRF without VMAF/bitrate targeting. Treat byte/PSNR deltas as **uncontrolled**.\n\n",
        );
    } else {
        o.push_str("- Not run (`--try-x264` false, or no ffmpeg, or no 128×128 clip).\n\n");
    }

    o.push_str("## 6. Sweep matrix\n\n");
    o.push_str(&format!(
        "- Total sweep rows emitted (all clips): **{}**\n\n",
        s.sweep_rows_total
    ));

    o.push_str("## Next bottleneck (by on-wire byte pressure, normalized)\n\n");
    o.push_str("Per-clip shares are **component_sum / (mv_entropy+mv_compact+inter_header+inter_residual+partition_map+partition_mv+partition_residual)** from the first successful **`SRSV2-pc-auto-fast-rdo`** row; table values are averaged across clips.\n\n");
    o.push_str(&format!(
        "- **Named bottleneck:** `{}`\n",
        s.bottleneck.dominant
    ));
    o.push_str(&format!(
        "- Mean share MV/header/compact/entropy: **{:.4}**\n",
        s.bottleneck.avg_share_mv_header
    ));
    o.push_str(&format!(
        "- Mean share `inter_residual_bytes`: **{:.4}**\n",
        s.bottleneck.avg_share_residual
    ));
    o.push_str(&format!(
        "- Mean share `partition_map_bytes`: **{:.4}**\n",
        s.bottleneck.avg_share_partition_map
    ));
    o.push_str(&format!(
        "- Mean share `partition_mv_bytes`: **{:.4}**\n",
        s.bottleneck.avg_share_partition_mv
    ));
    o.push_str(&format!(
        "- Mean share `partition_residual_bytes`: **{:.4}**\n",
        s.bottleneck.avg_share_partition_residual
    ));
    o.push_str("\n## Artifact index\n\n");
    for c in &s.clips {
        o.push_str(&format!(
            "- `{}`: entropy `{}`, partition `{}`, sweep `{}`, b `{}`\n",
            c.clip,
            c.entropy_json.display(),
            c.partition_costs_json.display(),
            c.sweep_json.display(),
            c.b_modes_json.display()
        ));
    }
    o
}
