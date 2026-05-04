#!/usr/bin/env bash
# Gentoo / Unix prerequisite probe for the .528 workspace.
# Does not install packages; does not require root.
# Exit status: 0 if all required tools are present; nonzero if any required tool is missing.

set -uo pipefail

pass_count=0
warn_count=0
fail_count=0

pass_line() {
	printf 'PASS: %s\n' "$1"
	pass_count=$((pass_count + 1))
}

warn_line() {
	printf 'WARN: %s\n' "$1"
	warn_count=$((warn_count + 1))
}

fail_line() {
	printf 'FAIL: %s\n' "$1"
	fail_count=$((fail_count + 1))
}

one_line_version() {
	local out
	out="$("$@" 2>/dev/null)" || return 1
	printf '%s\n' "${out}" | head -n 1
}

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(CDPATH='' cd -- "${script_dir}/.." && pwd)"
toolchain_file="${repo_root}/rust-toolchain.toml"

# --- required tools ---
for cmd in rustc cargo rustfmt git; do
	if command -v "${cmd}" >/dev/null 2>&1; then
		ver="$(one_line_version "${cmd}" --version)" || ver="(version unknown)"
		pass_line "${cmd} (${ver})"
	else
		fail_line "${cmd} not on PATH"
	fi
done

if command -v cargo >/dev/null 2>&1; then
	if ver="$(one_line_version cargo clippy --version)"; then
		pass_line "cargo clippy (${ver})"
	else
		fail_line "cargo clippy not available (install clippy / rustup component)"
	fi
else
	fail_line "cargo missing; skipped cargo clippy check"
fi

# --- optional ---
if command -v ffmpeg >/dev/null 2>&1; then
	ver="$(one_line_version ffmpeg -version)" || ver="present"
	pass_line "ffmpeg optional (${ver})"
else
	warn_line "ffmpeg not on PATH (optional; needed for compare-x264 / libsrs_compat ffmpeg feature)"
fi

if command -v pkg-config >/dev/null 2>&1; then
	ver="$(one_line_version pkg-config --version)" || ver="present"
	pass_line "pkg-config optional (${ver})"
else
	warn_line "pkg-config not on PATH (optional; some native crates use it)"
fi

# --- rust-toolchain hint ---
if [[ -f "${toolchain_file}" ]]; then
	channel="$(awk -F '"' '/^[[:space:]]*channel[[:space:]]*=/ { print $2; exit }' "${toolchain_file}")"
	if [[ -n "${channel}" ]] && command -v rustc >/dev/null 2>&1; then
		rustc_v="$(one_line_version rustc --version)" || rustc_v=""
		matches=0
		case "${rustc_v}" in
			*"${channel}"*) matches=1 ;;
		esac
		if [[ "${matches}" -eq 1 ]]; then
			pass_line "rust-toolchain channel '${channel}' matches rustc label"
		elif [[ "${channel}" == "stable" ]] && [[ "${rustc_v}" =~ rustc[[:space:]][0-9]+\.[0-9] ]]; then
			pass_line "rust-toolchain channel '${channel}' with release rustc (Portage/system toolchain)"
		else
			warn_line "rust-toolchain channel '${channel}' vs rustc '${rustc_v}' (check rustup override)"
		fi
	fi
fi

printf 'STATUS: pass=%s warn=%s fail=%s\n' "${pass_count}" "${warn_count}" "${fail_count}"

if [[ "${fail_count}" -gt 0 ]]; then
	exit 1
fi
exit 0
