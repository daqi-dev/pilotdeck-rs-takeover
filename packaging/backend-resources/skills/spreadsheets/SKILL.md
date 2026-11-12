---
name: spreadsheets
description: "Inspect or create XLSX workbooks and CSV or TSV data for an explicit task."
---

# spreadsheets

Inspect sheet names, formulas, units and number formats before changing a workbook. Use a separately installed openpyxl or an office application for XLSX editing; distinguish stored formula text from its cached value. Recalculate with a capable engine before claiming formula results. Check row counts, missing values, totals and chart ranges. The standard-library helper inspects OOXML or delimited data, but does not recalculate formulas, convert legacy XLS or provide a chart renderer.

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
