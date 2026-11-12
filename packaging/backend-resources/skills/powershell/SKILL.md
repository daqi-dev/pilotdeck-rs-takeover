---
name: powershell
description: "Write or run PowerShell commands while preserving argument boundaries and error behavior."
---

# powershell

Determine whether the target uses pwsh or Windows PowerShell before relying on version-specific syntax. Pass native arguments as an array and invoke an executable path with the call operator. Read LASTEXITCODE immediately for a native process; use terminating errors for cmdlets. Prefer LiteralPath when a filename contains wildcard characters. For complex commands, write a .ps1 file and invoke it with -File. Do not use expression evaluation to assemble commands from user text.

The optional external command is `pwsh`. Inspect `pwsh --help` and its installed version before selecting version-dependent options. Missing software or an unauthorized account is an unavailable dependency, not a successful operation.
