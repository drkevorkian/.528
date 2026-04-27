use std::env;
use std::fs::{create_dir_all, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use libsrs_container::{FileHeader, TrackDescriptor, TrackKind};
use libsrs_mux::MuxWriter;

fn main() -> Result<()> {
    let out_dir = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_output_dir);
    create_dir_all(&out_dir).with_context(|| format!("create {}", out_dir.display()))?;

    write_seed_header_only(&out_dir)?;
    write_seed_packet_like(&out_dir)?;
    write_seed_with_cue_and_index(&out_dir)?;

    println!("wrote seeds to {}", out_dir.display());
    Ok(())
}

fn write_seed_header_only(out_dir: &Path) -> Result<()> {
    let path = out_dir.join("seed_srsm_header_only");
    let file = File::create(&path).with_context(|| format!("create {}", path.display()))?;
    let tracks = vec![video_track_descriptor()];
    let header = FileHeader::new(1, 8);
    let mux = MuxWriter::new(file, header, tracks)?;
    let _file = mux.finalize()?;
    Ok(())
}

fn write_seed_packet_like(out_dir: &Path) -> Result<()> {
    let path = out_dir.join("seed_srsm_packet_like");
    let file = File::create(&path).with_context(|| format!("create {}", path.display()))?;
    let tracks = vec![video_track_descriptor()];
    let header = FileHeader::new(1, 8);
    let mut mux = MuxWriter::new(file, header, tracks)?;
    mux.write_packet(1, 0, 0, true, b"seedpkt0")?;
    let _file = mux.finalize()?;
    Ok(())
}

fn write_seed_with_cue_and_index(out_dir: &Path) -> Result<()> {
    let path = out_dir.join("seed_srsm_with_cue_and_index");
    let file = File::create(&path).with_context(|| format!("create {}", path.display()))?;
    let tracks = vec![video_track_descriptor()];
    // cue every packet to maximize cue/index parser coverage.
    let header = FileHeader::new(1, 1);
    let mut mux = MuxWriter::new(file, header, tracks)?;
    for idx in 0..3_u64 {
        let payload = [idx as u8; 16];
        mux.write_packet(1, idx * 3_000, idx * 3_000, idx == 0, &payload)?;
    }
    let _file = mux.finalize()?;
    Ok(())
}

fn video_track_descriptor() -> TrackDescriptor {
    let mut config = Vec::new();
    config.extend_from_slice(&16_u32.to_le_bytes());
    config.extend_from_slice(&16_u32.to_le_bytes());
    TrackDescriptor {
        track_id: 1,
        kind: TrackKind::Video,
        codec_id: 1,
        flags: 0,
        timescale: 90_000,
        config,
    }
}

fn default_output_dir() -> PathBuf {
    PathBuf::from("tests/fuzz/corpus/container_parser_demux_reader")
}
