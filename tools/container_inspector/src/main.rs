use std::collections::BTreeMap;
use std::env;
use std::fs::File;
use std::io::{self, BufReader};

use libsrs_demux::DemuxReader;

fn main() -> io::Result<()> {
    let path = env::args().nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: container_inspector <file.528> (legacy .srsm accepted)",
        )
    })?;

    let file = File::open(path)?;
    let mut demux = DemuxReader::open(BufReader::new(file))?;
    demux.rebuild_index()?;

    println!("== Header ==");
    println!("version: {}", demux.header().version);
    println!("flags: {}", demux.header().flags);
    println!("track_count: {}", demux.header().track_count);
    println!(
        "cue_interval_packets: {}",
        demux.header().cue_interval_packets
    );
    println!();

    println!("== Tracks ==");
    for track in demux.tracks() {
        println!(
            "id={} kind={:?} codec={} timescale={} config={} bytes",
            track.track_id,
            track.kind,
            track.codec_id,
            track.timescale,
            track.config.len()
        );
    }
    println!();

    let mut packet_count = 0u64;
    let mut per_track: BTreeMap<u16, u64> = BTreeMap::new();
    let mut min_pts = u64::MAX;
    let mut max_pts = 0u64;
    demux.reset_to_data_start()?;
    while let Some(packet) = demux.next_packet()? {
        packet_count += 1;
        *per_track.entry(packet.packet.header.track_id).or_default() += 1;
        min_pts = min_pts.min(packet.packet.header.pts);
        max_pts = max_pts.max(packet.packet.header.pts);
    }

    println!("== Packet Stats ==");
    println!("total_packets: {}", packet_count);
    for (track, count) in per_track {
        println!("track {} packets: {}", track, count);
    }
    if packet_count > 0 {
        println!("pts_range: {}..={}", min_pts, max_pts);
    }
    println!();

    println!("== Index Summary ==");
    println!("entries: {}", demux.index().len());
    if let Some(first) = demux.index().first() {
        println!("first_offset: {}", first.file_offset);
    }
    if let Some(last) = demux.index().last() {
        println!("last_offset: {}", last.file_offset);
    }
    Ok(())
}
