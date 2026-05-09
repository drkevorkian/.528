# Platform adoption plan (SRSV2 / `.528`)

This document describes **what would need to be true** for the SRS ecosystem—including SRSV2 video inside `.528` containers—to be treated as a **platform-mainstream candidate**: integrated with common tools, deployed behind CDNs, and trusted for interoperability and security.

It does **not** assert that any of this exists today, that adoption is imminent, or that third-party platforms will ship native support.

---

## How to read this plan

### Two intentional tracks

| Track | Purpose | Owned outcomes |
|--------|---------|------------------|
| **Codec & container engineering** | Correctness, compression, hostile-input safety, revisions (`FR2`), mux timing | Reference codecs, spec text, conformance, fuzzers |
| **Platform adoption** | Discovery in FFmpeg/GStreamer/browsers, packaging for streaming, operational knobs | Plugins, packaging specs, ladder/metadata conventions, partner integrations |

These tracks overlap only at **stable ABI/bitstream boundaries** and **documented segment/container profiles**. Codec iteration must not silently redefine profiles once labeled **frozen** for a mainstream milestone.

### “Mainstream-ready” (definition)

For this workspace, **mainstream-ready** means meeting **all** of the following **gates** (see §13 and [`platform_readiness_checklist.md`](platform_readiness_checklist.md)):

1. A **frozen** baseline profile (or small closed set of profiles) with a numbered **spec + conformance** story.
2. **Reference encoder and decoder** that implement that profile and power conformance tooling.
3. **Conformance corpus + differential fuzzing** with documented security posture—not “best effort.”
4. At least one **credible integration prototype** (e.g. FFmpeg demux/decode **path**) demonstrating realistic ingestion—not a demo that asserts ecosystem acceptance.
5. **Streaming packaging** defined enough that CDNs and players could ingest **without** reverse-engineering the repo (init/media segments, RAP rules).
6. **Performance envelopes** documented as targets with reproducible measurement—not claims of superiority vs AVC/HEVC.
7. **Legal/licensing posture** written down with explicit reliance on counsel—not inventor optimism.

Until those gates pass, treat SRSV2 as **research/engineering**, not as a substitute for shipping codecs with established platform support.

---

## 1. Stable bitstream spec freeze process

**Goal:** Platform partners need **immutable targets**: versioned specs, explicit profiles, and a change process that does not break silent compatibility.

**Recommended process**

1. **Profile ladder**  
   - **Draft** → **Candidate** → **Frozen** profile labels.  
   - Each profile pins: allowed `FR2` revisions, max resolution/bit-depth, GOP/B constraints (if any), and container mapping (elementary vs `.528`).

2. **Normative documents**  
   - Single **normative** bitstream document per freeze (today’s split across [`video_bitstream_v2.md`](video_bitstream_v2.md), [`528_container_format.md`](528_container_format.md), [`specs/packet_layout.md`](specs/packet_layout.md) should converge into versioned releases).  
   - **Errata** list published beside each freeze tag.

3. **Reference behavior**  
   - Decoder acceptance tests define **must decode**, **must reject**, **optional**.  
   - Encoder conformance declares **must emit within profile**.

4. **Change control**  
   - Backward-compatible extensions require **new profile revision** or **new elementary codec revision**, never silent widening of Frozen profiles.

**Deliverables**

- Versioned spec tarball or tagged docs bundle per milestone.
- Machine-readable **capabilities manifest** (JSON/schema): profile ID → revision matrix.

---

## 2. Reference encoder / decoder crates

**Goal:** Interoperability is judged against **artifacts**, not intentions.

**Direction**

| Crate role | Responsibility |
|------------|----------------|
| **Reference decoder** | Exhaustive rejection paths for malformed streams; explicit error taxonomy; zero UB under fuzz vectors aligned with [`libsrs_video`](../crates/libsrs_video) SRSV2 path |
| **Reference encoder** | Emits **only** declared profiles; emits conformance golden vectors; rate-control/API separated from **normative** bit layout |

**Non-goals for reference milestones**

- “Fastest encoder on Earth”—optimization belongs **after** correctness gates.

**Integration hooks**

- C ABI boundary plan for FFmpeg/GStreamer (thin FFI surface; bounded allocations documented).

---

## 3. Conformance corpus

**Goal:** Third parties reproduce behavior **without** cloning informal fixtures.

**Corpus contents**

- **Legal streams**: minimal intra-only; typical IP… sequences permitted by Frozen profile; B‑structures **only if** in Frozen profile; optional HDR/bit-depth ladders **only after** profile says so.
- **Illegal streams**: truncated packets; bogus lengths; revision/feature mismatches; overlapping timestamps—each tied to **expected** decoder outcomes (`Rejected`, `Unsupported`, deterministic drain behavior).

**Governance**

- Corpus tagged alongside **spec freeze** tags.
- Checksums published; CI runs decoder on full corpus nightly.

---

## 4. Fuzzing and hostile-file security

**Goal:** Demux + decode paths must assume **malicious inputs** (see also [`adr/ADR-0003-bitstream-goals.md`](adr/ADR-0003-bitstream-goals.md) philosophy).

**Program**

1. **Structured fuzzing** on parsing boundaries (sequence headers, packet headers, `FR2` payloads).
2. **Differential fuzzing**: decoder must match reference decoder outputs **or** agree-class reject where invalid.
3. **Bounded resources**: documented caps on allocations, reference counts, and reorder buffers (`PlaybackSession`-style bounds as precedent).

**Artifacts**

- Fuzz dictionaries seeded from corpus.  
- Public summary page: bugs fixed vs embargo policy—without implying formal certification unless obtained.

**Claims discipline**

- Do **not** claim “production-ready security” without independent audit milestones documented on the checklist.

---

## 5. FFmpeg demux / decode prototype plan

**Goal:** Prove **real ingest**, not benchmark isolation—without claiming FFmpeg upstream merged native SRS support.

**Phases**

1. **Out-of-tree prototype**: `.528` demuxer reading cues/index (reuse concepts from [`libsrs_demux`](../crates/libsrs_demux)); SRSV2 via Rust decoder linked as shared library or WASM bridge—architecture choice documented.
2. **Pixel/format contract**: Explicit pixel format (e.g. YUV420p8 limited range first); colorspace metadata passthrough rules.
3. **CLI parity**: `ffmpeg -i file.528 -f null -` reproducible on conformance corpus.
4. **Upstream conversation** (optional future): only **after** Frozen profile + corpus + security narrative exist.

**Honesty**

- FFmpeg integration is a **prototype milestone**, not proof of industry adoption.

---

## 6. GStreamer plugin prototype plan

**Goal:** Pipeline-friendly decode path for desktop/media servers—adjacent to FFmpeg track.

**Components**

- **demux** element for `.528` (or **parser** if elementary SRSV2).
- **decoder** element wrapping reference decoder C ABI.
- Caps negotiation matching FFmpeg pixel contract.

**Milestone**

- `gst-launch-1.0` playback of conformance streams under documented profiles.

---

## 7. Browser / WASM decoder prototype plan

**Goal:** Explore **technical feasibility** of client-side decode **without** claiming browser vendors will ship or approve SRS.

**Constraints**

- WASM bundle size budget; threading model (SIMD); memory caps for **hostile** streams.
- **No claim** of native `<video>` codec registration unless such registration exists and is documented.

**Deliverables**

- Demo page loading WASM decoder + Canvas/WebGL upload—clearly labeled **experimental**.
- Performance measured vs targets in §12—not vs hardware AVC/HEVC decode unless methodology is identical.

---

## 8. Streaming segment format

**Goal:** CDN-friendly packaging independent of “single local file” assumptions.

**Conceptual model** (names illustrative until normative doc exists):

| Artifact | Purpose |
|----------|---------|
| **Init segment** | Codec initialization: sequence header(s), encryption/default keys if used, profile declaration, timescale |
| **Media segment** | Chunk of muxed packets with **decode timestamps**, duration bounds, **SAP** markers |
| **Random access points (RAP)** | Keyframes / clean refresh points—ties to SRSV2 **intra** or documented **open‑GOP** rules **only if** profile allows |
| **Keyframe index** | Sidecar or internal cue map for HTTP Range / LL‑HLS‑style seeks |

**Requirements**

- Byte-range rules **deterministic** for seeks.
- Version byte or profile ID in init segment **must** match decoder capability manifest.

**Relationship to `.528`**

- Either **native fMP4-like** mapping **or** `.528` fragments with documented MOOF analog—pick one normative story per freeze; avoid dual meanings.

---

## 9. CDN / platform requirements

**Goal:** Operators need predictable artifacts—not “encoder defaults.”

**Bitrate ladder**

- Presets table: resolution rungs, peak bitrate caps, keyframe interval policy; alignment with §8 SAP/index rules.

**Adaptive streaming**

- Manifest schema (HLS/DASH-style **compatibility** discussed at packaging layer—do **not** imply standardized SRS MIME until registered).

**Thumbnails**

- Policy for sprite sheets vs I-frame extraction points (SAP-aligned).

**Metadata**

- Static metadata (title, language) vs timed metadata—mapping into container **or** sidecar JSON schema.

**Captions / subtitles**

- Out-of-band WebVTT / TTML carriage rules; **no** claim of in-band caption codec until specified.

---

## 10. Hardware path

**Goal:** Order work without pretending silicon exists.

**Near term**

- **GPU-assisted software decode**: SIMD + compute shaders for MC/residual hot paths—still **software-defined** normative behavior.

**Later**

- **Hardware vendor spec**: separate confidential/partner track; public freeze profile remains implementable **without** ASIC.

**Claims discipline**

- Do **not** claim hardware decode availability until silicon/vendor programs are documented under NDA or public SKUs.

---

## 11. Licensing / patent / royalty posture

**Goal:** Platforms adopt **business-clear** codecs.

**Workspace stance**

- Publish **license** for reference code (repository LICENSE).  
- Maintain **third-party notices** for dependencies.

**Patents / royalties**

- **Do not** state “royalty-free,” “patent-free,” or “safe for all uses” **without** dated legal memoranda and counsel sign-off referenced in [`platform_readiness_checklist.md`](platform_readiness_checklist.md).

**Outbound**

- Contribution policy + patent grant clarity if targeting foundation-hosted standards.

---

## 12. Performance targets

**Goal:** Quantify **engineering expectations**—not marketing superiority.

Targets are **minimum directional goals** for a Frozen **Mainstream Candidate Profile** on a **reference-class** desktop CPU (specific SKU listed per benchmark report):

| Scenario | Target |
|----------|--------|
| **1080p60 software decode** | Sustained real-time decode of conformance ladder clip **without** frame drops under documented buffer policy |
| **4K60 desktop decode** | Measurable throughput envelope (fps + CPU %) published with methodology |
| **8K offline encode** | Completes without failure on reference machine; wall-clock vs quality metric tables published **without** claiming broadcast readiness |

**Measurement**

- Same methodology as [`srsv2_benchmarks.md`](srsv2_benchmarks.md) extended to segment playback—not CRF-only snapshots alone.

---

## 13. Acceptance gates for “mainstream candidate”

All must be satisfied **together**:

1. **Frozen profile + versioned spec bundle** published.
2. **Reference encoder & decoder** tagged; decode corpus **100%** classified pass/reject per golden sheet.
3. **Fuzz campaign** results + fixed critical issues documented (severity taxonomy).
4. **Integration prototype**: FFmpeg **or** GStreamer path demonstrated on corpus **and** CI smoke job.
5. **Streaming packaging doc**: init/media segments + RAP + index **normative** for at least one profile.
6. **CDN ladder + manifest schema** draft adopted internally with examples.
7. **Security notes**: threat model + bounds; no unresolved **critical** decode-path issues.
8. **Legal checklist**: LICENSE + notices + **explicit** patent/royalty statement reviewed by counsel (**checkbox**, not self-attestation).
9. **Performance report** vs §12 targets.

Until then, describe the project as **not mainstream-ready**—even if compression metrics improve.

---

## Document maintenance

- Owner: engineering lead + product/legal stakeholders.
- Review cadence: quarterly or at each **Candidate → Frozen** transition.

Cross-references: [`platform_readiness_checklist.md`](platform_readiness_checklist.md), [`hevc_competition_plan.md`](hevc_competition_plan.md), [`playback_pipeline.md`](playback_pipeline.md), [`528_container_format.md`](528_container_format.md).
