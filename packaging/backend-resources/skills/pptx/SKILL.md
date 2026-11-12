---
name: pptx
description: "Create, edit or inspect an editable PPTX presentation."
---

# pptx

Establish audience, slide sequence and aspect ratio before building. Inspect an input deck before changing layouts or masters. Use a separately installed python-pptx, PptxGenJS or office application when editing shapes and charts. Keep text within its bounds, preserve editability, and inspect a render of every slide. Verify notes and chart data when they matter. This package inspector extracts package structure and text; it is not a slide renderer or a legacy PPT converter.

Inspect local input without extracting archive members:

```sh
python3 {{SKILL_ROOT_SHELL}}/../_tools/artifact_inspect.py "$INPUT"
```

Keep intermediate files under the turn work directory (`PILOTDECK_WORK_DIR` when supplied); deliver only requested outputs. Verify content and appearance before reporting completion.

Create a basic editable file from a small JSON specification using the separately installed format library:

```sh
python3 {{SKILL_ROOT_SHELL}}/../_tools/artifact_build.py "$SPEC_JSON" "$OUTPUT"
```

The helper accepts DOCX `title`, `paragraphs` (strings or text/heading objects) and `tables` (row arrays); XLSX `sheets` with `name` and `rows`; PPTX `slides` with `title` and `body`. It refuses existing outputs. Formula input remains formula text until an external engine recalculates it. Advanced editing, rendering and baseline helper CLI equivalence remain outside this small helper.
