#!/usr/bin/env python3
"""Launch the independently authored package helper from an installed skill."""
from pathlib import Path
import runpy
runpy.run_path(str(Path(__file__).resolve().parents[2] / '_tools' / 'docx_tools.py'), run_name='__main__')
