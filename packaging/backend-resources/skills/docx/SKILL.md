---
name: docx
description: "Create, edit or inspect DOCX Word documents while preserving document structure."
---

# docx

Inspect paragraphs, tables, sections, headers and embedded assets before editing. For creation or rich edits use a separately installed python-docx or office application appropriate to the task. Preserve styles and relationships; tracked changes and fields require format-aware handling. Render through an available office application and inspect every final page. A ZIP/XML inspection is a structural check, not a visual review. This package does not bundle a Word renderer or the baseline advanced redline toolkit.

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
