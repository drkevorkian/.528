use std::fs;
use std::process::Command;

fn write_test_config(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("srs-cli-test-{name}-{}.toml", std::process::id()));
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
    let config = write_test_config("analyze");
    let output = Command::new(env!("CARGO_BIN_EXE_srs_cli"))
        .env("SRS_CONFIG_PATH", &config)
        .arg("analyze")
        .arg("sample.foreign")
        .output()
        .expect("run analyze");
    let _ = fs::remove_file(&config);

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("format: foreign") || stdout.contains("foreign format"));
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

    assert!(!output.status.success(), "stdout: {}", String::from_utf8_lossy(&output.stdout));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No license key configured") || stderr.contains("play-only mode"),
        "unexpected stderr: {stderr}"
    );
}
