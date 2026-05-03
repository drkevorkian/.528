//! Guardrails: README benchmark examples stay aligned with current CLI flags.

use std::fs;
use std::path::PathBuf;

#[test]
fn readme_uses_current_synthetic_and_bench_flags() {
    let readme_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../README.md");
    let s = fs::read_to_string(&readme_path).expect("read README.md");

    assert!(
        !s.contains("--out-yuv"),
        "README should use --out for gen_synthetic_yuv, not --out-yuv"
    );
    assert!(
        !s.contains("--out-meta"),
        "README should use --meta for gen_synthetic_yuv metadata"
    );
    assert!(s.contains("--out "), "expected --out in benchmark snippet");
    assert!(
        s.contains("--meta "),
        "expected --meta in benchmark snippet"
    );
    assert!(
        s.contains("bench_srsv2"),
        "README should document bench_srsv2 as primary benchmark"
    );
    assert!(
        s.contains("--residual-entropy"),
        "README should mention residual entropy flag"
    );
    assert!(
        s.contains("--compare-partitions"),
        "README should mention partition comparison flag"
    );
}
