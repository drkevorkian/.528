use std::io::Cursor;

use libsrs_container::{
    decode_track_descriptor, encode_track_descriptor, FileHeader, PacketFlags, TrackDescriptor,
    TrackKind, MAX_TRACK_CONFIG_BYTES,
};
use libsrs_demux::DemuxReader;
use libsrs_mux::MuxWriter;

fn make_tracks() -> Vec<TrackDescriptor> {
    vec![
        TrackDescriptor {
            track_id: 1,
            kind: TrackKind::Video,
            codec_id: 101,
            flags: 0,
            timescale: 90_000,
            config: vec![0x01, 0x64, 0x00, 0x1F],
        },
        TrackDescriptor {
            track_id: 2,
            kind: TrackKind::Audio,
            codec_id: 201,
            flags: 0,
            timescale: 48_000,
            config: vec![0x11, 0x90],
        },
    ]
}

#[test]
fn golden_mux_demux_roundtrip() {
    let cursor = Cursor::new(Vec::new());
    let header = FileHeader::new(2, 2);
    let tracks = make_tracks();
    let mut mux = MuxWriter::new(cursor, header, tracks.clone()).expect("mux init");

    mux.write_packet(1, 0, 0, true, b"video-0").expect("packet");
    mux.write_packet(2, 0, 0, false, b"audio-0")
        .expect("packet");
    mux.write_packet(1, 3000, 3000, false, b"video-1")
        .expect("packet");
    mux.write_packet(2, 1024, 1024, false, b"audio-1")
        .expect("packet");

    let mut output = mux.finalize().expect("finalize");
    output.set_position(0);

    let mut demux = DemuxReader::open(output).expect("demux open");
    assert_eq!(demux.header().track_count, 2);
    assert_eq!(demux.tracks(), tracks.as_slice());

    demux.rebuild_index().expect("index");
    assert_eq!(demux.header().version, 2);
    assert_eq!(demux.index().len(), 4);

    demux.reset_to_data_start().expect("reset");
    let mut packets = Vec::new();
    while let Some(packet) = demux.next_packet().expect("next packet") {
        packets.push(packet.packet);
    }
    assert_eq!(packets.len(), 4);
    assert_eq!(packets[0].payload, b"video-0");
    assert_eq!(
        packets[0].header.flags & PacketFlags::KEYFRAME,
        PacketFlags::KEYFRAME
    );
    assert_eq!(packets[3].payload, b"audio-1");

    let seek_hit = demux.seek_nearest(3000).expect("seek").expect("index hit");
    assert_eq!(seek_hit.track_id, 1);
    assert_eq!(seek_hit.pts, 3000);
}

#[test]
fn truncated_file_is_detected() {
    let cursor = Cursor::new(Vec::new());
    let header = FileHeader::new(1, 0);
    let tracks = vec![TrackDescriptor {
        track_id: 1,
        kind: TrackKind::Video,
        codec_id: 101,
        flags: 0,
        timescale: 90_000,
        config: vec![],
    }];
    let mut mux = MuxWriter::new(cursor, header, tracks).expect("mux init");
    mux.write_packet(1, 0, 0, true, b"abc").expect("packet");
    let output = mux.finalize().expect("finalize").into_inner();

    let truncated = output[..output.len() - 2].to_vec();
    let mut demux = DemuxReader::open(Cursor::new(truncated)).expect("demux open");
    let err = demux.rebuild_index().expect_err("must fail");
    assert!(matches!(
        err.kind(),
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::InvalidData
    ));
}

#[test]
fn bad_crc_is_detected() {
    let cursor = Cursor::new(Vec::new());
    let header = FileHeader::new(1, 0);
    let tracks = vec![TrackDescriptor {
        track_id: 1,
        kind: TrackKind::Video,
        codec_id: 101,
        flags: 0,
        timescale: 90_000,
        config: vec![],
    }];
    let mut mux = MuxWriter::new(cursor, header, tracks).expect("mux init");
    mux.write_packet(1, 0, 0, true, b"payload").expect("packet");
    let mut output = mux.finalize().expect("finalize").into_inner();

    let flip_idx = output
        .windows(4)
        .position(|window| window == b"SBLK")
        .expect("block magic")
        + 25;
    output[flip_idx] ^= 0xFF;

    let mut demux = DemuxReader::open(Cursor::new(output)).expect("demux open");
    let err = demux.next_packet().expect_err("must fail");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn legacy_srsm_v1_container_still_decodeable() {
    let cursor = Cursor::new(Vec::new());
    let header = FileHeader::new_legacy(1, 0);
    let tracks = vec![TrackDescriptor {
        track_id: 1,
        kind: TrackKind::Video,
        codec_id: 101,
        flags: 0,
        timescale: 90_000,
        config: vec![1, 2, 3],
    }];
    let mut mux = MuxWriter::new(cursor, header, tracks).expect("mux init");
    mux.write_packet(1, 0, 0, true, b"legacy").expect("packet");
    let mut output = mux.finalize().expect("finalize");
    output.set_position(0);

    let mut demux = DemuxReader::open(output).expect("demux open");
    assert_eq!(demux.header().version, 1);
    assert!(!demux.header().block_checksum_is_crc32c());
    demux.rebuild_index().expect("index");
    demux.reset_to_data_start().expect("reset");
    let pkt = demux.next_packet().expect("packet").expect("one");
    assert_eq!(pkt.packet.payload, b"legacy");
}

#[test]
fn oversize_track_config_encode_rejected() {
    let track = TrackDescriptor {
        track_id: 1,
        kind: TrackKind::Video,
        codec_id: 1,
        flags: 0,
        timescale: 90_000,
        config: vec![0u8; MAX_TRACK_CONFIG_BYTES + 1],
    };
    let err = encode_track_descriptor(&track).expect_err("config too large");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn oversize_track_config_decode_rejected() {
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.push(TrackKind::Video as u8);
    bytes.push(0);
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&90_000u32.to_le_bytes());
    bytes.extend_from_slice(&((MAX_TRACK_CONFIG_BYTES as u32) + 1).to_le_bytes());
    let mut cursor = Cursor::new(bytes);
    let err = decode_track_descriptor(&mut cursor).expect_err("limit");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}
