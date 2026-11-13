# SDK signal abort order: portable sealed-CLI runner

This fixture runs the actual sealed Rust CLI against a precise local WebSocket Gateway mock. The child receives a real SIGINT. It never reads the real product home, never uses a live account/provider and never binds fixed port 18789. This is protocol/lifecycle evidence, not durable real-Gateway recovery evidence.

## Source identity

The files migrated back from the actually executed `_tmp/driver-r2` are byte-identical:

| File | SHA-256 |
|---|---|
| driver.mjs | 6b29ef61cf58a1403dbfc15e55f057c65ee8b1ef0afa3272f48a7a41d2d8926a |
| verify.mjs | e4e2d422e7b96d2b67219fec81510edb329e0566b308752d95add440ef1edc87 |

No Rust rebuild is required merely to move these runner files. Every platform must execute its own sealed CLI and bind its receipt to that binary's actual SHA-256. The macOS result for binary `dcf56515fa38d4b3cababbdbb204a7f47e739544fb69a5ebe943ad202dfe5ae6` cannot serve as a Linux receipt.

## Prerequisites and command (Linux/E0)

Wait for E0's assigned light slot and stable heavy queue; this document does not start or authorize a competing build. Use the already available Node/baseline source dependencies. No dependency installation, download, account, fixed port or shared binary replacement is needed.

All three environment paths below must be absolute. Choose a new output directory which has never existed; the driver does not overwrite-protect an operator-reused directory. Use the exact sealed Linux artifact recorded by E0, never an arbitrary current target/debug binary.

```sh
task_wire_root="$PWD"
task_wire_binary="/absolute/path/to/sealed/linux/pilotdeck"
task_wire_output="$task_wire_root/logs/sdk-signal-wire-NEW-RUN"
task_wire_baseline="$task_wire_root/baseline/PilotDeck"
test ! -e "$task_wire_output" || exit 2
task_wire_sha="$(sha256sum "$task_wire_binary" | cut -d ' ' -f 1)"
sha256sum \
  "$task_wire_root/src/pilotdeck-cli/tests/fixtures/sdk-signal-abort-order/driver.mjs" \
  "$task_wire_root/src/pilotdeck-cli/tests/fixtures/sdk-signal-abort-order/verify.mjs" \
  "$task_wire_binary" > "$task_wire_output.inputs.sha256"
H1_BASELINE="$task_wire_baseline" \
H1_DIAGNOSTIC_OUTPUT="$task_wire_output" \
H1_ISOLATED_LOCAL=1 \
H1_CLI_BIN="$task_wire_binary" \
node "$task_wire_root/src/pilotdeck-cli/tests/fixtures/sdk-signal-abort-order/driver.mjs" \
  > "$task_wire_output.driver.stdout" 2> "$task_wire_output.driver.stderr"
task_wire_driver_exit=$?
printf '%s\n' "$task_wire_driver_exit" > "$task_wire_output.driver.exit"
node "$task_wire_root/src/pilotdeck-cli/tests/fixtures/sdk-signal-abort-order/verify.mjs" \
  "$task_wire_output" "$task_wire_sha" \
  > "$task_wire_output.verifier.stdout" 2> "$task_wire_output.verifier.stderr"
task_wire_verifier_exit=$?
printf '%s\n' "$task_wire_verifier_exit" > "$task_wire_output.verifier.exit"
sha256sum "$task_wire_binary" > "$task_wire_output.binary-after.sha256"
```

The script flags are intentionally named for the existing driver contract. `H1_ISOLATED_LOCAL=1` means an explicitly isolated run on that host; it does not assert macOS. The driver constructs each product HOME/PILOT_HOME/project under the new output directory and sets those on every CLI child. Only ephemeral loopback WS ports are used.

Do not set H1_CASES for the canonical receipt: it must contain all eight exact cases in order. The abort-timeout case takes the product's original 30-second request deadline; only its diagnostic safety bound is 40 seconds. Other child safety bounds remain 12 seconds. The per-WebSocket natural-close observation bound is one second. Any diagnostic child kill, close timeout or forced socket cleanup is a failure, never a successful release.

## Manifest and receipt requirements

Save before/after identity separately from the runner's own identity.json. E0's manifest must bind:

- Exact product source commit and sealed build identity, platform/architecture, binary absolute path and SHA-256 before and after execution.
- The two exact runner hashes above, Node absolute executable/version/hash and actual fixed baseline commit.
- Baseline compiled CLI/SDK hashes (also recorded in identity.json), and actual resolved `ws` package version/path plus package.json, lib/websocket.js and lib/websocket-server.js SHA-256. The driver resolves baseline `ws`, falling back to the first matching baseline .pnpm/ws@ source directory; record the dependency actually selected.
- Fresh output path, actual command/environment, driver/verifier exit files, the exact eight cases and raw artifact hashes. Never substitute a previous run directory or a receipt from another binary/platform.

Successful verifier stdout is exactly the JSON object `{status:"PASS",binary:<this Linux binary SHA>,cases:[eight IDs]}`. Preserve it as the receipt for the private named policy. The comparator owner must validate/load this original output and manifest; a hand-written PASS object is not execution evidence.

The eight cases are: final-and-abort-batch, final-and-abort-next-turn, final-and-abort-separated, wrong-run-response, malformed-response, abort-rpc-error, connection-drop, abort-timeout. The first four require exact canonical abort/exit1, empty stderr, actual current-run abort acknowledgement and both naturally released sockets. The failure cases require the exact RPC/transport/deadline error and prohibit a successful aborted footer.

## Actual macOS evidence and first failures

The first failed run and corrected second run remain under `_tmp/20261113-sdk-signal-policy/`. First failure was a diagnostic safety deadline shorter than the actual product request deadline, plus recording WS close via the HTTP-server close callback instead of each WS close promise. It is not erased or called product PASS.

The corrected second run on the sealed 2b3568bc CLI passed all eight cases. A separate copied artifact with a legal-sequence forced_socket_cleanup record was rejected with exit1, proving forced cleanup cannot wash a failure into success. No original PASS artifacts were mutated. See `docs/backend-delivery/D1-sdk-signal-abort-order.md` for exact identities and evidence paths.
