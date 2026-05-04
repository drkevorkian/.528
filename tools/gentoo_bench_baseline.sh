#!/usr/bin/env bash
# Reproducible SRSV2 Gentoo baseline: synthetic corpus + bench_srsv2 compare modes.
# Does not install packages. Codec algorithms unchanged — benchmark harness only.
set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(CDPATH='' cd -- "${script_dir}/.." && pwd)"

ROOT="${repo_root}"
BASE="${ROOT}/var/bench/gentoo_baseline"
CORPUS="${BASE}/corpus"
RUNS="${BASE}/runs"
SUMMARY_MD="${BASE}/SUMMARY.md"
RESULTS_DOC="${ROOT}/docs/gentoo_baseline_results.md"

SEED="${GENTOO_BASELINE_SEED:-528}"
FPS=30
QP=28
KEYINT=30
MOTION=16

mkdir -p "${CORPUS}" "${RUNS}"

build_bench() {
	if [[ "${GENTOO_BASELINE_SKIP_BUILD:-0}" == "1" ]]; then
		return 0
	fi
	(
		cd "${ROOT}"
		cargo build --release -p quality_metrics --bin bench_srsv2
	)
}

bench_exe() {
	local p="${ROOT}/target/release/bench_srsv2"
	if [[ -x "${p}" ]]; then
		printf '%s\n' "${p}"
		return 0
	fi
	printf '%s\n' "${ROOT}/target/debug/bench_srsv2"
}

HAVE_FFMPEG=0
if command -v ffmpeg >/dev/null 2>&1; then
	HAVE_FFMPEG=1
fi

gen_clip() {
	local tag="$1"
	local pattern="$2"
	local w="$3"
	local h="$4"
	local frames="$5"
	local stem="gentoo_${tag}_${w}x${h}"
	local yuv="${CORPUS}/${stem}.yuv"
	local meta="${CORPUS}/${stem}.json"
	(
		cd "${ROOT}"
		cargo run -p quality_metrics --bin gen_synthetic_yuv -- \
			"--pattern=${pattern}" \
			"--width=${w}" \
			"--height=${h}" \
			"--frames=${frames}" \
			"--fps=${FPS}" \
			"--seed=${SEED}" \
			"--out=${yuv}" \
			"--meta=${meta}"
	) >/dev/null 2>&1
	printf '%s|%s|%s|%s|%s|%s\n' "${stem}" "${pattern}" "${w}" "${h}" "${frames}" "${yuv}"
}

run_bench() {
	local bench="$1"
	local yuv="$2"
	local w="$3"
	local h="$4"
	local frames="$5"
	local json_out="$6"
	local md_out="$7"
	shift 7
	mkdir -p "$(dirname "${json_out}")"
	"${bench}" \
		--input "${yuv}" \
		--width "${w}" \
		--height "${h}" \
		--frames "${frames}" \
		--fps "${FPS}" \
		--qp "${QP}" \
		--keyint "${KEYINT}" \
		--motion-radius "${MOTION}" \
		--report-json "${json_out}" \
		--report-md "${md_out}" \
		"$@"
}

append_summary_best_python() {
	local json_path="$1"
	local mode_tag="$2"
	python3 - "${json_path}" "${mode_tag}" <<'PY'
import json, sys

path, tag = sys.argv[1], sys.argv[2]

def srsv2_ok(row_obj):
    if not row_obj:
        return False
    c = row_obj.get("codec") or ""
    return "SRSV2" in c or c.startswith("SRSV2")

def best_compare_array(data, key):
    arr = data.get(key)
    if not isinstance(arr, list):
        return None
    cand = []
    for e in arr:
        if not e.get("ok"):
            continue
        row = e.get("row") or {}
        lab = row.get("codec") or e.get("label") or "?"
        if not srsv2_ok(row):
            continue
        b = row.get("bytes")
        if b is None:
            continue
        cand.append((b, lab))
    if not cand:
        return None
    cand.sort(key=lambda x: x[0])
    return cand[0]

def best_entropy(data):
    arr = data.get("compare_entropy_models")
    if not isinstance(arr, list):
        return None
    cand = []
    for e in arr:
        if not e.get("ok"):
            continue
        row = e.get("row") or {}
        mode = e.get("entropy_model_mode") or ""
        b = row.get("bytes")
        if b is None:
            continue
        cand.append((b, f"SRSV2 entropy {mode}"))
    if not cand:
        return None
    cand.sort(key=lambda x: x[0])
    return cand[0]

def best_sweep(data):
    rows = data.get("rows")
    if not isinstance(rows, list):
        return None
    okr = [r for r in rows if r.get("ok")]
    if not okr:
        return None
    best = min(okr, key=lambda r: r.get("total_bytes", 1 << 62))
    lab = f"qp={best.get('qp')} inter={best.get('inter_syntax')} part={best.get('inter_partition')} pcm={best.get('partition_cost_model')}"
    return (best.get("total_bytes"), lab)

def best_table_x264(data):
    tab = data.get("table")
    if not isinstance(tab, list):
        return None
    cand = []
    for row in tab:
        c = row.get("codec") or ""
        if "SRSV2" in c and row.get("error") is None:
            b = row.get("bytes")
            if b is not None:
                cand.append((b, c))
    if not cand:
        return None
    cand.sort(key=lambda x: x[0])
    return cand[0]

with open(path, encoding="utf-8") as f:
    d = json.load(f)

out = None
if tag == "compare_inter_syntax":
    out = best_compare_array(d, "compare_inter_syntax")
elif tag == "compare_rdo":
    out = best_compare_array(d, "compare_rdo")
elif tag == "compare_partition_costs":
    out = best_compare_array(d, "compare_partition_costs")
elif tag == "compare_entropy_models":
    out = best_entropy(d)
elif tag == "sweep_quality_bitrate":
    out = best_sweep(d)
elif tag == "compare_x264":
    out = best_table_x264(d)

if out:
    print(f"{out[0]} bytes — {out[1]}")
else:
    print("(no SRSV2 ok row parsed)")
PY
}

write_summary() {
	{
		printf '# Gentoo SRSV2 baseline — SUMMARY\n\n'
		printf 'Generated: %s\n' "$(date -Iseconds)"
		printf 'Git: %s\n' "$(git -C "${ROOT}" rev-parse --short HEAD 2>/dev/null || echo unknown)"
		printf 'Seed: %s  QP %s  keyint %s  motion-radius %s  fps %s\n\n' "${SEED}" "${QP}" "${KEYINT}" "${MOTION}" "${FPS}"
		if [[ "${HAVE_FFMPEG}" -eq 1 ]]; then
			printf 'ffmpeg: yes (%s)\n\n' "$(command -v ffmpeg)"
		else
			printf 'ffmpeg: no (optional x264 passes skipped)\n\n'
		fi
		printf 'Rule: among **ok** rows, pick **minimum total bytes** for SRSV2-family labels (engineering baseline).\n\n'
		printf '| Clip | compare-inter-syntax | compare-rdo | compare-partition-costs | compare-entropy-models | sweep-quality-bitrate'
		if [[ "${HAVE_FFMPEG}" -eq 1 ]]; then
			printf ' | compare-x264'
		fi
		printf ' |\n'
		printf '|------|----------------------|-------------|-------------------------|------------------------|---------------------'
		if [[ "${HAVE_FFMPEG}" -eq 1 ]]; then
			printf '|---------------'
		fi
		printf '|\n'

		while IFS='|' read -r stem pattern w h frames yuv; do
			[[ -n "${stem:-}" ]] || continue
			rd="${RUNS}/${stem}"
			c1="$(append_summary_best_python "${rd}/compare_inter_syntax.json" compare_inter_syntax)"
			c2="$(append_summary_best_python "${rd}/compare_rdo.json" compare_rdo)"
			c3="$(append_summary_best_python "${rd}/compare_partition_costs.json" compare_partition_costs)"
			c4="$(append_summary_best_python "${rd}/compare_entropy_models.json" compare_entropy_models)"
			c5="$(append_summary_best_python "${rd}/sweep_quality_bitrate.json" sweep_quality_bitrate)"
			printf '| `%s` | %s | %s | %s | %s | %s' "${stem}" "${c1}" "${c2}" "${c3}" "${c4}" "${c5}"
			if [[ "${HAVE_FFMPEG}" -eq 1 ]]; then
				c6="$(append_summary_best_python "${rd}/compare_x264.json" compare_x264)"
				printf ' | %s' "${c6}"
			fi
			printf ' |\n'
		done <"${BASE}/._clip_manifest.tmp"

		printf '\n## Artifact paths\n\n'
		printf 'Corpus: `%s`\nRuns: `%s`\n' "${CORPUS}" "${RUNS}"
	} >"${SUMMARY_MD}"
}

write_results_doc() {
	{
		printf '# Gentoo baseline benchmark results\n\n'
		printf '_Engineering measurements only — not a codec superiority claim. Numbers are machine-specific._\n\n'
		printf '## Host snapshot\n\n'
		printf '```\n'
		uname -a || true
		printf 'rustc: %s\n' "$(rustc --version 2>/dev/null || echo n/a)"
		printf 'cargo: %s\n' "$(cargo --version 2>/dev/null || echo n/a)"
		printf 'ffmpeg: %s\n' "$(command -v ffmpeg >/dev/null && ffmpeg -version 2>/dev/null | head -1 || echo not installed)"
		printf 'XDG_SESSION_TYPE=%s\n' "${XDG_SESSION_TYPE:-}"
		printf '```\n\n'
		printf '## Commands\n\n'
		printf '```bash\nbash tools/gentoo_bench_baseline.sh\n```\n\n'
		printf 'Optional: `GENTOO_BASELINE_SEED=528`, `GENTOO_BASELINE_SKIP_BUILD=1` if `bench_srsv2` already built.\n\n'
		printf '## Best SRSV2 rows\n\n'
		printf 'See [`var/bench/gentoo_baseline/SUMMARY.md`](../var/bench/gentoo_baseline/SUMMARY.md) (generated; under `var/` — gitignored except structure described here).\n\n'
		if [[ -f "${SUMMARY_MD}" ]]; then
			printf '### Copy of summary table\n\n'
			grep -A200 '^| Clip |' "${SUMMARY_MD}" | head -40 || true
			printf '\n'
		fi
		printf '## Sweep parameters\n\n'
		printf -- '- `--sweep-quality-bitrate` with `--sweep-ssim-threshold 0.90` and `--sweep-byte-budget 100000000`\n\n'
		printf '## Disclaimer\n\n'
		printf 'See [`docs/srsv2_benchmarks.md`](srsv2_benchmarks.md) for methodology. Optional x264 rows require `ffmpeg` on `PATH`.\n'
	} >"${RESULTS_DOC}"
}

write_manifest_stub() {
	local C="${CORPUS}"
	: >"${BASE}/._clip_manifest.tmp"
	# Note: use `printf -- '%s\n' "..."` — not `printf '%s\n' -- "..."` or `--` is printed as data.
	printf -- '%s\n' "gentoo_flat_64x64|flat|64|64|16|${C}/gentoo_flat_64x64.yuv" >>"${BASE}/._clip_manifest.tmp"
	printf -- '%s\n' "gentoo_gradient_64x64|gradient|64|64|16|${C}/gentoo_gradient_64x64.yuv" >>"${BASE}/._clip_manifest.tmp"
	printf -- '%s\n' "gentoo_moving_square_128x128|moving-square|128|128|24|${C}/gentoo_moving_square_128x128.yuv" >>"${BASE}/._clip_manifest.tmp"
	printf -- '%s\n' "gentoo_scrolling_bars_128x128|scrolling-bars|128|128|24|${C}/gentoo_scrolling_bars_128x128.yuv" >>"${BASE}/._clip_manifest.tmp"
	printf -- '%s\n' "gentoo_checker_64x64|checker|64|64|16|${C}/gentoo_checker_64x64.yuv" >>"${BASE}/._clip_manifest.tmp"
	printf -- '%s\n' "gentoo_scene_cut_128x128|scene-cut|128|128|24|${C}/gentoo_scene_cut_128x128.yuv" >>"${BASE}/._clip_manifest.tmp"
}

rebuild_clip_manifest() {
	: >"${BASE}/._clip_manifest.tmp"
	gen_clip flat flat 64 64 16 >>"${BASE}/._clip_manifest.tmp"
	gen_clip gradient gradient 64 64 16 >>"${BASE}/._clip_manifest.tmp"
	gen_clip moving_square moving-square 128 128 24 >>"${BASE}/._clip_manifest.tmp"
	gen_clip scrolling_bars scrolling-bars 128 128 24 >>"${BASE}/._clip_manifest.tmp"
	gen_clip checker checker 64 64 16 >>"${BASE}/._clip_manifest.tmp"
	gen_clip scene_cut scene-cut 128 128 24 >>"${BASE}/._clip_manifest.tmp"
}

only_summary() {
	write_manifest_stub
	write_summary
	write_results_doc
	rm -f "${BASE}/._clip_manifest.tmp"
	printf 'OK: regenerated %s and %s\n' "${SUMMARY_MD}" "${RESULTS_DOC}"
}

main() {
	if [[ "${1:-}" == "--only-summary" ]]; then
		only_summary
		return 0
	fi

	build_bench
	BEXE="$(bench_exe)"
	if [[ ! -x "${BEXE}" ]]; then
		echo "error: bench_srsv2 not found at ${BEXE}; run: cargo build --release -p quality_metrics --bin bench_srsv2" >&2
		exit 1
	fi

	rebuild_clip_manifest

	while IFS='|' read -r stem pattern w h frames yuv; do
		[[ -n "${stem:-}" ]] || continue
		rd="${RUNS}/${stem}"
		mkdir -p "${rd}"

		run_bench "${BEXE}" "${yuv}" "${w}" "${h}" "${frames}" \
			"${rd}/compare_inter_syntax.json" "${rd}/compare_inter_syntax.md" \
			--residual-entropy auto --compare-inter-syntax

		run_bench "${BEXE}" "${yuv}" "${w}" "${h}" "${frames}" \
			"${rd}/compare_rdo.json" "${rd}/compare_rdo.md" \
			--residual-entropy auto --compare-rdo

		run_bench "${BEXE}" "${yuv}" "${w}" "${h}" "${frames}" \
			"${rd}/compare_partition_costs.json" "${rd}/compare_partition_costs.md" \
			--residual-entropy auto --compare-partition-costs

		run_bench "${BEXE}" "${yuv}" "${w}" "${h}" "${frames}" \
			"${rd}/compare_entropy_models.json" "${rd}/compare_entropy_models.md" \
			--inter-syntax entropy --residual-entropy explicit --compare-entropy-models

		run_bench "${BEXE}" "${yuv}" "${w}" "${h}" "${frames}" \
			"${rd}/sweep_quality_bitrate.json" "${rd}/sweep_quality_bitrate.md" \
			--residual-entropy auto \
			--sweep-quality-bitrate \
			--sweep-ssim-threshold 0.90 \
			--sweep-byte-budget 100000000

		if [[ "${HAVE_FFMPEG}" -eq 1 ]]; then
			run_bench "${BEXE}" "${yuv}" "${w}" "${h}" "${frames}" \
				"${rd}/compare_x264.json" "${rd}/compare_x264.md" \
				--residual-entropy auto \
				--compare-x264 --x264-crf 23 --x264-preset medium
		fi
	done <"${BASE}/._clip_manifest.tmp"

	write_summary
	write_results_doc
	rm -f "${BASE}/._clip_manifest.tmp"
	printf 'OK: wrote %s and %s\n' "${SUMMARY_MD}" "${RESULTS_DOC}"
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
	main "$@"
fi
