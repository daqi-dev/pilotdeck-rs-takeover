#!/usr/bin/env python3
"""Relocate a verified package and exercise its native CLI without Node or TS."""
from __future__ import annotations

import argparse
import base64
import hashlib
import http.server
import json
import os
import shutil
import signal
import socket
import struct
import subprocess
import threading
import time
import zlib
from common import Path, digest, write_json
from verify import verify


class WebSocket:
    def __init__(self, port):
        self.socket = socket.create_connection(("127.0.0.1", port), 10)
        self.socket.settimeout(30)
        self.buffer = bytearray()
        key = base64.b64encode(os.urandom(16)).decode()
        self.socket.sendall((f"GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
        while b"\r\n\r\n" not in self.buffer:
            self.buffer.extend(self.socket.recv(65536))
        headers, rest = bytes(self.buffer).split(b"\r\n\r\n", 1)
        self.buffer = bytearray(rest)
        expected = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
        assert headers.startswith(b"HTTP/1.1 101 ") and expected.lower() in headers.lower(), headers

    def read(self, count):
        while len(self.buffer) < count:
            chunk = self.socket.recv(65536)
            if not chunk:
                raise EOFError("WebSocket closed before terminal")
            self.buffer.extend(chunk)
        result = bytes(self.buffer[:count])
        del self.buffer[:count]
        return result

    def send_frame(self, opcode, data):
        mask = os.urandom(4)
        length = len(data)
        header = bytes([0x80 | opcode, 0x80 | (length if length < 126 else 126 if length < 65536 else 127)])
        if length >= 126:
            header += struct.pack("!H" if length < 65536 else "!Q", length)
        self.socket.sendall(header + mask + bytes(value ^ mask[index % 4] for index, value in enumerate(data)))

    def send(self, value):
        self.send_frame(1, json.dumps(value).encode())

    def receive(self):
        message = bytearray()
        while True:
            first, second = self.read(2)
            assert not second & 0x80, "server frames must be unmasked"
            length = second & 0x7F
            if length == 126:
                length = struct.unpack("!H", self.read(2))[0]
            elif length == 127:
                length = struct.unpack("!Q", self.read(8))[0]
            assert length < 16 * 1024 * 1024, "unexpectedly large smoke response"
            payload = self.read(length)
            opcode = first & 15
            if opcode == 9:
                self.send_frame(10, payload)
                continue
            if opcode == 8:
                raise EOFError("WebSocket closed before terminal")
            assert opcode in (0, 1), f"unexpected websocket opcode {opcode}"
            message.extend(payload)
            if first & 0x80:
                return json.loads(message)

    def close(self):
        try:
            self.send_frame(8, struct.pack("!H", 1000))
        finally:
            self.socket.close()


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    assert port != 18789
    return port


def fixtures(project):
    def chunk(kind, content):
        return struct.pack("!I", len(content)) + kind + content + struct.pack("!I", zlib.crc32(kind + content))
    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack("!2I5B", 1, 1, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(b"\x00\x20\x80\xd0")) + chunk(b"IEND", b"")
    (project / "release.png").write_bytes(png)
    text = b"BT /F1 12 Tf 20 40 Td (RELEASE_PDF_OK) Tj ET\n"
    objects = [b"<< /Type /Catalog /Pages 2 0 R >>", b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
               b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
               b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
               b"<< /Length " + str(len(text)).encode() + b" >>\nstream\n" + text + b"endstream"]
    pdf, offsets = bytearray(b"%PDF-1.4\n"), [0]
    for index, value in enumerate(objects, 1):
        offsets.append(len(pdf))
        pdf.extend(f"{index} 0 obj\n".encode() + value + b"\nendobj\n")
    start = len(pdf)
    pdf.extend(f"xref\n0 {len(offsets)}\n0000000000 65535 f \n".encode())
    for offset in offsets[1:]:
        pdf.extend(f"{offset:010d} 00000 n \n".encode())
    pdf.extend(f"trailer\n<< /Size {len(offsets)} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n".encode())
    (project / "release.pdf").write_bytes(pdf)



def png_pixel(data):
    """Decode the smoke fixture's exact one-pixel RGB PNG contract."""
    assert data.startswith(b"\x89PNG\r\n\x1a\n"), "provider image is not PNG"
    offset, pixels, header, ended = 8, bytearray(), None, False
    while offset < len(data):
        assert offset + 12 <= len(data), "truncated PNG chunk"
        length = struct.unpack("!I", data[offset:offset + 4])[0]
        kind = data[offset + 4:offset + 8]
        end = offset + 12 + length
        assert end <= len(data), "truncated PNG payload"
        content = data[offset + 8:offset + 8 + length]
        crc = struct.unpack("!I", data[end - 4:end])[0]
        assert crc == zlib.crc32(kind + content), "PNG checksum differs"
        if kind == b"IHDR":
            assert header is None, "duplicate PNG header"
            header = struct.unpack("!2I5B", content)
        elif kind == b"IDAT":
            pixels.extend(content)
        elif kind == b"IEND":
            assert not content and end == len(data), "PNG has trailing data"
            ended = True
        offset = end
    assert ended and header == (1, 1, 8, 2, 0, 0, 0), "unexpected PNG dimensions/format"
    row = zlib.decompress(pixels)
    # For the first and only RGB pixel all left/above predictors are zero.
    assert len(row) == 4 and row[0] in range(5), "invalid PNG scanline"
    return row[1:]


def provider_media(records, workspace):
    media = []
    for request in records:
        for message in request["body"].get("messages", []):
            content = message.get("content")
            if message.get("role") != "user" or not isinstance(content, list):
                continue
            for block in content:
                if block.get("type") == "image_url":
                    url = block["image_url"]["url"]
                    prefix, encoded = url.split(",", 1)
                    assert prefix in {"data:application/pdf;base64", "data:image/jpeg;base64"}, "unexpected provider media type"
                    media.append((prefix, base64.b64decode(encoded, validate=True)))
    pdfs = [data for prefix, data in media if prefix == "data:application/pdf;base64"]
    images = [data for prefix, data in media if prefix == "data:image/jpeg;base64"]
    assert pdfs, "bundled PDF component did not supply provider PDF block"
    assert images, "bundled image component did not supply provider JPEG block"
    expected_pdf = (workspace / "release.pdf").read_bytes()
    assert b"RELEASE_PDF_OK" in expected_pdf
    assert all(data == expected_pdf for data in pdfs), "provider PDF bytes differ from fixture"
    input_png = (workspace / "release.png").read_bytes()
    assert png_pixel(input_png) == b"\x20\x80\xd0"
    golden_file = Path(__file__).resolve().parent / "fixtures/read-file-rgb1x1-jpeg.json"
    golden = json.loads(golden_file.read_text())
    assert hashlib.sha256(input_png).hexdigest() == golden["inputSha256"], "image fixture differs from fixed TS golden input"
    expected_image = base64.b64decode(golden["outputBase64"], validate=True)
    assert golden["outputMime"] == "image/jpeg"
    assert len(expected_image) == golden["outputBytes"] and hashlib.sha256(expected_image).hexdigest() == golden["outputSha256"]
    assert all(data == expected_image for data in images), "provider JPEG bytes differ from fixed TS read_file golden"
    return {"pdfFixtureSha256": hashlib.sha256(expected_pdf).hexdigest(), "pdfBlocks": len(pdfs),
            "pdfBytesExactlyMatchFixture": True, "imageBlocks": len(images), "inputPngRGB": [32, 128, 208],
            "imageInputFixtureSha256": golden["inputSha256"], "imageOutputMime": golden["outputMime"],
            "imageOutputSha256": golden["outputSha256"], "imageOutputBytes": golden["outputBytes"],
            "imageGoldenSha256": digest(golden_file), "providerImageBytesMatchFixedTs": True}



def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    source = args.package.resolve(strict=True)
    verified = verify(source)
    manifest = json.loads((source / "manifest.json").read_text())
    if args.out.exists():
        parser.error("output directory must be new")
    args.out.mkdir(parents=True)
    out = args.out.resolve()
    # Moving copied executables, not wrappers that still point into the checkout.
    package = out / "relocated/backend"
    shutil.copytree(source, package, symlinks=True)
    verify(package)
    home, project, guards = out / "home", out / "project", out / "guard-bin"
    for directory in [home, project, guards]:
        directory.mkdir()
    workspace = home / ".pilotdeck"
    workspace.mkdir()
    (workspace / "release-check.txt").write_text("RELEASE_NATIVE_READ_OK\n")
    fixtures(workspace)
    attempts = out / "forbidden-runtime-attempts.txt"
    env = {"HOME": str(home), "PILOT_HOME": str(home / ".pilotdeck"), "PATH": f"{guards}:{package / 'bin'}",
           "TMPDIR": str(out / "tmp"), "PILOTDECK_GATEWAY_TOKEN": "release-smoke-token", "DOCKER_CONTAINER": "1",
           "PDRS_FORBIDDEN_RUNTIME_LOG": str(attempts), "LANG": "C.UTF-8"}
    Path(env["TMPDIR"]).mkdir()
    sentinel = '#!/bin/sh\nprintf "%s\\n" "$0" >> "$PDRS_FORBIDDEN_RUNTIME_LOG"\nexit 93\n'
    for name in ["node", "nodejs", "npm", "npx", "pnpm", "bun", "deno"]:
        (guards / name).write_text(sentinel)
        (guards / name).chmod(0o755)
    records, frames, model_errors = [], [], []
    requests_lock = threading.Lock()

    class Model(http.server.BaseHTTPRequestHandler):
        def log_message(self, *unused):
            pass

        def do_POST(self):
            try:
                body = json.loads(self.rfile.read(int(self.headers["content-length"])))
                with requests_lock:
                    records.append({"path": self.path, "body": body})
                assert self.path == "/v1/chat/completions", self.path
                text = json.dumps(body.get("messages", []))
                title = "Generate a concise title" in text
                has_receipt = "RELEASE_NATIVE_READ_OK" in text
                if title:
                    delta, reason = {"role": "assistant", "content": '{"title":"Release smoke"}'}, "stop"
                elif has_receipt:
                    delta, reason = {"role": "assistant", "content": "RELEASE_NATIVE_COMPLETE"}, "stop"
                else:
                    delta = {"role": "assistant", "tool_calls": [{"index": index, "id": f"release-read-{index}", "type": "function", "function": {"name": "read_file", "arguments": json.dumps({"file_path": name})}} for index, name in enumerate(["release-check.txt", "release.pdf", "release.png"])]}
                    reason = "tool_calls"
                if body.get("stream"):
                    self.send_response(200)
                    self.send_header("content-type", "text/event-stream")
                    self.end_headers()
                    chunks = [{"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                              {"choices": [{"index": 0, "delta": {}, "finish_reason": reason}], "usage": {"prompt_tokens": 8, "completion_tokens": 4, "total_tokens": 12}}]
                    for chunk in chunks:
                        self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
                    self.wfile.write(b"data: [DONE]\n\n")
                else:
                    data = json.dumps({"choices": [{"index": 0, "message": delta, "finish_reason": reason}]}).encode()
                    self.send_response(200)
                    self.send_header("content-type", "application/json")
                    self.send_header("content-length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
            except Exception as error:
                model_errors.append(str(error))
                self.send_error(500)

    model = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Model)
    threading.Thread(target=model.serve_forever, daemon=True).start()
    config = home / "pilotdeck.yaml"
    config.write_text(f"schemaVersion: 1\nagent:\n  model: mock/deterministic\n  maxTurns: 4\nmodel:\n  providers:\n    mock:\n      protocol: openai\n      url: http://127.0.0.1:{model.server_port}/v1\n      apiKey: local-mock-only\n      models:\n        deterministic: {{multimodal: {{input: [text, image, pdf]}}}}\ntelemetry:\n  enabled: false\n")
    env["PILOTDECK_CONFIG_PATH"] = str(config)
    sidecar = package / "bin/pilotdeck-agent-loop-sidecar"
    version = subprocess.run([str(sidecar), "--version"], cwd=project, env=env, capture_output=True, timeout=10)
    assert version.returncode == 0, version.stderr.decode(errors="replace")
    (out / "sidecar-version.txt").write_bytes(version.stdout + version.stderr)
    # The main smoke must succeed even when starting any separate loop is fatal.
    original_sidecar = sidecar.read_bytes()
    sidecar.write_text(sentinel)
    sidecar.chmod(0o755)
    port, child, ws = free_port(), None, None
    evidence = {"schema": 1, "package": verified, "passed": False, "nodeRuntimeAvailable": False,
                "referenceDirectoryIncluded": False, "defaultLoopProbe": "independent sidecar replaced by failing sentinel"}
    log = (out / "server.log").open("wb")
    try:
        child = subprocess.Popen([str(package / "bin/pilotdeck"), "server", "--host", "127.0.0.1", "--port", str(port)], cwd=project, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        evidence["serverPid"] = child.pid
        deadline = time.monotonic() + 45
        while True:
            assert child.poll() is None, f"server exited {child.returncode}"
            try:
                with socket.create_connection(("127.0.0.1", port), .2):
                    break
            except OSError:
                if time.monotonic() > deadline:
                    raise TimeoutError("server readiness deadline")
                time.sleep(.03)
        ws = WebSocket(port)
        ws.send({"type": "hello", "protocolVersion": "1.2", "clientName": "release-smoke", "clientVersion": "0.0.0", "token": env["PILOTDECK_GATEWAY_TOKEN"]})
        assert ws.receive()["type"] == "hello_ok"
        ws.send({"type": "request", "id": "release-turn", "method": "submit_turn", "params": {"sessionKey": "release-session", "channelKey": "test", "runId": "release-run", "message": "Read release-check.txt and confirm the result."}})
        while True:
            frame = ws.receive()
            frames.append(frame)
            if frame.get("id") == "release-turn" and (frame.get("final") or frame.get("type") == "response"):
                assert frame.get("event", {}).get("finishReason") == "completed", frame
                break
        assert any(f.get("event", {}).get("text") == "RELEASE_NATIVE_COMPLETE" for f in frames), frames
        assert any("RELEASE_NATIVE_READ_OK" in json.dumps(r["body"].get("messages")) for r in records), "tool receipt never reached provider"
        observed = json.dumps(records)
        if manifest["variant"] == "core":
            assert "PDF capability unavailable:" in observed, "missing PDF component did not report unavailable"
            assert "Image capability unavailable:" in observed, "missing image component did not report unavailable"
        else:
            evidence["nativeReadContracts"] = provider_media(records, workspace)
            assert "capability unavailable:" not in observed, "full package failed component discovery"
        assert not model_errors, model_errors
        assert not attempts.exists(), "Node or sidecar was invoked in default native mode"
        evidence["passed"] = True
    except Exception as error:
        evidence["error"] = repr(error)
    finally:
        if ws:
            ws.close()
        if child and child.poll() is None:
            child.send_signal(signal.SIGTERM)
            try:
                child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
        if child:
            evidence["exitCode"] = child.returncode
            if child.returncode != 0:
                evidence["passed"] = False
        log.close()
        model.shutdown()
        model.server_close()
        sidecar.write_bytes(original_sidecar)
        sidecar.chmod(0o755)
        evidence["packageUnchanged"] = verify(package) == verified
        evidence["manifestSha256"] = digest(source / "manifest.json")
        write_json(out / "provider.json", records)
        write_json(out / "frames.json", frames)
        write_json(out / "result.json", evidence)
    print(json.dumps(evidence, indent=2))
    if not evidence["passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
