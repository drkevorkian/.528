//! Canonical **native** `.528` track `codec_id` values (SRS family).
//!
//! These are **container** identifiers (`TrackDescriptor::codec_id`), not the same Rust type as
//! [`libsrs_video::srsv2::SrsElementaryVideoCodecId`] (logical elementary tags — see that crate).

/// Legacy SRSV1 video (`TrackKind::Video`).
pub const CONTAINER_VIDEO_CODEC_SRSV1: u16 = 1;
/// SRSA audio (`TrackKind::Audio`).
pub const CONTAINER_AUDIO_CODEC_SRSA: u16 = 2;
/// SRSV2 video (`TrackKind::Video`).
pub const CONTAINER_VIDEO_CODEC_SRSV2: u16 = 3;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_mux_codec_ids_do_not_collide() {
        assert_eq!(CONTAINER_VIDEO_CODEC_SRSV1, 1);
        assert_eq!(CONTAINER_AUDIO_CODEC_SRSA, 2);
        assert_eq!(CONTAINER_VIDEO_CODEC_SRSV2, 3);
        assert_ne!(CONTAINER_VIDEO_CODEC_SRSV2, CONTAINER_AUDIO_CODEC_SRSA);
    }
}
