use std::env;
use std::fs;
use std::process;

use libsrs_audio::{
    parse_audio_frame_packet_header, AudioStreamHeader, PACKET_SYNC as AUDIO_SYNC,
    STREAM_MAGIC as AUDIO_MAGIC,
};
use libsrs_video::{
    parse_video_frame_packet_header, FrameType, VideoStreamHeader, PACKET_SYNC as VIDEO_SYNC,
    STREAM_MAGIC as VIDEO_MAGIC,
};

fn main() {
    if let Err(err) = run() {
        eprintln!("bitstream_dump error: {err}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args();
    let _exe = args.next();
    let path = args
        .next()
        .ok_or_else(|| "usage: bitstream_dump <path-to-elementary-stream>".to_string())?;
    if args.next().is_some() {
        return Err("usage: bitstream_dump <path-to-elementary-stream>".to_string());
    }

    let bytes = fs::read(&path).map_err(|err| format!("failed to read '{path}': {err}"))?;
    if bytes.len() < 16 {
        return Err("stream too small to include header".to_string());
    }
    let mut header_bytes = [0_u8; 16];
    header_bytes.copy_from_slice(&bytes[0..16]);

    if header_bytes[0..4] == VIDEO_MAGIC {
        dump_video_stream(&bytes, header_bytes)?;
    } else if header_bytes[0..4] == AUDIO_MAGIC {
        dump_audio_stream(&bytes, header_bytes)?;
    } else {
        return Err("unknown stream magic; expected SRSV or SRSA".to_string());
    }
    Ok(())
}

fn dump_video_stream(stream: &[u8], header_bytes: [u8; 16]) -> Result<(), String> {
    let header = VideoStreamHeader::decode(header_bytes).map_err(|e| e.to_string())?;
    println!("kind=video");
    println!("version=1");
    println!("width={}", header.width);
    println!("height={}", header.height);

    let mut cursor = 16usize;
    let mut packet_idx = 0usize;
    while cursor < stream.len() {
        if stream.len().saturating_sub(cursor) < 16 {
            return Err("truncated video packet header".to_string());
        }
        if stream[cursor..cursor + 2] != VIDEO_SYNC {
            return Err(format!("invalid video packet sync at offset {cursor}"));
        }
        let meta = parse_video_frame_packet_header(&stream[cursor..cursor + 16])
            .map_err(|e| format!("failed parsing video packet header: {e}"))?;
        let payload_start = cursor + 16;
        let payload_end = payload_start + meta.payload_len as usize;
        if payload_end > stream.len() {
            return Err("truncated video packet payload".to_string());
        }
        println!(
            "packet[{packet_idx}] frame_index={} frame_type={} payload_len={} crc32=0x{:08x}",
            meta.frame_index,
            frame_type_name(meta.frame_type),
            meta.payload_len,
            meta.crc32
        );
        packet_idx += 1;
        cursor = payload_end;
    }
    Ok(())
}

fn dump_audio_stream(stream: &[u8], header_bytes: [u8; 16]) -> Result<(), String> {
    let header = AudioStreamHeader::decode(header_bytes).map_err(|e| e.to_string())?;
    println!("kind=audio");
    println!("version=1");
    println!("sample_rate={}", header.sample_rate);
    println!("channels={}", header.channels);

    let mut cursor = 16usize;
    let mut packet_idx = 0usize;
    while cursor < stream.len() {
        if stream.len().saturating_sub(cursor) < 20 {
            return Err("truncated audio packet header".to_string());
        }
        if stream[cursor..cursor + 2] != AUDIO_SYNC {
            return Err(format!("invalid audio packet sync at offset {cursor}"));
        }
        let meta = parse_audio_frame_packet_header(&stream[cursor..cursor + 20])
            .map_err(|e| format!("failed parsing audio packet header: {e}"))?;
        let payload_start = cursor + 20;
        let payload_end = payload_start + meta.payload_len as usize;
        if payload_end > stream.len() {
            return Err("truncated audio packet payload".to_string());
        }
        println!(
            "packet[{packet_idx}] frame_index={} channels={} samples_per_channel={} payload_len={} crc32=0x{:08x}",
            meta.frame_index, meta.channels, meta.sample_count_per_channel, meta.payload_len, meta.crc32
        );
        packet_idx += 1;
        cursor = payload_end;
    }
    Ok(())
}

fn frame_type_name(frame_type: FrameType) -> &'static str {
    match frame_type {
        FrameType::I => "I",
        FrameType::PReserved => "P(reserved)",
    }
}
