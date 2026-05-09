# Platform readiness checklist (SRSV2 / `.528`)

Use this checklist with [`platform_adoption_plan.md`](platform_adoption_plan.md).

**Definitions**

- **Mainstream-ready**: satisfies **all** sections marked **Gate** below at the same milestone tag.
- **Codec track**: SRSV2 correctness, revisions (`FR2`), compression work—see plan §“Two intentional tracks.”
- **Adoption track**: integrations, packaging, operational artifacts—does **not** substitute for codec gates.

**Claims discipline**

- Do **not** claim platform adoption, royalty-free status, patent freedom, or browser codec support unless the corresponding checkbox has evidence attached (link, tag, counsel memo ID)—see Honesty row in §Meta.

---

## Meta — governance & honesty

| # | Item | Status | Evidence / link |
|---|------|--------|-----------------|
| M1 | “Mainstream-ready” used only when **all Gate rows** below are complete | ☐ | |
| M2 | Public messaging avoids implying **FFmpeg/GStreamer/browser/OS** shipping SRS without upstream/vendor artifacts | ☐ | |
| M3 | No “royalty-free” / “patent-free” language without **dated legal review** reference | ☐ | Counsel memo ID: ___ |
| M4 | Codec vs adoption tracks called out in release notes for major milestones | ☐ | |

---

## 1. Stable bitstream spec freeze process — **Gate**

| # | Item | Status | Notes |
|---|------|--------|-------|
| 1.1 | Profile ladder defined (Draft / Candidate / Frozen) | ☐ | |
| 1.2 | At least one **Frozen** profile ID assigned | ☐ | ID: ___ |
| 1.3 | Frozen profile pins allowed `FR2` revisions & features | ☐ | |
| 1.4 | Normative spec bundle **version-tagged** (docs release artifact) | ☐ | Tag: ___ |
| 1.5 | Errata process documented next to freeze tag | ☐ | |
| 1.6 | Machine-readable profile manifest (schema + JSON example) | ☐ | |

---

## 2. Reference encoder / decoder crates — **Gate**

| # | Item | Status | Notes |
|---|------|--------|-------|
| 2.1 | Reference **decoder** crate/repo revision tagged | ☐ | Tag: ___ |
| 2.2 | Reference **encoder** emits **only** Frozen profile | ☐ | |
| 2.3 | Decoder error taxonomy documented (`Unsupported` vs `Malformed` vs resource limits) | ☐ | |
| 2.4 | C ABI or stable FFI boundary documented (for integrations) | ☐ | |
| 2.5 | CI builds reference encoder+decoder on tier-1 platforms | ☐ | |

---

## 3. Conformance corpus — **Gate**

| # | Item | Status | Notes |
|---|------|--------|-------|
| 3.1 | **Legal** golden streams tarball published with checksums | ☐ | SHA256: ___ |
| 3.2 | **Illegal** streams with expected outcomes matrix | ☐ | |
| 3.3 | Corpus indexed by profile ID | ☐ | |
| 3.4 | Nightly CI runs decoder on full corpus | ☐ | |
| 3.5 | Regression policy for corpus changes (review + tag bump) | ☐ | |

---

## 4. Fuzzing & hostile-file security — **Gate**

| # | Item | Status | Notes |
|---|------|--------|-------|
| 4.1 | Structured fuzz targets listed (demux, seq hdr, packet, FR2 payload) | ☐ | |
| 4.2 | Differential fuzz vs reference decoder (where applicable) | ☐ | |
| 4.3 | Resource bounds documented (alloc caps, reorder depth) | ☐ | |
| 4.4 | Critical/high findings addressed or explicitly risk-accepted | ☐ | |
| 4.5 | External audit **scheduled or completed** (optional but noted) | ☐ | |

---

## 5. FFmpeg demux/decode prototype

| # | Item | Status | Notes |
|---|------|--------|-------|
| 5.1 | Out-of-tree branch or repo published | ☐ | URL: ___ |
| 5.2 | Decodes **Frozen** profile conformance **legal** set | ☐ | |
| 5.3 | Pixel format & range documented | ☐ | |
| 5.4 | CI or scripted smoke: `ffmpeg …` on subset | ☐ | |
| 5.5 | Upstream merge **not** claimed unless MR/issue exists | ☐ | |

---

## 6. GStreamer plugin prototype

| # | Item | Status | Notes |
|---|------|--------|-------|
| 6.1 | Plugin repo/branch published | ☐ | URL: ___ |
| 6.2 | `gst-launch` example for corpus clip | ☐ | |
| 6.3 | Caps negotiation matches FFmpeg contract | ☐ | |

---

## 7. Browser / WASM decoder prototype

| # | Item | Status | Notes |
|---|------|--------|-------|
| 7.1 | WASM build reproducible (pinned toolchain) | ☐ | |
| 7.2 | Demo page **labeled experimental** | ☐ | URL: ___ |
| 7.3 | Memory hostile-input limits documented | ☐ | |
| 7.4 | **No** claim of native `<video>` codec without vendor registration proof | ☐ | N/A unless proven |

---

## 8. Streaming segment format — **Gate**

| # | Item | Status | Notes |
|---|------|--------|-------|
| 8.1 | Init segment fields normative (profile, timescale, codec init) | ☐ | Doc: ___ |
| 8.2 | Media segment framing (duration bounds, mux alignment) | ☐ | |
| 8.3 | Random access point (RAP) definition tied to intra/open-GOP rules | ☐ | |
| 8.4 | Keyframe / SAP index format (internal or sidecar) | ☐ | |
| 8.5 | Seek byte-range recipe reproducible | ☐ | |

---

## 9. CDN / platform operations

| # | Item | Status | Notes |
|---|------|--------|-------|
| 9.1 | Bitrate ladder table for Frozen profile | ☐ | |
| 9.2 | Adaptive manifest example (HLS/DASH-style **compatibility** doc only) | ☐ | |
| 9.3 | Thumbnail policy (SAP-aligned extraction) | ☐ | |
| 9.4 | Static metadata schema | ☐ | |
| 9.5 | Captions/subtitles carriage (out-of-band until in-band specified) | ☐ | |

---

## 10. Hardware path

| # | Item | Status | Notes |
|---|------|--------|-------|
| 10.1 | GPU-accelerated **software** decode path plan (SIMD/compute) | ☐ | |
| 10.2 | Hardware vendor spec tracked separately (NDA/partner) | ☐ | |
| 10.3 | No public claim of ASIC decode until SKU/program documented | ☐ | |

---

## 11. Licensing / patent / royalty posture — **Gate (legal)**

| # | Item | Status | Notes |
|---|------|--------|-------|
| 11.1 | Repository LICENSE accurate for shipped artifacts | ☐ | |
| 11.2 | Third-party notices complete | ☐ | |
| 11.3 | Patent / royalty positioning reviewed by **outside counsel** | ☐ | Memo: ___ |
| 11.4 | Public-facing royalty statement approved by counsel | ☐ | |

---

## 12. Performance targets

| # | Item | Status | Notes |
|---|------|--------|-------|
| 12.1 | Reference machine SKU documented | ☐ | |
| 12.2 | **1080p60** software decode real-time report | ☐ | |
| 12.3 | **4K60** desktop decode throughput report | ☐ | |
| 12.4 | **8K** offline encode feasibility report | ☐ | |
| 12.5 | Methodology matches repo benchmark discipline | ☐ | |

---

## 13. Final acceptance — **Mainstream candidate**

| # | Item | Status |
|---|------|--------|
| G1 | §1 Gate complete | ☐ |
| G2 | §2 Gate complete | ☐ |
| G3 | §3 Gate complete | ☐ |
| G4 | §4 Gate complete | ☐ |
| G5 | §5 **or** §6 integration prototype operational on corpus | ☐ |
| G6 | §8 Gate complete | ☐ |
| G7 | §9 operational artifacts published (internal draft OK with tag) | ☐ |
| G8 | §11 Gate complete **with counsel** | ☐ |
| G9 | §12 reports attached to milestone | ☐ |

**Signed readiness** (when all checked): owner _________________ date _________

---

## Quick “not yet mainstream-ready” triggers

If **any** of the following is true, say **not mainstream-ready** publicly:

- Frozen profile unset or changing weekly without errata.
- No published conformance corpus checksum.
- FFmpeg/GStreamer path exists only as private hack without corpus CI.
- Legal/Patent row unchecked.
