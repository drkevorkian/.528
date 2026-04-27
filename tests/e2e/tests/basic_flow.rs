use std::io::Cursor;
use std::time::{SystemTime, UNIX_EPOCH};

use libsrs_audio::AudioFrame;
use libsrs_container::{FileHeader, TrackDescriptor, TrackKind};
use libsrs_demux::DemuxReader;
use libsrs_mux::MuxWriter;
use libsrs_pipeline::{NoopNativeTranscoder, TranscodePipeline};
use libsrs_video::{FrameType, VideoFrame};
use srs_e2e_tests::synthetic_packet;

#[test]
fn synthetic_contract_packet_shape_is_valid() {
    let packet = synthetic_packet(7, 120);
    assert_eq!(packet.data.len(), 128);
    assert!(packet.keyframe);
}

#[test]
fn pipeline_analyze_and_import_native_container_flow() {
    let temp_path = std::env::temp_dir().join(format!(
        "srs-e2e-{}.srsm",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));

    let video = VideoFrame {
        width: 16,
        height: 16,
        frame_index: 0,
        frame_type: FrameType::I,
        data: (0..(16 * 16)).map(|v| v as u8).collect(),
    };
    let encoded = libsrs_video::encode_frame(&video).expect("encode video");
    let tracks = vec![TrackDescriptor {
        track_id: 1,
        kind: TrackKind::Video,
        codec_id: 1,
        flags: 0,
        timescale: 90_000,
        config: [16_u32.to_le_bytes(), 16_u32.to_le_bytes()].concat(),
    }];
    let file = std::fs::File::create(&temp_path).expect("create temp container");
    let mut mux = MuxWriter::new(file, FileHeader::new(1, 4), tracks).expect("init mux");
    mux.write_packet(1, 0, 0, true, &encoded)
        .expect("write video packet");
    let _ = mux.finalize().expect("finalize mux");

    let pipeline = TranscodePipeline::default();

    let metadata = pipeline
        .analyze_source(&temp_path)
        .expect("native metadata should resolve");
    assert_eq!(metadata.format_name, "srsm");
    assert!(!metadata.tracks.is_empty());

    let mut native = NoopNativeTranscoder::default();
    let processed = pipeline
        .import_to_native(&temp_path, &mut native)
        .expect("native ingest should produce packets");
    assert!(processed > 0);

    std::fs::remove_file(&temp_path).expect("cleanup temp container");
}

#[test]
fn native_video_audio_mux_demux_roundtrip() {
    let video = VideoFrame {
        width: 16,
        height: 16,
        frame_index: 0,
        frame_type: FrameType::I,
        data: (0..(16 * 16)).map(|v| v as u8).collect(),
    };
    let audio = AudioFrame {
        sample_rate: 48_000,
        channels: 1,
        frame_index: 0,
        samples: (0..256).map(|v| v as i16).collect(),
    };

    let encoded_video = libsrs_video::encode_frame(&video).expect("encode video");
    let encoded_audio = libsrs_audio::encode_frame(&audio).expect("encode audio");
    let tracks = vec![
        TrackDescriptor {
            track_id: 1,
            kind: TrackKind::Video,
            codec_id: 1,
            flags: 0,
            timescale: 90_000,
            config: [16_u32.to_le_bytes(), 16_u32.to_le_bytes()].concat(),
        },
        TrackDescriptor {
            track_id: 2,
            kind: TrackKind::Audio,
            codec_id: 2,
            flags: 0,
            timescale: 48_000,
            config: [48_000_u32.to_le_bytes().to_vec(), vec![1_u8]].concat(),
        },
    ];

    let mut mux = MuxWriter::new(Cursor::new(Vec::new()), FileHeader::new(2, 4), tracks)
        .expect("init mux");
    mux.write_packet(1, 0, 0, true, &encoded_video)
        .expect("write video packet");
    mux.write_packet(2, 0, 0, true, &encoded_audio)
        .expect("write audio packet");
    let mut out = mux.finalize().expect("finalize");
    out.set_position(0);

    let mut demux = DemuxReader::open(out).expect("open demux");
    let video_packet = demux
        .next_packet()
        .expect("next packet")
        .expect("video packet");
    let decoded_video = libsrs_video::decode_frame(16, 16, 0, FrameType::I, &video_packet.packet.payload)
        .expect("decode video");
    assert_eq!(decoded_video.data, video.data);

    let audio_packet = demux
        .next_packet()
        .expect("next packet")
        .expect("audio packet");
    let decoded_audio =
        libsrs_audio::decode_frame(48_000, 0, &audio_packet.packet.payload).expect("decode audio");
    assert_eq!(decoded_audio.samples, audio.samples);
}
