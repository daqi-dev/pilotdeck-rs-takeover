---
name: pdf
description: "Inspect, extract, build, merge, split, rotate, fill and render local PDF files."
---

# PDF

Use `bash {{SKILL_ROOT_SHELL}}/scripts/pdf.sh <command>`. Preserve source PDFs and use distinct new outputs. Every output path must be new (only scaffold accepts an explicit `--force` to replace its builder); reuse a fresh QA directory on each iteration. Intermediates belong under `${PILOTDECK_WORK_DIR:-$PWD/.pilotdeck/work/manual/pdf}`. Do not infer visual correctness from text extraction.

The package supplies Python workflow code and a basic builder, not Python libraries or a renderer. `pypdf`, `pdfplumber`, ReportLab, chardet, and Pillow must be installed in the selected external Python (`PILOTDECK_PYTHON`, default `python3`). Render with system Poppler `pdftoppm`, or set `PILOTDECK_PDF_RENDERER` to a separately source-built MuPDF `mutool`. `check` reports library versions and renderer identity; it fails if dependencies are absent. No implicit installation, private host paths, OCR, or network conversion is performed. Run `fix` explicitly to create a private venv under `${PDF_SKILL_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/pilotdeck-pdf}/runtime`; it reuses existing external site packages and installs missing requirements from source only (`--no-binary=:all:`), without modifying the global Python. Build tools must already be available. Set `PILOTDECK_PDF_SOURCE_DIR` to a directory of source distributions for offline installation. Missing source/build prerequisites fail visibly; renderers must be supplied separately. External runtime licensing and packaging are separate from this helper.

```sh
PDF_TOOL={{SKILL_ROOT_SHELL}}/scripts/pdf.sh
bash "$PDF_TOOL" check || bash "$PDF_TOOL" fix
bash "$PDF_TOOL" inspect --input input.pdf --out "$WORK/inspection.json" --text-out "$WORK/text.json" --tables-out "$WORK/tables.json"
bash "$PDF_TOOL" scaffold --out "$WORK/build_pdf.py"
bash "$PDF_TOOL" build --builder "$WORK/build_pdf.py" --out final.pdf
bash "$PDF_TOOL" merge --inputs first.pdf second.pdf --out merged.pdf
bash "$PDF_TOOL" split --input input.pdf --pages '1-3,7' --out-dir "$WORK/split"
bash "$PDF_TOOL" rotate --input input.pdf --pages '2,4-5' --degrees 90 --out rotated.pdf
bash "$PDF_TOOL" forms-inspect --input form.pdf --out "$WORK/fields.json"
bash "$PDF_TOOL" forms-fill --input form.pdf --data "$WORK/values.json" --out filled.pdf
bash "$PDF_TOOL" audit --input final.pdf --out "$WORK/audit.json"
bash "$PDF_TOOL" render --input final.pdf --out-dir "$WORK/render" --dpi 144 --montage "$WORK/montage.png"
bash "$PDF_TOOL" self-test --out "$WORK/self-test"
```

Set `WORK` to the turn work directory before running these examples. Page selections are 1-based inclusive ranges; repeated page numbers are deduplicated preserving first occurrence. Rotation applies only to selected pages and must be a multiple of 90. Merge/split/rotation preserve PDF page content without reflow. The previous `scripts/pdf_pages.py` entry remains available for compatibility with its existing selection contract.

The scaffold is one editable, offline ReportLab builder accepting `--out`. It uses paragraph flowables, explicit page margins/type styles, a table with repeated headers, and deterministic page footers. Use `PILOTDECK_PDF_FONT` for a project-provided TrueType font; the builder registers it and embeds it. An explicit Helvetica Base14 fallback is available when no supported local font exists; check required glyphs and use a suitable font for multilingual text. Keep images at their intended aspect ratio and use ReportLab flowables for additional layout. The builder is executable task code: review it before running. `artifact_build.py` handles other document formats and does not supply PDF generation.

The fixed baseline requires `reportlab>=4.2,<5` without an exact pin. This resource selects ReportLab 4.2.5 (source release 2024-10-01, before the baseline commit), plus chardet 5.2.0; source-only runtime requirements are in `runtime/requirements.txt`. ReportLab is BSD licensed, chardet LGPL-2.1; these external packages and their license files remain in the separate runtime and are not vendored or relicensed by this helper.

AcroForm values are a JSON object mapping existing field names to values. `forms-fill` clones the full document, updates every page appearance, and checks canonical field values after writing; it keeps forms interactive. Inspect canonical fields and page widgets before filling. Ambiguous/orphaned widget trees need a dedicated repair workflow; this helper does not repair or flatten them. XFA, encrypted and signature forms require separate workflows and are rejected. Existing signatures cannot be preserved by pretending a rewrite is harmless.

Audit checks parseability, page count, geometry and extraction; empty/vector-only pages and mixed geometry can produce warnings requiring review. It does not detect clipping/overlap or certify appearance. Render every final page and inspect each full-resolution PNG; montage is only an overview. For filled forms, verify both field values and visible appearances. Image-only valid pages can have empty extraction; OCR is not supplied.

Read `references/contracts.md` for the implementation boundary and verification command. Never report completion on a failed builder, missing renderer, absent form field, audit hard failure or partial render.
