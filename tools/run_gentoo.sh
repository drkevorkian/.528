#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
export SRS_OPEN_ADMIN_UI="${SRS_OPEN_ADMIN_UI:-1}"
export SRS_GUI_BACKEND="${SRS_GUI_BACKEND:-x11}"
exec bash "${script_dir}/run_unix.sh" "$@"
