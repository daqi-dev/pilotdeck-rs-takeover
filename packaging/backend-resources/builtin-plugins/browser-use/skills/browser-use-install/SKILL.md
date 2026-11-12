---
name: browser-use-install
description: Diagnose and provision the optional browser-use MCP dependency for an authorized local installation.
---

# Browser MCP provisioning

Core PilotDeck does not require Node. This optional plugin requires a separately installed Node runtime, a pinned Playwright MCP source checkout and a browser executable. Do not silently install software while answering a browser task.

Inspect the current MCP error, Node version and the path selected by `PILOTDECK_RUNTIME_ROOT`. A portable external runtime root can provide `node_modules/@playwright/mcp/cli.js`; the host uses that candidate when configured. Pin and record the MCP revision, its lockfile, browser version and license materials before packaging it. Follow the upstream source build instructions and local software policy; do not fetch an arbitrary release binary or disable a browser sandbox to make a smoke test pass.

After provisioning, restart the affected session, list the actual MCP tools and open a local fixture page. Verify a snapshot and one interaction before reporting availability. Authentication failures and missing browser executables require their actual diagnostics. This resource does not include a browser, Node or Playwright dependencies.
