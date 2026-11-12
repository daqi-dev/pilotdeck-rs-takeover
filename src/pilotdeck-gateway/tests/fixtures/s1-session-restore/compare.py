#!/usr/bin/env python3
"""Compare two controlled TS observations; preserve business values and JSONL order."""
import json
import pathlib
import re
import sys

UUID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
ISO = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z")
NAMES = ["middle-fault", "retry-after-fault", "success-and-existing-target", "occupied-target-boundaries", "restart-read", "middle-hold", "retry-after-process-cancel"]

def projection(record):
    assert record["side"] == "ts" and record["expectation"] == "safe"
    assert [row["name"] for row in record["operations"]] == NAMES
    assert record["calls"] == []
    own = str(pathlib.Path(record["runs"][0]["env"]["HOME"]).parent)
    identifiers = {}
    def identify(match):
        value = match.group()
        if value not in identifiers:
            identifiers[value] = f"UUID-{len(identifiers) + 1}"
        return identifiers[value]
    def clean(value, key=""):
        if isinstance(value, dict):
            return {name: clean(item, name) for name, item in value.items()}
        if isinstance(value, list):
            return [clean(item, key) for item in value]
        if not isinstance(value, str):
            return value
        if key in ("bytes", "previous", "targetBytes", "finalBytes") and value.startswith('{"type":'):
            # JSON decode/encode preserves insertion order, including every
            # JSONL envelope and nested metadata/message key.
            return "\n".join(json.dumps(clean(json.loads(line)), ensure_ascii=False, separators=(",", ":")) for line in value.splitlines()) + "\n"
        if key in ("createdAt", "updatedAt"):
            assert ISO.fullmatch(value), value
            return "ISO-TIMESTAMP"
        if key == "path" or (key == "temps" and value.endswith(".tmp")):
            value = "OWNED-PATH/" + pathlib.Path(value).name
        value = re.sub(r"'" + re.escape(own) + r"/[^']*'", lambda match: "'OWNED-PATH/" + pathlib.Path(match.group()[1:-1]).name + "'", value)
        return UUID.sub(identify, value)
    return {
        "revision": record["revision"], "node": record["node"],
        "baselineSha256": record["baselineSha256"], "driverSha256": record["driverSha256"],
        "runs": [{key: row[key] for key in ("ready", "exitCode", "signal", "forceKilled")} for row in record["runs"]],
        "operations": clean(record["operations"]),
    }

first, second = [json.loads(pathlib.Path(name).read_text()) for name in sys.argv[1:]]
a, b = projection(first), projection(second)
assert a == b, "S1 observations differ after only fixture paths/UUIDs/timestamps are normalized"
print(json.dumps({"same": True, "operations": len(NAMES), "processes": len(a["runs"]), "driverSha256": a["driverSha256"], "normalization": ["owned fixture paths", "correlated UUIDs", "createdAt/updatedAt ISO timestamps"]}, indent=2))
