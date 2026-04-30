//! Helpers for the `codec_compare` binary (PSNR / FFmpeg probe).
//!
//! Normal `cargo test` does **not** require FFmpeg; probe functions exist for optional baselines.

use std::process::Command;

/// Returns `true` if `ffmpeg` on `PATH` runs `ffmpeg -version` successfully.
pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_probe_does_not_panic() {
        let _ = ffmpeg_available();
    }
}
