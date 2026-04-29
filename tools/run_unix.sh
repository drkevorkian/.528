#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "${script_dir}/.." && pwd)"
config_path="${SRS_CONFIG_PATH:-${repo_root}/config/srs.toml}"

mode="player"
release_flag=""
start_server=1
admin_ui="${SRS_OPEN_ADMIN_UI:-0}"
gui_backend="${SRS_GUI_BACKEND:-auto}"
declare -a passthrough=()
profile_dir="debug"

usage() {
  cat <<'EOF'
Usage:
  bash tools/run_unix.sh [player|server|cli|help] [options] [-- cli args...]

Modes:
  player   Start the local licensing server, then launch the desktop app.
  server   Run only the local licensing server in the foreground.
  cli      Start the server, then run the CLI with the remaining arguments.
  help     Show this message.

Options:
  --release        Run Cargo in release mode.
  --no-server      Skip auto-starting the local licensing server.
  --admin-ui       Start the dedicated admin desktop UI.
  --no-admin-ui    Do not start the admin desktop UI.
  --gui-backend B  Force GUI backend: auto, x11, or wayland.
  --config PATH    Use a specific config file instead of config/srs.toml.

Examples:
  bash tools/run_unix.sh
  bash tools/run_unix.sh server
  bash tools/run_unix.sh cli analyze path/to/file.528
  bash tools/run_unix.sh cli --no-server -- analyze path/to/file.528

Supported Unix targets:
  - Linux: Gentoo, Ubuntu, RHEL-compatible, SUSE-compatible
  - macOS
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command '$1'"
}

read_bind_addr() {
  local value
  if [[ -f "${config_path}" ]]; then
    value="$(awk -F '"' '/^bind_addr = / { print $2; exit }' "${config_path}")"
  fi
  printf '%s\n' "${value:-127.0.0.1:3000}"
}

read_bind_port() {
  local bind_addr
  bind_addr="$(read_bind_addr)"
  printf '%s\n' "${bind_addr##*:}"
}

listener_pid_for_bind_port() {
  local bind_port
  bind_port="$(read_bind_port)"

  if command -v lsof >/dev/null 2>&1; then
    lsof -t -iTCP:${bind_port} -sTCP:LISTEN -Pn 2>/dev/null | head -n1
    return 0
  fi

  if command -v ss >/dev/null 2>&1; then
    ss -ltnp "( sport = :${bind_port} )" 2>/dev/null \
      | sed -n 's/.*pid=\([0-9]\+\).*/\1/p' \
      | head -n1
    return 0
  fi

  return 0
}

listener_info_for_bind_port() {
  local bind_port
  bind_port="$(read_bind_port)"

  if command -v lsof >/dev/null 2>&1; then
    lsof -iTCP:${bind_port} -sTCP:LISTEN -Pn 2>/dev/null || true
    return 0
  fi

  if command -v ss >/dev/null 2>&1; then
    ss -ltnp "( sport = :${bind_port} )" 2>/dev/null || true
    return 0
  fi

  return 0
}

stop_existing_local_server_processes() {
  local pid cmdline
  pid="$(listener_pid_for_bind_port || true)"
  [[ -n "${pid}" ]] || return 0

  cmdline="$(ps -p "${pid}" -o args= 2>/dev/null || true)"
  if [[ "${cmdline}" == *srs_license_server* ]]; then
    printf 'Stopping existing local srs_license_server listener on port %s (pid %s)\n' "$(read_bind_port)" "${pid}"
    kill "${pid}" >/dev/null 2>&1 || true
    sleep 0.5
  fi
}

ensure_bind_port_available() {
  local bind_port bind_info pid
  bind_port="$(read_bind_port)"
  pid="$(listener_pid_for_bind_port || true)"
  [[ -z "${pid}" ]] && return 0

  bind_info="$(listener_info_for_bind_port)"
  printf 'error: port %s is already in use by another process.\n%s\n' "${bind_port}" "${bind_info}" >&2
  exit 1
}

wait_for_server() {
  local bind_addr probe_host probe_port
  bind_addr="$(read_bind_addr)"
  probe_host="${bind_addr%:*}"
  probe_port="${bind_addr##*:}"

  if [[ "${probe_host}" == "0.0.0.0" || "${probe_host}" == "*" ]]; then
    probe_host="127.0.0.1"
  fi

  for _ in {1..50}; do
    if command -v curl >/dev/null 2>&1; then
      if curl -fsS "http://${probe_host}:${probe_port}/healthz" >/dev/null 2>&1; then
        return 0
      fi
      sleep 0.2
      continue
    fi
    if (exec 3<>"/dev/tcp/${probe_host}/${probe_port}") >/dev/null 2>&1; then
      exec 3>&-
      exec 3<&-
      return 0
    fi
    sleep 0.2
  done

  return 1
}

run_cargo_package() {
  local package="$1"
  shift

  if [[ -n "${release_flag}" ]]; then
    SRS_CONFIG_PATH="${config_path}" cargo run --release -p "${package}" -- "$@"
  else
    SRS_CONFIG_PATH="${config_path}" cargo run -p "${package}" -- "$@"
  fi
}

build_cargo_packages() {
  local packages=("$@")
  local cargo_args=(build)

  if [[ -n "${release_flag}" ]]; then
    cargo_args+=(--release)
    profile_dir="release"
  else
    profile_dir="debug"
  fi

  for package in "${packages[@]}"; do
    cargo_args+=(-p "${package}")
  done

  SRS_CONFIG_PATH="${config_path}" cargo "${cargo_args[@]}"
}

package_binary_path() {
  local package="$1"
  printf '%s/target/%s/%s\n' "${repo_root}" "${profile_dir}" "${package}"
}

effective_gui_backend() {
  case "${gui_backend}" in
    x11|wayland)
      printf '%s\n' "${gui_backend}"
      ;;
    auto)
      if [[ -n "${WAYLAND_DISPLAY:-}" && -n "${DISPLAY:-}" ]]; then
        printf 'wayland\n'
      elif [[ -n "${WAYLAND_DISPLAY:-}" ]]; then
        printf 'wayland\n'
      elif [[ -n "${DISPLAY:-}" ]]; then
        printf 'x11\n'
      else
        printf 'auto\n'
      fi
      ;;
    *)
      die "invalid GUI backend '${gui_backend}' (expected auto, x11, or wayland)"
      ;;
  esac
}

run_package_binary() {
  local package="$1"
  shift
  local binary
  binary="$(package_binary_path "${package}")"
  [[ -x "${binary}" ]] || die "built binary not found: ${binary}"
  SRS_CONFIG_PATH="${config_path}" "${binary}" "$@"
}

run_gui_binary() {
  local package="$1"
  shift
  local binary backend
  binary="$(package_binary_path "${package}")"
  [[ -x "${binary}" ]] || die "built binary not found: ${binary}"
  backend="$(effective_gui_backend)"

  case "${backend}" in
    x11)
      [[ -n "${DISPLAY:-}" ]] || die "DISPLAY is not set for x11 backend"
      printf 'Launching %s with X11 backend.\n' "${package}"
      env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
        "SRS_CONFIG_PATH=${config_path}" \
        "DISPLAY=${DISPLAY}" \
        "XDG_SESSION_TYPE=x11" \
        "WINIT_UNIX_BACKEND=x11" \
        "${binary}" "$@"
      ;;
    wayland)
      [[ -n "${WAYLAND_DISPLAY:-}" ]] || die "WAYLAND_DISPLAY is not set for wayland backend"
      printf 'Launching %s with Wayland backend.\n' "${package}"
      env -u DISPLAY \
        "SRS_CONFIG_PATH=${config_path}" \
        "WAYLAND_DISPLAY=${WAYLAND_DISPLAY}" \
        "XDG_SESSION_TYPE=wayland" \
        "WINIT_UNIX_BACKEND=wayland" \
        "${binary}" "$@"
      ;;
    auto)
      SRS_CONFIG_PATH="${config_path}" "${binary}" "$@"
      ;;
  esac
}

admin_pid=""

open_admin_ui() {
  [[ "${admin_ui}" == "1" ]] || return 0

  printf 'Starting dedicated admin UI...\n'
  local binary backend
  binary="$(package_binary_path srs_admin)"
  [[ -x "${binary}" ]] || die "built admin binary not found: ${binary}"
  backend="$(effective_gui_backend)"
  case "${backend}" in
    x11)
      [[ -n "${DISPLAY:-}" ]] || die "DISPLAY is not set for x11 backend"
      printf 'Launching srs_admin with X11 backend.\n'
      env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
        "SRS_CONFIG_PATH=${config_path}" \
        "DISPLAY=${DISPLAY}" \
        "XDG_SESSION_TYPE=x11" \
        "WINIT_UNIX_BACKEND=x11" \
        "${binary}" >"${repo_root}/var/srs_admin.log" 2>&1 &
      ;;
    wayland)
      [[ -n "${WAYLAND_DISPLAY:-}" ]] || die "WAYLAND_DISPLAY is not set for wayland backend"
      printf 'Launching srs_admin with Wayland backend.\n'
      env -u DISPLAY \
        "SRS_CONFIG_PATH=${config_path}" \
        "WAYLAND_DISPLAY=${WAYLAND_DISPLAY}" \
        "XDG_SESSION_TYPE=wayland" \
        "WINIT_UNIX_BACKEND=wayland" \
        "${binary}" >"${repo_root}/var/srs_admin.log" 2>&1 &
      ;;
    auto)
      SRS_CONFIG_PATH="${config_path}" "${binary}" >"${repo_root}/var/srs_admin.log" 2>&1 &
      ;;
  esac
  admin_pid="$!"
  sleep 1
  if ! kill -0 "${admin_pid}" >/dev/null 2>&1; then
    printf 'warning: dedicated admin UI exited early; see %s/var/srs_admin.log\n' "${repo_root}" >&2
  fi
}

server_pid=""
cleanup() {
  if [[ -n "${server_pid}" ]] && kill -0 "${server_pid}" >/dev/null 2>&1; then
    kill "${server_pid}" >/dev/null 2>&1 || true
    wait "${server_pid}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${admin_pid}" ]] && kill -0 "${admin_pid}" >/dev/null 2>&1; then
    kill "${admin_pid}" >/dev/null 2>&1 || true
    wait "${admin_pid}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

while (($#)); do
  case "$1" in
    player|server|cli|help)
      mode="$1"
      shift
      ;;
    --release)
      release_flag="--release"
      shift
      ;;
    --no-server)
      start_server=0
      shift
      ;;
    --admin-ui)
      admin_ui="1"
      shift
      ;;
    --no-admin-ui)
      admin_ui="0"
      shift
      ;;
    --gui-backend)
      (($# >= 2)) || die "--gui-backend requires auto, x11, or wayland"
      gui_backend="$2"
      shift 2
      ;;
    --config)
      (($# >= 2)) || die "--config requires a path"
      config_path="$2"
      shift 2
      ;;
    --)
      shift
      passthrough+=("$@")
      break
      ;;
    *)
      passthrough+=("$1")
      shift
      ;;
  esac
done

require_command cargo
[[ -f "${config_path}" ]] || die "config file not found: ${config_path}"

cd "${repo_root}"

case "${mode}" in
  help)
    usage
    ;;
  server)
    stop_existing_local_server_processes
    ensure_bind_port_available
    build_cargo_packages srs_license_server
    run_package_binary srs_license_server
    ;;
  player|cli)
    if [[ "${mode}" == "player" ]]; then
      if (( admin_ui )); then
        build_cargo_packages srs_license_server srs_admin srs_player
      else
        build_cargo_packages srs_license_server srs_player
      fi
    else
      if (( admin_ui )); then
        build_cargo_packages srs_license_server srs_admin srs_cli
      else
        build_cargo_packages srs_license_server srs_cli
      fi
    fi

    if (( start_server )); then
      stop_existing_local_server_processes
      ensure_bind_port_available
      mkdir -p "${repo_root}/var"
      SRS_CONFIG_PATH="${config_path}" "$(package_binary_path srs_license_server)" \
        >"${repo_root}/var/srs_license_server.log" 2>&1 &
      server_pid="$!"
      wait_for_server || die "local licensing server did not start; see var/srs_license_server.log"
      open_admin_ui
    fi

    if [[ "${mode}" == "player" ]]; then
      run_gui_binary srs_player
    else
      run_package_binary srs_cli "${passthrough[@]}"
    fi
    ;;
  *)
    die "unknown mode '${mode}'"
    ;;
esac
