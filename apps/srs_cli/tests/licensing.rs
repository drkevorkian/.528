use std::fs;
use std::process::Command;

fn write_test_config(name: &str) -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("srs-cli-test-{name}-{}.toml", std::process::id()));
    fs::write(
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

#[test]
fn analyze_remains_available_without_license_key() {
    use libsrs_container::{FileHeader, TrackDescriptor, TrackKind};
    use libsrs_mux::MuxWriter;
    use libsrs_video::{encode_frame, FrameType, VideoFrame};

    let config = write_test_config("analyze");
    let sample = std::env::temp_dir().join(format!("srs-cli-analyze-{}.528", std::process::id()));
    let w = 16u32;
    let video = VideoFrame {
        width: w,
        height: w,
        frame_index: 0,
        frame_type: FrameType::I,
        data: vec![0x42; (w * w) as usize],
    };
    let enc = encode_frame(&video).expect("encode");
    let tracks = vec![TrackDescriptor {
        track_id: 1,
        kind: TrackKind::Video,
        codec_id: 1,
        flags: 0,
        timescale: 90_000,
        config: [w.to_le_bytes(), w.to_le_bytes()].concat(),
    }];
    let file = fs::File::create(&sample).expect("temp 528");
    let mut mux = MuxWriter::new(file, FileHeader::new(1, 4), tracks).expect("mux");
    mux.write_packet(1, 0, 0, true, &enc).expect("pkt");
    mux.finalize().expect("fin");

    let output = Command::new(env!("CARGO_BIN_EXE_srs_cli"))
        .env("SRS_CONFIG_PATH", &config)
        .arg("analyze")
        .arg(&sample)
        .output()
        .expect("run analyze");
    let _ = fs::remove_file(&config);
    let _ = fs::remove_file(&sample);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("528-container") || stdout.contains("container"),
        "unexpected analyze output: {stdout}"
    );
}

#[test]
fn encode_is_rejected_without_verified_editor_key() {
    let config = write_test_config("encode");
    let output = Command::new(env!("CARGO_BIN_EXE_srs_cli"))
        .env("SRS_CONFIG_PATH", &config)
        .arg("encode")
        .arg("input.raw")
        .arg("output.srsv")
        .output()
        .expect("run encode");
    let _ = fs::remove_file(&config);

    assert!(
        !output.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No license key configured") || stderr.contains("play-only mode"),
        "unexpected stderr: {stderr}"
    );
}
