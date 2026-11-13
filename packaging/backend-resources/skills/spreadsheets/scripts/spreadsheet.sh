#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${PILOTDECK_PYTHON:-python3}" "$HERE/../../_tools/spreadsheet_tools.py" "$@"
