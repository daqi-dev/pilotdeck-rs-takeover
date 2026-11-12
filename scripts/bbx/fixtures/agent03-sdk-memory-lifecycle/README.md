# AGENT03 memory and MCP lifecycle calibration

Run `scripts/bbx/scenarios/agent03-sdk-memory-lifecycle.json` with the normal isolated BBX runner. Invoke `node scripts/bbx/fixtures/agent03-sdk-memory-lifecycle/verify.mjs CAPTURE scripts/bbx/scenarios/agent03-sdk-memory-lifecycle.json` on each capture. This separate calibration has no value-policy declaration; preserve self-comparison differences for independent analysis.

The scenario seeds an isolated EdgeClaw identity and sends its real memory-gate request to the controlled model. The verifier requires the marker in the actual parent system request and its absence from the complete memory-disabled child request. The actual child MCP logs `close-held`, waits at least 900 ms, then logs `close-released` and its exit callback. The verifier requires the exit callback timestamp to precede the durable worker-completion timestamp and the parent agent result to follow that completion in the same transcript.

The first capture has not yet calibrated whether the baseline imports the seed, routes the memory gate, or closes this MCP through stdin EOF or SIGTERM. Preserve its failure if any. The timing assertion observes the child process's exit callback, not an OS wait/reap event; it does not alone prove kernel process reaping. A stronger observation requires an external process-liveness witness or driver.
