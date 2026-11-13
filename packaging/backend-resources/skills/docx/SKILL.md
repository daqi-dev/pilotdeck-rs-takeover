---
name: docx
description: "Create, inspect, review, redact metadata, finalize revisions and render Word DOCX documents."
---

# DOCX

Inspect the document before editing and keep intermediates in `PILOTDECK_WORK_DIR` when supplied. The independently authored Python standard-library helper works directly on DOCX ZIP/XML. Inspection includes paragraph/run styles, nested tables, sections, headers and footers, fields, revisions and package feature inventories; incomplete semantic coverage is reported explicitly. It preserves unrelated package member bytes and run properties during supported text replacements. It does not supply a full Word layout engine or OOXML schema validator.

```sh
DOCX="{{SKILL_ROOT_SHELL}}/scripts/docx_cli.py"
python3 "$DOCX" inspect --input "$INPUT" --out "$WORK/inspection.json"
python3 "$DOCX" validate --input "$INPUT"
python3 "$DOCX" annotate --input "$INPUT" --spec "$WORK/review.json" --out "$WORK/review.docx"
python3 "$DOCX" finalize --input "$WORK/review.docx" --accept-changes --remove-comments --out "$WORK/final.docx"
python3 "$DOCX" compare --before "$INPUT" --after "$WORK/final.docx" --out "$WORK/changes.json"
python3 "$DOCX" sanitize --input "$WORK/final.docx" --remove-comments --out "$WORK/sanitized.docx"
```

The review JSON accepts `comments` and `tracked_replacements` arrays. Each entry uses a nonempty `match`, optional positive `occurrence`, `author` and `date`. Comments use `text` and anchor the containing paragraph. Replacements use a nonempty `replacement` and produce real `w:del`/`w:ins` revisions, preserving the original run formatting. Example:

```json
{"comments":[{"match":"old wording","text":"Please confirm this revision."}],"tracked_replacements":[{"match":"old wording","replacement":"new wording"}]}
```

Ambiguous matches require `occurrence`. Replacements spanning multiple runs, runs with drawings/fields, complex move/property/table revisions, signed/macro packages and markup-compatibility AlternateContent require a dedicated editor and fail explicitly. Main-story annotation is supported; finalization also processes revisions in headers, footers and notes. `--reject-changes` restores deletions and discards insertions. `--remove-comments` removes comment parts, markers, references and relationships. Sanitization clears core author/last-modified-by/subject/keywords, custom properties and Word rsid attributes; it does **not** promise anonymization of all body text, revision authors or embedded files.

Render with explicit externally provisioned executables:

```sh
python3 "$DOCX" render --input "$WORK/final.docx" --out-dir "$WORK/pages" \
  --soffice "$PILOTDECK_SOFFICE" --pdf-renderer "$PILOTDECK_PDF_RENDERER" \
  --mutool "$PILOTDECK_MUTOOL" --dpi 150 --timeout 180
```

`PILOTDECK_SOFFICE` and `PILOTDECK_PDF_RENDERER` may supply the executable paths without flags; the PDF renderer is `pdftoppm`. Rendering needs LibreOffice, Poppler and appropriate fonts. Rendering text and glyph checks also require a source-built MuPDF `mutool` supplied through `PILOTDECK_MUTOOL` or `--mutool`; missing it blocks rendering. The helper compares character boxes with actual PNG pixels and rejects pages with at least eight CJK characters when fewer than 70% are visibly painted. Use repeatable `--font-dir` paths for separately supplied font directories when the renderer needs them. These engines and fonts are not bundled by this skill. Missing engines, a failed conversion, timeout or missing PDF/PNG pages return an error and do not publish new page images. Successful pages are written beneath the input SHA-256 prefix directory under `--out-dir`. Current owned process-group cleanup requires POSIX. Open every generated page before making visual claims. The CJK paint check detects missing paint, not every incorrect glyph or pagination error. PNG output still needs visual review.

Publish the exact candidate atomically, preserving a supplied source by default:

```sh
python3 "$DOCX" deliver --input "$WORK/final.docx" --source "$INPUT" --out "$DELIVERY"
```

Outputs refuse clobbering unless `--overwrite` is explicit. Replacing the supplied source requires `--replace-source` and `PILOTDECK_WORK_DIR`; a digest-verified recovery copy is kept there. Replacement rechecks source identity and content immediately before commit and refuses a concurrent save; it does not promise a filesystem transaction against arbitrary external writers. Non-replacement delivery hashes a supplied source before publication and rechecks it afterward; concurrent changes cause an error even when the candidate was already published. With that environment set, delivery accepts candidates inside it and writes final outputs outside it. `validate` checks bounded ZIP/XML structure and internal relationship targets; it does not perform complete OOXML schema, field calculation or visual validation.

For basic creation, the shared `../_tools/artifact_build.py` remains available with separately installed python-docx. Rich formatting/layout changes need a suitable document library or Office application; the review helper does not synthesize arbitrary Word layouts. `python3 "$DOCX" self-test` runs generated-document regression tests. See `IMPLEMENTATION.md` for measured coverage and remaining baseline differences.
