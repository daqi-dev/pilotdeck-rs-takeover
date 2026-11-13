---
name: pptx
description: "Create, edit, inspect, render and safely deliver editable PowerPoint presentations; convert legacy binary PPT using an explicit office engine."
---

# Editable PowerPoint presentations

Determine audience, purpose, slide sequence, source facts and aspect ratio first. Preserve the source deck. Keep candidate files, comparison reports, renders and recovery copies under `PILOTDECK_WORK_DIR`; place the selected final deliverable outside that directory.

Use Python, python-pptx, a suitable office application or direct OOXML for authoring. The separately installed python-pptx library can create basic slides through the sibling `artifact_build.py`; the review helper itself uses Python's standard library for package validation, comparison and delivery. Never flatten a deck to screenshots and describe it as editable.

The packaged entry has five commands:

```sh
PPTX="{{SKILL_ROOT_SHELL}}/scripts/pptx.sh"
"$PPTX" validate --input "$CANDIDATE" --out "$PILOTDECK_WORK_DIR/validation.json"
"$PPTX" compare --source "$SOURCE" --candidate "$CANDIDATE" --out "$PILOTDECK_WORK_DIR/comparison.json"
"$PPTX" render --input "$CANDIDATE" --out-dir "$PILOTDECK_WORK_DIR/new-slides" \
  --soffice "$PILOTDECK_SOFFICE" --pdf-renderer "$PILOTDECK_PDF_RENDERER"
"$PPTX" deliver --input "$CANDIDATE" --out "$FINAL" --source "$SOURCE"
"$PPTX" convert-legacy --input "$LEGACY_PPT" --out "$PILOTDECK_WORK_DIR/converted.pptx" \
  --soffice "$PILOTDECK_SOFFICE"
```

`validate` inspects package relationships, content types, ordered slides, text, notes and feature counts. It rejects malformed XML, dangling references, duplicate ZIP members, path traversal and DTD/entity declarations. A structural success does not prove that slide layout is correct. `compare` reports actual part, slide text and notes changes; it does not make a visual pass/fail judgment.

`render` requires explicitly configured executable paths: `--soffice` or `PILOTDECK_SOFFICE`, and `--pdf-renderer` or `PILOTDECK_PDF_RENDERER`. Legacy `PPTX_SKILL_SOFFICE` and `PPTX_SKILL_PDF_RENDERER` are also recognized. LibreOffice converts to PDF under a fresh `UserInstallation` profile; pdftoppm or mutool rasterizes it. Pillow validates that each output is a real nonempty PNG, and page count must equal presentation order. Rendering uses a new output directory and refuses an existing one. No component is automatically installed. Missing engines, missing output, invalid images and nonzero engine exits are errors. `--dpi` defaults to 144; optional `--timeout` bounds each engine call.

Read every rendered slide to check clipping, overlap, spacing, alignment, fonts and intended visual hierarchy. Inspect speaker notes, charts, embedded workbooks, layouts, masters, animations and uncommon media when the task depends on them. LibreOffice can substitute fonts and differs from PowerPoint; do not describe its render as a PowerPoint fidelity guarantee.

For localized text work, a task script may import the sibling `_tools/presentation_tools.py` and call `replace_text(source, candidate, old, new)`. This changes matching text within individual slide text runs and preserves all other package parts byte-for-byte. It does not match across runs, edit chart data or rebuild shapes. Signed packages are refused for editing until the task supplies a signature strategy. For broader authoring, use an appropriate format library and revalidate the complete candidate.

`deliver` validates the candidate, copies it by atomic publication and verifies final bytes. Existing final output requires `--overwrite`. Replacing the original requires both `--source "$SOURCE"` and `--replace-source`, with `--out` identifying that source; a verified recovery copy is retained under `PILOTDECK_WORK_DIR/pptx/recovery`. Candidate and source must be distinct; final delivery must be outside the work directory. Recheck content and visual evidence before selecting a candidate.

`convert-legacy` accepts actual OLE binary PowerPoint input, preserves it and produces a validated `.pptx` candidate through LibreOffice. It reports conversion warnings because legacy layouts, animation, fonts, embedded objects and editability may change. Existing candidate output requires `--force`. Review and render the converted deck before delivering it.
