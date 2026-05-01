//! Helpers for interpreting SRSV2 frame payload tags (`FR2` revision byte).

use super::error::SrsV2Error;

/// Kind of SRSV2 mux packet payload (after `FR2` magic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Srsv2PayloadKind {
    /// `FR2\\x01` — intra / refresh picture (should be indexed as a keyframe).
    Intra,
    /// `FR2\\x02` — experimental predicted picture (not a keyframe).
    Predicted,
    /// `FR2` with an unsupported revision byte (must not be muxed without decoder support).
    Unknown,
}

/// Classify a mux/elementary SRSV2 frame payload by its `FR2` revision.
///
/// - `FR2\\x01` / `FR2\\x03` → [`Srsv2PayloadKind::Intra`] (rev 3 uses entropy residuals)
/// - `FR2\\x02` / `FR2\\x04` → [`Srsv2PayloadKind::Predicted`]
/// - Other `FR2\\x??` → [`Srsv2PayloadKind::Unknown`]
/// - Too short or bad magic → [`SrsV2Error`]
pub fn classify_srsv2_payload(payload: &[u8]) -> Result<Srsv2PayloadKind, SrsV2Error> {
    if payload.len() < 4 {
        return Err(SrsV2Error::syntax("SRSV2 payload too short for FR2 header"));
    }
    if &payload[0..3] != b"FR2" {
        return Err(SrsV2Error::BadMagic);
    }
    Ok(match payload[3] {
        1 | 3 => Srsv2PayloadKind::Intra,
        2 | 4 => Srsv2PayloadKind::Predicted,
        _ => Srsv2PayloadKind::Unknown,
    })
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    #[test]
    fn fr2_rev1_is_intra() {
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 1, 0]).unwrap(),
            Srsv2PayloadKind::Intra
        );
    }

    #[test]
    fn fr2_rev2_is_predicted() {
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 2]).unwrap(),
            Srsv2PayloadKind::Predicted
        );
    }

    #[test]
    fn too_short_errors() {
        assert!(classify_srsv2_payload(&[1, 2]).is_err());
    }

    #[test]
    fn bad_magic_errors() {
        assert!(classify_srsv2_payload(b"XX2\x01").is_err());
    }

    #[test]
    fn unknown_revision_ok_unknown() {
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 99]).unwrap(),
            Srsv2PayloadKind::Unknown
        );
    }

    #[test]
    fn fr2_rev3_is_intra() {
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 3]).unwrap(),
            Srsv2PayloadKind::Intra
        );
    }

    #[test]
    fn fr2_rev4_is_predicted() {
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 4]).unwrap(),
            Srsv2PayloadKind::Predicted
        );
    }
}
