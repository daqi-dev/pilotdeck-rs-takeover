---
name: 1password
description: "Use the installed 1Password CLI to retrieve or inject secrets for an authorized task."
---

# 1password

Check the selected account and vault before resolving an item. Request only the fields required by the current operation. Prefer passing a secret through a child environment or stdin; do not place it in a generated file, command transcript, commit, or reply. Confirm the destination before writing or sharing an item. An installed CLI and an already authorized account are required; sign-in remains an interactive user operation.

The optional external command is `op`. Inspect `op --help` and its installed version before selecting version-dependent options. Missing software or an unauthorized account is an unavailable dependency, not a successful operation.
