#!/usr/bin/env python3
"""Resource entry point; no private host paths or implicit installation."""
from pathlib import Path
import runpy
runpy.run_path(str(Path(__file__).resolve().parents[2] / '_tools/pdf_tools.py'), run_name='__main__')
