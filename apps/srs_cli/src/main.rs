use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use libsrs_app_config::SrsConfig;
use libsrs_app_services::{
    royalty_free_codec_names, AppServices, MediaInspection, Native528VideoCodec, PlaybackEvent,
    PlaybackSession,
};
use libsrs_licensing_client::{LicenseSnapshot, LicensingClient};
use libsrs_licensing_proto::EntitlementClaims;

#[derive(Debug, Parser)]
#[command(name = "srs-cli", about = "SRS media integration CLI")]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    key: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Encode {
        input: PathBuf,
        output: PathBuf,
        /// `srsv2` (default, YUV420 intra) or `srsv1` (legacy grayscale elementary / mux).
        #[arg(long, default_value = "srsv2")]
        codec: String,
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        height: Option<u32>,
        #[arg(long, default_value_t = 30)]
        fps: u32,
        #[arg(long, default_value = "rgba8")]
        pix_fmt: String,
        /// SRSV2 sequence profile: baseline | main | pro | lossless | screen | ultra | research (see docs/srsv2_design_targets.md).
        #[arg(long, default_value = "main")]
        profile: String,
        #[arg(long, default_value_t = 28)]
        quality: u8,
    },
    Decode {
        input: PathBuf,
        output: PathBuf,
    },
    Mux {
        input: PathBuf,
        output: PathBuf,
    },
    Demux {
        input: PathBuf,
        output: PathBuf,
    },
    Analyze {
        input: PathBuf,
        #[arg(long, default_value_t = false)]
        dump_codec: bool,
    },
    Codecs,
    Play {
        input: PathBuf,
        /// Decode at most this many interleaved steps (video and/or audio packets), or per-stream when filtered.
        #[arg(long, default_value_t = 30)]
        frames: usize,
        #[arg(long, default_value_t = false)]
        no_audio: bool,
        #[arg(long, default_value_t = false)]
        no_video: bool,
        #[arg(long)]
        seek_ms: Option<u64>,
        /// Only print counters and timestamps (no extra banners).
        #[arg(long, default_value_t = false)]
        decode_only: bool,
    },
    Import {
        input: PathBuf,
        output: PathBuf,
        #[arg(long, default_value = "srsv2")]
        codec: String,
    },
    Transcode {
        input: PathBuf,
        output: PathBuf,
        #[arg(long, default_value = "srsv2")]
        codec: String,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    let config = load_config(cli.config.as_deref())?;
    let services = AppServices::default();
    let licensing = LicensingClient::new(config.client.clone())?;
    if let Some(key) = cli.key {
        licensing.set_license_key(key)?;
    }

    match cli.command {
        Commands::Analyze { input, dump_codec } => {
            let inspection = services.inspect_media(&input)?;
            print_inspection(&inspection);
            if dump_codec {
                println!("--- dump-codec ---");
                for t in &inspection.tracks {
                    println!("track {}: codec={} detail={}", t.id, t.codec, t.detail);
                }
            }
        }
        Commands::Codecs => {
            println!("Royalty-free codecs allowed for playback/conversion:");
            for codec in royalty_free_codec_names() {
                println!("- {codec}");
            }
        }
        Commands::Play {
            input,
            frames,
            no_audio,
            no_video,
            seek_ms,
            decode_only,
        } => {
            run_play_command(
                &services,
                &input,
                frames,
                no_audio,
                no_video,
                seek_ms,
                decode_only,
            )?;
        }
        Commands::Encode {
            input,
            output,
            codec,
            width,
            height,
            fps: _fps,
            pix_fmt,
            profile,
            quality,
        } => {
            let snapshot = licensing.refresh_entitlement("srs-cli", env!("CARGO_PKG_VERSION"));
            let claims = require_editor_claims(&snapshot)?;
            match codec.as_str() {
                "srsv1" | "srsv" | "native" => {
                    services.encode_input_to_native_with_video_codec(
                        &input,
                        &output,
                        claims,
                        Native528VideoCodec::Srsv1Legacy,
                    )?;
                }
                "srsv2" => {
                    let out_ext = output
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.to_ascii_lowercase());
                    if let (Some(w), Some(h)) = (width, height) {
                        let srsv2_out = match out_ext.as_deref() {
                            Some("528" | "srsm") => output.with_extension("srsv2"),
                            Some("srsv2") => output.clone(),
                            _ => {
                                return Err(anyhow!(
                                    "SRSV2 RGB encode: output must end in .srsv2 or .528 / .srsm, got {}",
                                    output.display()
                                ));
                            }
                        };
                        encode_srsv2_elementary_file(
                            &input, &srsv2_out, w, h, &pix_fmt, &profile, quality,
                        )?;
                        if matches!(out_ext.as_deref(), Some("528" | "srsm")) {
                            let stem = output.with_extension("");
                            services.mux_elementary_streams(&stem, &output, claims)?;
                        }
                    } else {
                        services.encode_input_to_native_with_video_codec(
                            &input,
                            &output,
                            claims,
                            Native528VideoCodec::Srsv2,
                        )?;
                    }
                }
                other => return Err(anyhow!("unknown --codec {other}")),
            }
            println!("encoded {} -> {}", input.display(), output.display());
        }
        Commands::Decode { input, output } => {
            let snapshot = licensing.refresh_entitlement("srs-cli", env!("CARGO_PKG_VERSION"));
            let claims = require_editor_claims(&snapshot)?;
            services.decode_native_to_raw(&input, &output, claims)?;
            println!("decoded {} -> {}", input.display(), output.display());
        }
        Commands::Mux { input, output } => {
            let snapshot = licensing.refresh_entitlement("srs-cli", env!("CARGO_PKG_VERSION"));
            let claims = require_editor_claims(&snapshot)?;
            services.mux_elementary_streams(&input, &output, claims)?;
            println!("muxed {} -> {}", input.display(), output.display());
        }
        Commands::Demux { input, output } => {
            let snapshot = licensing.refresh_entitlement("srs-cli", env!("CARGO_PKG_VERSION"));
            let claims = require_editor_claims(&snapshot)?;
            services.demux_container_to_elementary(&input, &output, claims)?;
            println!("demuxed {} -> {}", input.display(), output.display());
        }
        Commands::Import {
            input,
            output,
            codec,
        } => {
            let snapshot = licensing.refresh_entitlement("srs-cli", env!("CARGO_PKG_VERSION"));
            let claims = require_editor_claims(&snapshot)?;
            let vc = parse_cli_native_video_codec(&codec)?;
            let count = services.import_to_native_with_video_codec(&input, &output, claims, vc)?;
            println!("processed {count} packets into {}", output.display());
        }
        Commands::Transcode {
            input,
            output,
            codec,
        } => {
            let snapshot = licensing.refresh_entitlement("srs-cli", env!("CARGO_PKG_VERSION"));
            let claims = require_editor_claims(&snapshot)?;
            let vc = parse_cli_native_video_codec(&codec)?;
            let count =
                services.transcode_to_native_with_video_codec(&input, &output, claims, vc)?;
            println!("processed {count} packets into {}", output.display());
        }
    }

    Ok(())
}

fn parse_cli_native_video_codec(s: &str) -> Result<Native528VideoCodec> {
    match s.to_ascii_lowercase().as_str() {
        "srsv2" | "v2" => Ok(Native528VideoCodec::Srsv2),
        "srsv1" | "srsv" | "native" | "v1" => Ok(Native528VideoCodec::Srsv1Legacy),
        other => Err(anyhow!(
            "unknown video codec policy '{other}'; use srsv2 (default) or srsv1"
        )),
    }
}

fn load_config(path: Option<&Path>) -> Result<SrsConfig> {
    match path {
        Some(path) => SrsConfig::load_from_path(path),
        None => SrsConfig::load(),
    }
}

fn require_editor_claims(snapshot: &LicenseSnapshot) -> Result<&EntitlementClaims> {
    if snapshot.allows_editor() {
        return snapshot
            .claims
            .as_ref()
            .ok_or_else(|| anyhow!("editor verification succeeded without claims"));
    }
    Err(anyhow!(snapshot.message.clone()))
}

fn run_play_command(
    services: &AppServices,
    input: &Path,
    frames: usize,
    no_audio: bool,
    no_video: bool,
    seek_ms: Option<u64>,
    decode_only: bool,
) -> Result<()> {
    let inspection = services.inspect_media(input)?;
    if !decode_only {
        print_inspection(&inspection);
    }
    let mut session =
        PlaybackSession::open(input).map_err(|e| anyhow::anyhow!("open playback session: {e}"))?;
    if let Some(ms) = seek_ms {
        session
            .seek_ms(ms)
            .map_err(|e| anyhow::anyhow!("seek: {e}"))?;
        if !decode_only {
            println!("seek_ms: {ms} (ok)");
        }
    }
    if no_audio && no_video {
        return Err(anyhow::anyhow!(
            "--no-audio and --no-video cannot be combined"
        ));
    }
    let mut steps = 0_usize;
    if no_video {
        while steps < frames {
            match session.decode_next_audio_chunk() {
                Ok(None) => break,
                Ok(Some(chunk)) => {
                    let pts_ms = chunk
                        .pts_ticks
                        .saturating_mul(1000)
                        .saturating_div(chunk.timescale_hz.max(1) as u64);
                    if !decode_only {
                        println!(
                            "audio chunk: pts_ms={pts_ms} samples={}",
                            chunk.samples_interleaved.len()
                        );
                    }
                    steps += 1;
                }
                Err(e) => return Err(anyhow::anyhow!("{e}")),
            }
        }
    } else if no_audio {
        while steps < frames {
            match session.decode_next_video_frame() {
                Ok(None) => break,
                Ok(Some(v)) => {
                    let pts_ms = v
                        .pts_ticks
                        .saturating_mul(1000)
                        .saturating_div(v.timescale_hz.max(1) as u64);
                    if !decode_only {
                        println!(
                            "video frame: pts_ms={pts_ms} {}x{} crc32c={:08x}",
                            v.width, v.height, v.payload_crc32c
                        );
                    }
                    steps += 1;
                }
                Err(e) => return Err(anyhow::anyhow!("{e}")),
            }
        }
    } else {
        while steps < frames {
            match session.decode_next_step() {
                Ok(PlaybackEvent::EndOfStream) => {
                    if !decode_only {
                        println!("end_of_stream");
                    }
                    break;
                }
                Ok(PlaybackEvent::Video(v)) => {
                    let pts_ms = v
                        .pts_ticks
                        .saturating_mul(1000)
                        .saturating_div(v.timescale_hz.max(1) as u64);
                    if !decode_only {
                        println!(
                            "video: pts_ms={pts_ms} {}x{} crc32c={:08x}",
                            v.width, v.height, v.payload_crc32c
                        );
                    }
                    steps += 1;
                }
                Ok(PlaybackEvent::Audio(a)) => {
                    let pts_ms = a
                        .pts_ticks
                        .saturating_mul(1000)
                        .saturating_div(a.timescale_hz.max(1) as u64);
                    if !decode_only {
                        println!(
                            "audio: pts_ms={pts_ms} samples={}",
                            a.samples_interleaved.len()
                        );
                    }
                    steps += 1;
                }
                Err(e) => return Err(anyhow::anyhow!("{e}")),
            }
        }
    }
    println!(
        "playback decode summary: steps={} video_frames={} audio_chunks={} file={}",
        steps,
        session.decoded_video_frames,
        session.decoded_audio_chunks,
        input.display()
    );
    Ok(())
}

fn rgba_to_rgb888(buf: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w.saturating_mul(h).saturating_mul(3));
    for i in 0..w.saturating_mul(h) {
        let j = i.saturating_mul(4);
        out.extend_from_slice(&buf[j..j + 3]);
    }
    out
}

fn bgra_to_rgb888(buf: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w.saturating_mul(h).saturating_mul(3));
    for i in 0..w.saturating_mul(h) {
        let j = i.saturating_mul(4);
        out.push(buf[j + 2]);
        out.push(buf[j + 1]);
        out.push(buf[j]);
    }
    out
}

/// One-frame SRSV2 intra elementary stream (`.srsv2`).
fn encode_srsv2_elementary_file(
    input: &Path,
    srsv2_out: &Path,
    width: u32,
    height: u32,
    pix_fmt: &str,
    profile: &str,
    quality: u8,
) -> Result<()> {
    use libsrs_video::{
        encode_yuv420_intra_payload, rgb888_full_to_yuv420_bt709, ChromaSiting, ColorPrimaries,
        ColorRange, MatrixCoefficients, PixelFormat, SrsV2EncodeSettings, SrsVideoProfile,
        TransferFunction, VideoSequenceHeaderV2, VideoStreamWriterV2,
    };

    let raw = std::fs::read(input).with_context(|| format!("read {}", input.display()))?;
    let profile_e = match profile.to_ascii_lowercase().as_str() {
        "baseline" => SrsVideoProfile::Baseline,
        "main" => SrsVideoProfile::Main,
        "pro" => SrsVideoProfile::Pro,
        "lossless" => SrsVideoProfile::Lossless,
        "screen" => SrsVideoProfile::Screen,
        "ultra" => SrsVideoProfile::Ultra,
        "research" => SrsVideoProfile::Research,
        _ => return Err(anyhow!("unknown --profile {profile}")),
    };
    let w = width as usize;
    let h = height as usize;
    let rgb: Vec<u8> = match pix_fmt.to_ascii_lowercase().as_str() {
        "rgb8" => {
            let need = w
                .checked_mul(h)
                .and_then(|x| x.checked_mul(3))
                .ok_or_else(|| anyhow!("dimension overflow"))?;
            if raw.len() < need {
                return Err(anyhow!(
                    "input size {} < expected {} bytes for rgb8",
                    raw.len(),
                    need
                ));
            }
            raw[..need].to_vec()
        }
        "rgba8" => {
            let need = w
                .checked_mul(h)
                .and_then(|x| x.checked_mul(4))
                .ok_or_else(|| anyhow!("dimension overflow"))?;
            if raw.len() < need {
                return Err(anyhow!(
                    "input size {} < expected {} bytes for rgba8",
                    raw.len(),
                    need
                ));
            }
            rgba_to_rgb888(&raw[..need], w, h)
        }
        "bgra8" => {
            let need = w
                .checked_mul(h)
                .and_then(|x| x.checked_mul(4))
                .ok_or_else(|| anyhow!("dimension overflow"))?;
            if raw.len() < need {
                return Err(anyhow!(
                    "input size {} < expected {} bytes for bgra8",
                    raw.len(),
                    need
                ));
            }
            bgra_to_rgb888(&raw[..need], w, h)
        }
        _ => {
            return Err(anyhow!(
                "unsupported --pix-fmt {pix_fmt} for srsv2 (use rgb8, rgba8, bgra8)"
            ));
        }
    };
    let seq = VideoSequenceHeaderV2 {
        width,
        height,
        profile: profile_e,
        pixel_format: PixelFormat::Yuv420p8,
        color_primaries: ColorPrimaries::Bt709,
        transfer: TransferFunction::Sdr,
        matrix: MatrixCoefficients::Bt709,
        chroma_siting: ChromaSiting::Center,
        range: ColorRange::Limited,
        disable_loop_filter: true,
        max_ref_frames: 0,
    };
    let yuv = rgb888_full_to_yuv420_bt709(&rgb, width, height, ColorRange::Limited)
        .map_err(|e| anyhow!("color convert: {e}"))?;
    let qp = quality.clamp(1, 51);
    let payload =
        encode_yuv420_intra_payload(&seq, &yuv, 0, qp, &SrsV2EncodeSettings::default(), None)
            .map_err(|e| anyhow!("encode: {e}"))?;
    let f = File::create(srsv2_out).with_context(|| format!("create {}", srsv2_out.display()))?;
    let mut wr = VideoStreamWriterV2::new(f, &seq).map_err(|e| anyhow!("srsv2 writer: {e}"))?;
    wr.write_frame_payload(0, &payload)
        .map_err(|e| anyhow!("write frame: {e}"))?;
    Ok(())
}

fn print_inspection(inspection: &MediaInspection) {
    println!("{}", inspection.summary);
    println!("format: {}", inspection.format_name);
    println!("duration_ms: {:?}", inspection.duration_ms);
    if let Some(packet_count) = inspection.packet_count {
        println!("packet_count: {packet_count}");
    }
    if let Some(frame_count) = inspection.frame_count {
        println!("frame_count: {frame_count}");
    }
    if let Some(index_entries) = inspection.index_entries {
        println!("index_entries: {index_entries}");
    }
    println!("tracks: {}", inspection.tracks.len());
    for track in &inspection.tracks {
        println!(
            "track {}: kind={} codec={} role={} supported_without_license={} detail={}",
            track.id,
            track.kind,
            track.codec,
            track.role,
            track.supported_without_license,
            track.detail
        );
    }
}
