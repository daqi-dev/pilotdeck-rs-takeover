#!/usr/bin/env python3
"""Fake `agentcpm-pd` CLI for the compat-shim blackbox contract tests.

The shim invokes it as `agentcpm-pd [--target T] api invoke <methodId>
[--request-key K] --input-json -` with the operation input JSON on stdin.

Responses come from AGENTCPM_PD_FAKE_SCRIPT (a JSON array of
{"method", "respond"} entries; each invocation consumes the first
not-yet-consumed entry whose method matches). When that queue runs dry,
AGENTCPM_PD_FAKE_DEFAULT (a single {"method", "respond"}) is used; with
neither, a FAKE_NO_RESPONSE failure envelope is returned so an
orchestration bug surfaces instead of hanging. Every invocation appends
one JSON line {"index", "method", "requestKey", "input"} to
<AGENTCPM_PD_FAKE_DIR>/calls.jsonl — the test's call-sequence assertions
read that log.
"""
import json
import os
import sys


def main():
    argv = sys.argv[1:]
    method = None
    request_key = None
    for i, token in enumerate(argv):
        if token == "invoke" and i + 1 < len(argv):
            method = argv[i + 1]
        elif token == "--request-key" and i + 1 < len(argv):
            request_key = argv[i + 1]
    if method is None:
        sys.stderr.write("fake agentcpm-pd: expected `api invoke <methodId>`\n")
        return 2
    try:
        stdin_json = json.loads(sys.stdin.read() or "null")
    except ValueError:
        stdin_json = None
    script = json.loads(os.environ.get("AGENTCPM_PD_FAKE_SCRIPT", "[]"))
    default = json.loads(os.environ.get("AGENTCPM_PD_FAKE_DEFAULT", "null"))
    log_path = os.path.join(os.environ["AGENTCPM_PD_FAKE_DIR"], "calls.jsonl")
    consumed = set()
    if os.path.exists(log_path):
        with open(log_path, encoding="utf-8") as log:
            consumed = {json.loads(line)["index"] for line in log if line.strip()}
    index, respond = -1, None
    for idx, item in enumerate(script):
        if idx not in consumed and item.get("method") == method:
            index, respond = idx, item.get("respond")
            break
    if respond is None and default is not None and default.get("method") == method:
        respond = default.get("respond")
    with open(log_path, "a", encoding="utf-8") as log:
        log.write(json.dumps({"index": index, "method": method,
                              "requestKey": request_key,
                              "input": stdin_json}) + "\n")
    if respond is None:
        respond = {"outcome": "failed",
                   "error": {"code": "FAKE_NO_RESPONSE",
                             "message": "no scripted response for " + method}}
    if respond.get("drop"):
        sys.stderr.write(str(respond.get("stderr", "response dropped")))
        return int(respond.get("exit", 2))
    envelope = {"version": "agentcpm-pd.operation/1", "requestId": "fake",
                "outcome": respond.get("outcome", "succeeded")}
    if "result" in respond:
        envelope["result"] = respond["result"]
    if "error" in respond:
        envelope["error"] = respond["error"]
    sys.stdout.write(json.dumps(envelope))
    return 0


if __name__ == "__main__":
    sys.exit(main())
