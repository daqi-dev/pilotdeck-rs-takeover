#!/bin/sh
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cache=${PDF_SKILL_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/pilotdeck-pdf}
if [ -n "${PILOTDECK_PYTHON:-}" ]; then
    python=$PILOTDECK_PYTHON
elif [ -x "$cache/runtime/bin/python" ]; then
    python=$cache/runtime/bin/python
else
    python=python3
fi
if [ "$#" -eq 0 ] || [ "${1:-}" = help ]; then
    exec "$python" "$script_dir/pdf_cli.py" --help
fi
exec "$python" "$script_dir/pdf_cli.py" "$@"
