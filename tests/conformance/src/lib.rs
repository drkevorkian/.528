#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use libsrs_audio::{AudioFrame, AudioStreamReader, AudioStreamWriter};
    use libsrs_video::{FrameType, VideoFrame, VideoStreamReader, VideoStreamWriter};

    #[test]
    fn video_roundtrip_is_deterministic() {
        let width = 16_u32;
        let height = 16_u32;
        let frame_a = VideoFrame {
            width,
            height,
            frame_index: 0,
            frame_type: FrameType::I,
            data: build_video_vector(width, height, 3),
        };
        let frame_b = VideoFrame {
            width,
            height,
            frame_index: 1,
            frame_type: FrameType::I,
            data: build_video_vector(width, height, 7),
        };

        let mut encoded_bytes = Vec::new();
        {
            let mut writer = VideoStreamWriter::new(&mut encoded_bytes, width, height)
                .expect("writer should initialize");
            let meta_a = writer.write_frame(&frame_a).expect("frame a should encode");
            let meta_b = writer.write_frame(&frame_b).expect("frame b should encode");
            assert!(meta_a.crc32 != 0);
            assert!(meta_b.crc32 != 0);
        }

        let mut reader =
            VideoStreamReader::new(Cursor::new(&encoded_bytes)).expect("reader should initialize");
        let decoded_a = reader
            .read_next_frame()
            .expect("frame a should decode")
            .expect("frame a should exist");
        let decoded_b = reader
            .read_next_frame()
            .expect("frame b should decode")
            .expect("frame b should exist");
        let end = reader.read_next_frame().expect("eof should decode");
        assert!(end.is_none());
        assert_eq!(decoded_a, frame_a);
        assert_eq!(decoded_b, frame_b);

        let mut encoded_bytes_second = Vec::new();
        {
            let mut writer = VideoStreamWriter::new(&mut encoded_bytes_second, width, height)
                .expect("writer should initialize");
            writer.write_frame(&frame_a).expect("frame a should encode");
            writer.write_frame(&frame_b).expect("frame b should encode");
        }
        assert_eq!(encoded_bytes, encoded_bytes_second);
    }

    #[test]
    fn audio_roundtrip_is_sample_perfect() {
        let sample_rate = 48_000_u32;
        let channels = 2_u8;
        let frame_a = AudioFrame {
            sample_rate,
            channels,
            frame_index: 0,
            samples: build_audio_vector(128, channels, 19),
        };
        let frame_b = AudioFrame {
            sample_rate,
            channels,
            frame_index: 1,
            samples: build_audio_vector(128, channels, 37),
        };

        let mut encoded_bytes = Vec::new();
        {
            let mut writer = AudioStreamWriter::new(&mut encoded_bytes, sample_rate, channels)
                .expect("audio writer should initialize");
            let meta_a = writer.write_frame(&frame_a).expect("frame a should encode");
            let meta_b = writer.write_frame(&frame_b).expect("frame b should encode");
            assert!(meta_a.crc32 != 0);
            assert!(meta_b.crc32 != 0);
        }

        let mut reader = AudioStreamReader::new(Cursor::new(&encoded_bytes))
            .expect("audio reader should initialize");
        let decoded_a = reader
            .read_next_frame()
            .expect("frame a should decode")
            .expect("frame a should exist");
        let decoded_b = reader
            .read_next_frame()
            .expect("frame b should decode")
            .expect("frame b should exist");
        let end = reader.read_next_frame().expect("eof should decode");
        assert!(end.is_none());
        assert_eq!(decoded_a, frame_a);
        assert_eq!(decoded_b, frame_b);

        let mut encoded_bytes_second = Vec::new();
        {
            let mut writer =
                AudioStreamWriter::new(&mut encoded_bytes_second, sample_rate, channels)
                    .expect("audio writer should initialize");
            writer.write_frame(&frame_a).expect("frame a should encode");
            writer.write_frame(&frame_b).expect("frame b should encode");
        }
        assert_eq!(encoded_bytes, encoded_bytes_second);
    }

    fn build_video_vector(width: u32, height: u32, phase: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            for x in 0..width {
                let value = (((x * 17 + y * 13) as u8).wrapping_add(phase)).wrapping_mul(3);
                out.push(value);
            }
        }
        out
    }

    fn build_audio_vector(sample_count_per_channel: usize, channels: u8, seed: i16) -> Vec<i16> {
        let mut out = Vec::with_capacity(sample_count_per_channel * channels as usize);
        let mut left = seed;
        let mut right = -seed;
        for i in 0..sample_count_per_channel {
            left = left.wrapping_add((i as i16 % 11) - 5);
            out.push(left);
            if channels == 2 {
                right = right.wrapping_sub((i as i16 % 7) - 3);
                out.push(right);
            }
        }
        out
    }
}
