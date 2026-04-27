use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use libsrs_app_config::SrsConfig;
use libsrs_app_services::{royalty_free_codec_names, AppServices, MediaInspection};
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
    Encode { input: PathBuf, output: PathBuf },
    Decode { input: PathBuf, output: PathBuf },
    Mux { input: PathBuf, output: PathBuf },
    Demux { input: PathBuf, output: PathBuf },
    Analyze { input: PathBuf },
    Codecs,
    Play { input: PathBuf },
    Import { input: PathBuf, output: PathBuf },
    Transcode { input: PathBuf, output: PathBuf },
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
        Commands::Play { input } => {
            print_inspection(&services.inspect_media(&input)?);
            println!("playback entrypoint validated for {}", input.display());
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
