//! Smoke: `srs_cli play` decodes SRSV2 intra + experimental P (`.528`).

use std::fs::File;
use std::process::Command;

use libsrs_container::{FileHeader, TrackDescriptor, TrackKind};
use libsrs_mux::MuxWriter;
use libsrs_video::{
    decode_yuv420_srsv2_payload, encode_sequence_header_v2, encode_yuv420_inter_payload,
    encode_yuv420_intra_payload, gray8_packed_to_yuv420p8_neutral, ResidualEntropy,
    SrsV2EncodeSettings, VideoSequenceHeaderV2, SEQUENCE_HEADER_BYTES,
};

fn write_temp_config() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("srs-cli-play-srsv2-{}.toml", std::process::id()));
    std::fs::write(
        &path,
        r#"[client]
primary_url = "http://localhost:3000"
backup_url = "http://127.0.0.1:3000"
license_key = ""

[server]
bind_addr = "127.0.0.1:3000"
base_url = "http://localhost:3000"
database_path = "var/test.sqlite3"
"#,
    )
    .expect("write temp config");
    path
}

fn write_srsv2_ip_528(path: &std::path::Path) {
    let w = 16u32;
    let h = 16u32;
    let seq = VideoSequenceHeaderV2::intra_main_yuv420_bt709_limited_one_ref(w, h);
    let gray0 = vec![0x22u8; (w * h) as usize];
    let gray1 = vec![0xEEu8; (w * h) as usize];
    let yuv0 = gray8_packed_to_yuv420p8_neutral(&gray0, w, h).unwrap();
    let yuv1 = gray8_packed_to_yuv420p8_neutral(&gray1, w, h).unwrap();
    let st = SrsV2EncodeSettings {
        residual_entropy: ResidualEntropy::Explicit,
        ..Default::default()
    };
    let enc0 = encode_yuv420_intra_payload(&seq, &yuv0, 0, 28, &st, None, None).unwrap();
    let mut slot = None;
    decode_yuv420_srsv2_payload(&seq, &enc0, &mut slot).unwrap();
    let enc1 =
        encode_yuv420_inter_payload(&seq, &yuv1, slot.as_ref(), 1, 28, &st, None, None, None)
            .unwrap();
    assert_eq!(enc1[3], 2);
    let cfg = encode_sequence_header_v2(&seq).to_vec();
    assert_eq!(cfg.len(), SEQUENCE_HEADER_BYTES);
    let tracks = vec![TrackDescriptor {
        track_id: 1,
        kind: TrackKind::Video,
        codec_id: 3,
        flags: 0,
        timescale: 90_000,
        config: cfg,
    }];
    let f = File::create(path).unwrap();
    let mut mux = MuxWriter::new(f, FileHeader::new(1, 4), tracks).unwrap();
    mux.write_packet(1, 0, 0, true, &enc0).unwrap();
    mux.write_packet(1, 3000, 3000, false, &enc1).unwrap();
    mux.finalize().unwrap();
}

#[test]
fn cli_play_smoke_decodes_srsv2_intra_and_predicted() {
    let config = write_temp_config();
    let sample = std::env::temp_dir().join(format!("srs-cli-play-ip-{}.528", std::process::id()));
    write_srsv2_ip_528(&sample);

    let output = Command::new(env!("CARGO_BIN_EXE_srs_cli"))
        .env("SRS_CONFIG_PATH", &config)
        .args(["play", "--frames", "4", "--no-audio", "--decode-only"])
        .arg(&sample)
        .output()
        .expect("run play");

    let _ = std::fs::remove_file(&config);
    let _ = std::fs::remove_file(&sample);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
