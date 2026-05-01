//! SRSV2 vs optional **libx264** comparison harness (engineering measurements only — not marketing).

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use libsrs_container::{FileHeader, TrackDescriptor, TrackKind};
use libsrs_mux::MuxWriter;
use libsrs_video::{
    decode_yuv420_intra_payload, encode_sequence_header_v2, encode_yuv420_intra_payload,
    gray8_packed_to_yuv420p8_neutral, SrsV2EncodeSettings, VideoSequenceHeaderV2,
};
use quality_metrics::{compression_ratio, psnr_u8, synthetic::SyntheticMeta};
use serde::Serialize;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    yuv: PathBuf,
    #[arg(long)]
    meta: PathBuf,
    /// Write JSON report here.
    #[arg(long)]
    out_json: Option<PathBuf>,
    /// Write Markdown summary here.
    #[arg(long)]
    out_md: Option<PathBuf>,
    #[arg(long, default_value_t = 28)]
    qp: u8,
}

#[derive(Debug, Serialize)]
struct Srsv2Arm {
    encoded_payload_bytes: u64,
    compression_ratio_vs_raw: f64,
    encode_wall_seconds: f64,
    decode_wall_seconds: f64,
    encode_fps: f64,
    decode_fps: f64,
    psnr_y_db: f64,
    ssim_y_proxy: Option<f64>,
}

#[derive(Debug, Serialize)]
struct H264Arm {
    encoded_file_bytes: u64,
    compression_ratio_vs_raw: f64,
    encode_wall_seconds: f64,
    decode_wall_seconds: f64,
    encode_fps: f64,
    decode_fps: f64,
    psnr_y_db: f64,
}

#[derive(Debug, Serialize)]
struct Report {
    note: &'static str,
    meta: SyntheticMeta,
    raw_bytes: u64,
    srsv2: Srsv2Arm,
    h264: Option<H264Arm>,
    ffmpeg_skipped: Option<&'static str>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let meta_json = fs::read_to_string(&args.meta).context("read meta json")?;
    let meta: SyntheticMeta = serde_json::from_str(&meta_json).context("parse meta json")?;
    let raw = fs::read(&args.yuv).context("read yuv")?;
    let w = meta.width;
    let h = meta.height;
    let frames = meta.frames;
    let frame_sz = quality_metrics::synthetic::yuv420p8_frame_bytes(w, h);
    if raw.len() != frame_sz * frames as usize {
        return Err(anyhow!(
            "yuv size {} does not match meta {}×{}×{} frames (expected {} bytes/frame)",
            raw.len(),
            w,
            h,
            frames,
            frame_sz
        ));
    }

    let seq = VideoSequenceHeaderV2::intra_main_yuv420_bt709_limited(w, h);
    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(frames as usize);
    let t0 = Instant::now();
    for fi in 0..frames {
        let chunk = &raw[fi as usize * frame_sz..][..frame_sz];
        let gray = y420_to_gray8(chunk, w, h)?;
        let yuv = gray8_packed_to_yuv420p8_neutral(&gray, w, h)?;
        let p = encode_yuv420_intra_payload(
            &seq,
            &yuv,
            fi,
            args.qp,
            &SrsV2EncodeSettings::default(),
            None,
        )
        .map_err(|e| anyhow!("{e}"))?;
        payloads.push(p);
    }
    let enc_secs = t0.elapsed().as_secs_f64();

    let sum_payload: u64 = payloads.iter().map(|p| p.len() as u64).sum();
    let raw_u64 = raw.len() as u64;

    let mut tmp528 = std::env::temp_dir();
    tmp528.push(format!(
        "cmp528-{}.528",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let cfg = encode_sequence_header_v2(&seq).to_vec();
        let tracks = vec![TrackDescriptor {
            track_id: 1,
            kind: TrackKind::Video,
            codec_id: 3,
            flags: 0,
            timescale: meta.fps.max(1),
            config: cfg,
        }];
        let f = File::create(&tmp528)?;
        let mut mux = MuxWriter::new(f, FileHeader::new(1, 4), tracks)?;
        let mut pts = 0_u64;
        for p in &payloads {
            mux.write_packet(1, pts, pts, true, p)?;
            pts = pts.saturating_add(1_000_000 / u64::from(meta.fps.max(1)));
        }
        mux.finalize()?;
    }

    let mut luma_dec = Vec::with_capacity((w * h * frames) as usize);
    let t1 = Instant::now();
    for pl in payloads.iter() {
        let dec = decode_yuv420_intra_payload(&seq, pl).map_err(|e| anyhow!("{e}"))?;
        luma_dec.extend_from_slice(dec.yuv.y.samples.as_slice());
    }
    let dec_secs = t1.elapsed().as_secs_f64();

    let mut luma_src = Vec::with_capacity((w * h * frames) as usize);
    for fi in 0..frames {
        let chunk = &raw[fi as usize * frame_sz..][..frame_sz];
        luma_src.extend_from_slice(&chunk[..(w * h) as usize]);
    }

    let psnr_y = psnr_u8(&luma_src, &luma_dec, 255.0).map_err(|e| anyhow!("{e}"))?;
    let ssim_y = quality_metrics::ssim_u8_simple(
        &luma_src,
        &luma_dec,
        w as usize,
        h as usize * frames as usize,
    )
    .ok();

    let srsv2 = Srsv2Arm {
        encoded_payload_bytes: sum_payload,
        compression_ratio_vs_raw: compression_ratio(raw_u64, sum_payload.max(1)),
        encode_wall_seconds: enc_secs,
        decode_wall_seconds: dec_secs,
        encode_fps: frames as f64 / enc_secs.max(1e-9),
        decode_fps: frames as f64 / dec_secs.max(1e-9),
        psnr_y_db: psnr_y,
        ssim_y_proxy: ssim_y,
    };

    let (h264, ffmpeg_skipped) = if codec_compare::ffmpeg_available() {
        let tmp_mp4 = std::env::temp_dir().join(format!(
            "cmp264-{}.mp4",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let tmp_dec = std::env::temp_dir().join(format!(
            "cmp264dec-{}.yuv",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let fps = meta.fps.max(1);
        let t_h0 = Instant::now();
        let st = Command::new("ffmpeg")
            .arg("-y")
            .arg("-f")
            .arg("rawvideo")
            .arg("-pix_fmt")
            .arg("yuv420p")
            .arg("-s")
            .arg(format!("{w}x{h}"))
            .arg("-r")
            .arg(fps.to_string())
            .arg("-i")
            .arg(args.yuv.as_os_str())
            .arg("-frames:v")
            .arg(frames.to_string())
            .arg("-c:v")
            .arg("libx264")
            .arg("-preset")
            .arg("ultrafast")
            .arg("-crf")
            .arg("28")
            .arg("-an")
            .arg(tmp_mp4.as_os_str())
            .status()
            .context("spawn ffmpeg encode")?;
        let h264_enc = t_h0.elapsed().as_secs_f64();
        if !st.success() {
            (None, Some("ffmpeg libx264 encode failed"))
        } else {
            let sz = fs::metadata(&tmp_mp4).map(|m| m.len()).unwrap_or(0);
            let t_h1 = Instant::now();
            let st2 = Command::new("ffmpeg")
                .arg("-y")
                .arg("-i")
                .arg(tmp_mp4.as_os_str())
                .arg("-f")
                .arg("rawvideo")
                .arg("-pix_fmt")
                .arg("yuv420p")
                .arg("-frames:v")
                .arg(frames.to_string())
                .arg(tmp_dec.as_os_str())
                .status()
                .context("spawn ffmpeg decode")?;
            let h264_dec = t_h1.elapsed().as_secs_f64();
            let l264 = if st2.success() {
                fs::read(&tmp_dec).unwrap_or_default()
            } else {
                vec![]
            };
            let mut l264_y = Vec::with_capacity(luma_src.len());
            if l264.len() >= frame_sz * frames as usize {
                for fi in 0..frames {
                    let chunk = &l264[fi as usize * frame_sz..][..frame_sz];
                    let ylen = (w * h) as usize;
                    l264_y.extend_from_slice(&chunk[..ylen]);
                }
            }
            let psnr_h = if l264_y.len() == luma_src.len() {
                psnr_u8(&luma_src, &l264_y, 255.0).unwrap_or(f64::NAN)
            } else {
                f64::NAN
            };
            let _ = fs::remove_file(&tmp_mp4);
            let _ = fs::remove_file(&tmp_dec);
            (
                Some(H264Arm {
                    encoded_file_bytes: sz,
                    compression_ratio_vs_raw: compression_ratio(raw_u64, sz.max(1)),
                    encode_wall_seconds: h264_enc,
                    decode_wall_seconds: h264_dec,
                    encode_fps: frames as f64 / h264_enc.max(1e-9),
                    decode_fps: frames as f64 / h264_dec.max(1e-9),
                    psnr_y_db: psnr_h,
                }),
                None,
            )
        }
    } else {
        (None, Some("ffmpeg not on PATH — SRSV2-only metrics"))
    };

    let report = Report {
        note: "Engineering measurement only; not a competitive claim.",
        meta: meta.clone(),
        raw_bytes: raw_u64,
        srsv2,
        h264,
        ffmpeg_skipped,
    };

    if let Some(p) = args.out_json.as_ref() {
        let f = File::create(p).context("create json out")?;
        serde_json::to_writer_pretty(BufWriter::new(f), &report)?;
    }
    if let Some(p) = args.out_md.as_ref() {
        let mut s = String::new();
        s.push_str("# Codec compare (SRSV2 vs optional H.264)\n\n");
        s.push_str("| Arm | Enc bytes | Ratio vs raw | Enc s | Dec s | PSNR-Y (luma) |\n");
        s.push_str("|-----|-----------|--------------|-------|-------|---------------|\n");
        s.push_str(&format!(
            "| SRSV2 payloads | {} | {:.3} | {:.4} | {:.4} | {:.2} |\n",
            report.srsv2.encoded_payload_bytes,
            report.srsv2.compression_ratio_vs_raw,
            report.srsv2.encode_wall_seconds,
            report.srsv2.decode_wall_seconds,
            report.srsv2.psnr_y_db
        ));
        if let Some(h) = &report.h264 {
            s.push_str(&format!(
                "| H.264 file | {} | {:.3} | {:.4} | {:.4} | {:.2} |\n",
                h.encoded_file_bytes,
                h.compression_ratio_vs_raw,
                h.encode_wall_seconds,
                h.decode_wall_seconds,
                h.psnr_y_db
            ));
        }
        if let Some(note) = report.ffmpeg_skipped {
            s.push_str(&format!("\n*{note}*\n"));
        }
        fs::write(p, s)?;
    }

    println!("{}", serde_json::to_string_pretty(&report)?);
    let _ = fs::remove_file(&tmp528);
    Ok(())
}

/// Packed YUV420 planar → packed grayscale (Y-only duplicated per pixel for encoder helper).
fn y420_to_gray8(chunk: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    let ylen = (w * h) as usize;
    if chunk.len() < ylen {
        return Err(anyhow!("truncated yuv chunk"));
    }
    let y = &chunk[..ylen];
    Ok(y.to_vec())
}
