---
name: powershell-safe-invocation
description: Preserve native argument boundaries and reliable failure handling when invoking Windows programs from PowerShell.
---

# Windows invocation

Identify the shell version first. PowerShell 7 and Windows PowerShell 5.1 can marshal native arguments differently. Store an executable path separately from its argument array and invoke it with `&`; each intended argument is one array element. Capture `$LASTEXITCODE` immediately after a native call. For cmdlets use `-ErrorAction Stop` or a scoped terminating-error preference instead of consulting a previous process exit code.

Use `-LiteralPath` for a file whose name is not a glob. Serialize structured data using the format library, not string concatenation. Save complex multi-line code as a temporary `.ps1` script and invoke it with `-NoProfile -NonInteractive -File`. Avoid an extra `cmd.exe` shell or expression evaluation when launching a native executable. Inspect the resulting files and exit code; absence of console output does not establish success.
