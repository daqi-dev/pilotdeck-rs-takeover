---
name: apple-notes
description: "Read or update Apple Notes using an installed memo CLI on macOS."
---

# apple-notes

Inspect the available notebooks and identify a note by its title and notebook together. Read before updating; retain attachments and formatting when the selected CLI cannot round-trip them. For an export, write a new file and compare the exported text with the source. macOS automation permission may require a user action. Avoid treating an empty listing following a permission error as an empty notebook.

The optional external command is `memo`. Inspect `memo --help` and its installed version before selecting version-dependent options. Missing software or an unauthorized account is an unavailable dependency, not a successful operation.
