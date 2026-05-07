//! Helpers for interpreting SRSV2 frame payload tags (`FR2` revision byte).

use super::error::SrsV2Error;

/// Kind of SRSV2 mux packet payload (after `FR2` magic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Srsv2PayloadKind {
    /// `FR2\\x01` — intra / refresh picture (should be indexed as a keyframe).
    Intra,
    /// `FR2\\x02` … `FR2\\x09` — forward/inter predicted; experimental **B** / extended inter revisions (**10**–**11**, **13**–**28** except **12**) are classified here as **predicted** for mux/index policy (not keyframes) even when logical frame type is **B** — see `docs/video_bitstream_v2.md` for decode support per revision.
    Predicted,
    /// `FR2\\x0C` — experimental non-displayable alt-ref (`FR2` rev **12**).
    AltRef,
    /// `FR2` with an unsupported revision byte (must not be muxed without decoder support).
    Unknown,
}

/// Classify a mux/elementary SRSV2 frame payload by its `FR2` revision.
///
/// - `FR2\\x01` / `FR2\\x03` / `FR2\\x07` → [`Srsv2PayloadKind::Intra`] (rev 3/7 use entropy residuals; rev 7 adds block `qp_delta`)
/// - Forward/inter and experimental **B** revisions (**2**, **4**–**11**, **13**–**28**) → [`Srsv2PayloadKind::Predicted`] for mux/index policy (includes **P** rev **19**/**20**/**23**/**25**/**27**/**28**, **B** rev **16**/**18**/**24**, and reserved **B** rev **21**/**22**/**26** — **decode** may still return `Unsupported` for some of these; see `docs/video_bitstream_v2.md`)
/// - `FR2\\x0C` → [`Srsv2PayloadKind::AltRef`]
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
        1 | 3 | 7 => Srsv2PayloadKind::Intra,
        2 | 4 | 5 | 6 | 8 | 9 | 10 | 11 | 13 | 14 | 15 | 16 | 17 | 18 | 19 | 20 | 21 | 22 | 23
        | 24 | 25 | 26 | 27 | 28 => Srsv2PayloadKind::Predicted,
        12 => Srsv2PayloadKind::AltRef,
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

    #[test]
    fn fr2_rev5_rev6_are_predicted() {
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 5]).unwrap(),
            Srsv2PayloadKind::Predicted
        );
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 6]).unwrap(),
            Srsv2PayloadKind::Predicted
        );
    }

    #[test]
    fn fr2_rev7_is_intra() {
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 7]).unwrap(),
            Srsv2PayloadKind::Intra
        );
    }

    #[test]
    fn fr2_rev8_rev9_are_predicted() {
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 8]).unwrap(),
            Srsv2PayloadKind::Predicted
        );
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 9]).unwrap(),
            Srsv2PayloadKind::Predicted
        );
    }

    #[test]
    fn fr2_rev10_rev11_are_predicted_b_syntax() {
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 10]).unwrap(),
            Srsv2PayloadKind::Predicted
        );
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 11]).unwrap(),
            Srsv2PayloadKind::Predicted
        );
    }

    #[test]
    fn fr2_rev12_is_alt_ref() {
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 12]).unwrap(),
            Srsv2PayloadKind::AltRef
        );
    }

    #[test]
    fn fr2_rev14_is_predicted_b_syntax() {
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 14]).unwrap(),
            Srsv2PayloadKind::Predicted
        );
    }

    #[test]
    fn fr2_rev15_rev17_compact_entropy_p_are_predicted() {
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 15]).unwrap(),
            Srsv2PayloadKind::Predicted
        );
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 17]).unwrap(),
            Srsv2PayloadKind::Predicted
        );
    }

    #[test]
    fn fr2_rev19_through_22_are_predicted_for_mux_policy() {
        for rev in [19u8, 20, 21, 22] {
            assert_eq!(
                classify_srsv2_payload(&[b'F', b'R', b'2', rev]).unwrap(),
                Srsv2PayloadKind::Predicted
            );
        }
    }

    #[test]
    fn fr2_rev27_rev28_are_predicted_for_mux_policy() {
        for rev in [27u8, 28] {
            assert_eq!(
                classify_srsv2_payload(&[b'F', b'R', b'2', rev]).unwrap(),
                Srsv2PayloadKind::Predicted
            );
        }
    }

    #[test]
    fn fr2_rev23_through_26_are_predicted_for_mux_policy() {
        for rev in [23u8, 24, 25, 26] {
            assert_eq!(
                classify_srsv2_payload(&[b'F', b'R', b'2', rev]).unwrap(),
                Srsv2PayloadKind::Predicted
            );
        }
    }

    #[test]
    fn fr2_rev16_rev18_compact_entropy_b_are_predicted() {
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 16]).unwrap(),
            Srsv2PayloadKind::Predicted
        );
        assert_eq!(
            classify_srsv2_payload(&[b'F', b'R', b'2', 18]).unwrap(),
            Srsv2PayloadKind::Predicted
        );
    }
}
