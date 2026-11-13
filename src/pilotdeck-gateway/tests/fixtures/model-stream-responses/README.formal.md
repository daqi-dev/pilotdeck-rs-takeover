# Responses formal Gateway probe

This separate formal probe complements the existing module-level `README.md` and its recorded failure/cancellation cases. It uses an isolated `HOME`, `PILOT_HOME`, project and `TMPDIR`; only a loopback port chosen by the OS; a fake `fixture-only` provider credential; and no external service or account. The full CLI server starts, an authenticated WS `submit_turn` reaches the real model transport, the mock returns Responses SSE calling the real `read_file`, and a second Responses request must carry its exact `function_call_output` before the mock sends the final text. The checker binds call IDs, run IDs, event and durable history ordering, timestamps, request path/auth/body, SSE and both usage records. It retains raw provider, gateway and server logs. It does not test response failure/cancellation; preserve the pre-existing module golden evidence for those boundaries.

Use a **new, nonexistent** output directory per execution. Set `PDRS_BASELINE` to clean fixed `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` with its fixed dependencies. `M1_BINARY` may name either the fixed built-TS exec wrapper or the sealed Rust CLI. When it is set, `J7_IMPLEMENTATION=ts|rust` is required; the runner must independently bind the declared implementation to the actual executable/source identity. Forward all three variables explicitly into an isolated runner/container. Without `M1_BINARY`, the source-TS entry defaults to `ts`.

The fixture uses the unbound General session, so its input is created in the isolated `PILOT_HOME` workspace. Process cwd alone does not bind a project. `sourceSha256` is the guard's full path-to-SHA256 closure map; capture and identity are compared structurally after independent JSON parsing.

```sh
PDRS_BASELINE=/absolute/fixed/baseline node formal.mjs /absolute/new-ts-output
PDRS_BASELINE=/absolute/fixed/baseline M1_BINARY=/absolute/fixed/ts-exec-wrapper J7_IMPLEMENTATION=ts node formal.mjs /absolute/new-built-ts-output
PDRS_BASELINE=/absolute/fixed/baseline M1_BINARY=/absolute/sealed/pilotdeck J7_IMPLEMENTATION=rust node formal.mjs /absolute/new-rust-output
node verify.mjs /absolute/new-ts-output /absolute/new-rust-output
```

Before a product pair, capture the first TS run without overwriting failures, then require two TS captures and run `verify.mjs` on each and as a pair. The checker compares only the enumerated stable business contract; it separately validates the actual request body, response frame ordering and same-run IDs/timestamps in each raw capture. No wildcard ignores are added. The captures contain mock payloads and generated isolated paths, not credentials from a user.

For an actual negative control set `J7_WRONG_TOOL_OUTPUT=1` on a fresh TS invocation: the real fixture file is changed, so real `read_file` returns a wrong value. The same unchanged mock rejects the subsequent `function_call_output` and the unchanged positive checker must reject the capture. This is distinct from synthetic capture mutations. Preserve its raw logs and do not count it as a positive run.

`formal.mjs`/`verify.mjs` are newly authored, with syntax and synthetic checker controls only until an actual capture is obtained. The mock's exact protocol assumptions, title background request behavior, usage accumulation and history projections must be calibrated by the first fixed-TS run; a timeout, unexpected request, or strict mismatch is retained, not relabeled as product failure or silently loosened.
