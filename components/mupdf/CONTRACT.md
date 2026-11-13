# Private PDF component contract

`pilotdeck-pdf` is a source-built MuPDF helper used by the Rust host. This is an internal process contract, not a Gateway wire protocol. The host owns input snapshots, timeouts, cancellation, process cleanup and output limits.

| Exit | Meaning |
| --- | --- |
| 0 | Requested count, text or page-render operation succeeded. |
| 1 | Ordinary open, parse, IO, MuPDF, raster-limit or other runtime failure. Keep its diagnostic; it does not identify an invalid page. |
| 2 | Invalid command or arguments. |
| 3 | **Render only:** the document opened successfully, page count completed, and the positive requested page is greater than that count. No page is rendered. |

Only the helper's explicit `requested > pages` branch assigns exit 3. It skips rendering and still reaches `fz_always` resource cleanup. If a later output flush fails, the ordinary catch sets exit 1 instead. A message containing “out of range” never establishes exit 3 semantics by itself.

For `render` with a known positive requested page and exit 3, the Rust PDF adapter reports `invalid page number: N`, matching the fixed TypeScript page-load failure observed for a repairable zero-page PDF. It does not classify by stderr text. In all other modes, with a missing or zero page, or for any other exit, the prior error branch remains. Count exit 1 remains a parse failure; IO, timeout, cancellation and output-size handling are unchanged.

The helper and Rust consumer must be deployed together. Rebuild only the wrapper target against the already built, pinned MuPDF libraries; record the new C source and executable SHA. Do not relabel an old helper binary as supporting this contract.

Required validation is the real repairable 45-byte PDF's `render 1` branch plus a missing file, an unrecoverable non-PDF, invalid arguments, and the existing host timeout/cancellation controls. The Rust projection tests also require that ordinary exit 1 with similar error text, count/text exit 3, unknown exit, and absent/zero page do not receive the render mapping. These checks are required; this document is not evidence that they have executed.
