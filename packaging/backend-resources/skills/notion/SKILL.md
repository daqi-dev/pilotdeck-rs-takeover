---
name: notion
description: "Read or update Notion pages and data sources using an installed Notion CLI."
---

# notion

Find the target page or data source and inspect its properties before editing. Use stable IDs and distinguish a data source schema change from adding a page. Preserve blocks not mentioned by the task. For large content, paginate until the requested range is complete. An API error is not evidence that a page is absent. Read back edits; authentication, workspace sharing and remote writes remain externally authorized.

The optional external command is `notion`. Inspect `notion --help` and its installed version before selecting version-dependent options. Missing software or an unauthorized account is an unavailable dependency, not a successful operation.
