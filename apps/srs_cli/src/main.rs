use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use libsrs_app_config::SrsConfig;
use libsrs_app_services::{
    royalty_free_codec_names, AppServices, MediaInspection, PlaybackEvent, PlaybackSession,
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
    },
    Transcode {
        input: PathBuf,
        output: PathBuf,
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
        Commands::Analyze { input } => {
            print_inspection(&services.inspect_media(&input)?);
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
        Commands::Encode { input, output } => {
            let snapshot = licensing.refresh_entitlement("srs-cli", env!("CARGO_PKG_VERSION"));
            let claims = require_editor_claims(&snapshot)?;
            services.encode_input_to_native(&input, &output, claims)?;
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
        Commands::Import { input, output } => {
            let snapshot = licensing.refresh_entitlement("srs-cli", env!("CARGO_PKG_VERSION"));
            let claims = require_editor_claims(&snapshot)?;
            let count = services.import_to_native(&input, &output, claims)?;
            println!("processed {count} packets into {}", output.display());
        }
        Commands::Transcode { input, output } => {
            let snapshot = licensing.refresh_entitlement("srs-cli", env!("CARGO_PKG_VERSION"));
            let claims = require_editor_claims(&snapshot)?;
            let count = services.transcode_to_native(&input, &output, claims)?;
            println!("processed {count} packets into {}", output.display());
        }
    }

    Ok(())
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
