---
name: pdf
description: "Read, create, transform or inspect PDF files with available local tools."
---

# pdf

Use the native read_file PDF capability for extraction when available. For page-level edits use separately installed pypdf; for new documents use an available PDF generator such as ReportLab. Preserve the input and write a distinct output. Render final pages using an available renderer and inspect clipping, glyphs, pagination and forms. Text extraction alone is insufficient visual evidence. Full packages may include MuPDF for the native reader; Python editing libraries and visual export workflows are optional external dependencies, not silently supplied by this resource.

Keep intermediate files under the turn work directory (`PILOTDECK_WORK_DIR` when supplied); deliver only requested outputs. Verify content and appearance before reporting completion.

Inspect page boxes and form-field names, or select/reorder/rotate pages with an installed pypdf:

```sh
python3 {{SKILL_ROOT_SHELL}}/scripts/pdf_pages.py "$INPUT_PDF"
python3 {{SKILL_ROOT_SHELL}}/scripts/pdf_pages.py "$INPUT_PDF" --pages 2,1 --rotate 90 --output "$OUTPUT_PDF"
```

Page numbers are 1-based. Existing outputs and encrypted inputs are refused. This helper does not fill forms, preserve a complete interactive form catalog during page selection, or generate page images; use an appropriate format-aware workflow for those requirements.
