import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test, { describe } from "node:test";
import { readFileSync, readdirSync } from "node:fs";
import type { ProviderConfig } from "../../../src/model/protocol/canonical.js";
import type { GoogleClientFactory } from "../../../src/model/providers/google/client.js";

import { parseModelConfig } from "../../../src/model/config/parseModelConfig.js";
import type { CanonicalModelRequest } from "../../../src/model/protocol/canonical.js";
import { complete, streamModel } from "../../../src/model/streaming/streamModel.js";
import { JsonlInvocationLogSink, type InvocationLogRecord, type ModelInvocationLogSink } from "../../../src/storage/invocationStorage.js";

const config = parseModelConfig({
  providers: {
    test: {
      protocol: "openai",
      url: "https://example.test/v1",
      apiKey: "test-key",
      retry: { requestMaxRetries: 0, streamMaxRetries: 0 },
      models: { "test-model": {} },
    },
  },
});

const request: CanonicalModelRequest = {
  provider: "test",
  model: "test-model",
  messages: [{ role: "user", content: [{ type: "text", text: "hello" }] }],
};

function createSink(): { staged: InvocationLogRecord[]; appended: InvocationLogRecord[]; sink: ModelInvocationLogSink } {
  const staged: InvocationLogRecord[] = [];
  const appended: InvocationLogRecord[] = [];
  return {
    staged,
    appended,
    sink: {
      stage: (record) => { staged.push(record); },
      append: async (record) => { appended.push(record); },
    },
  };
}

function invocation(sink: ModelInvocationLogSink) {
  return {
    sink,
    context: {
      workspaceId: "workspace",
      sessionId: "session",
      turnId: "turn",
      runId: "run",
      logicalCallId: "call",
      caller: "agent" as const,
    },
  };
}

test("complete stages the raw invocation before sending HTTP", async () => {
  const { staged, appended, sink } = createSink();
  let fetchCalls = 0;
  await complete(request, config, {
    invocation: invocation(sink),
    fetch: async (_input, init) => {
      fetchCalls += 1;
      assert.equal(staged.length, 1);
      assert.equal(staged[0]?.requestBody, String(init?.body));
      return new Response(JSON.stringify({ choices: [{ message: { role: "assistant", content: "ok" } }] }), { status: 200 });
    },
  });
  assert.equal(fetchCalls, 1);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(appended.length, 1);
  assert.equal(appended[0]?.outcome, "success");
  assert.equal(appended[0]?.responseComplete, true);
});

test("a staging failure prevents provider transmission", async () => {
  let fetchCalls = 0;
  await assert.rejects(
    complete(request, config, {
      invocation: invocation({
        stage: () => { throw new Error("storage unavailable"); },
        append: async () => {},
      }),
      fetch: async () => {
        fetchCalls += 1;
        throw new Error("provider must not be called");
      },
    }),
    /storage unavailable/,
  );
  assert.equal(fetchCalls, 0);
});

test("complete records provider HTTP failures without marking the response complete", async () => {
  const { appended, sink } = createSink();
  await assert.rejects(
    complete(request, config, {
      invocation: invocation(sink),
      fetch: async () => new Response(JSON.stringify({ error: { message: "provider rejected" } }), { status: 503 }),
    }),
    /provider rejected|503/,
  );
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(appended.length, 1);
  assert.equal(appended[0]?.outcome, "provider_error");
  assert.equal(appended[0]?.httpStatus, 503);
  assert.equal(appended[0]?.responseComplete, false);
});

test("each retry attempt gets its own audit record", async () => {
  const retryConfig = parseModelConfig({
    providers: {
      test: {
        protocol: "openai",
        url: "https://example.test/v1",
        apiKey: "test-key",
        retry: { requestMaxRetries: 1, streamMaxRetries: 0, baseDelayMs: 1 },
        models: { "test-model": {} },
      },
    },
  });
  const { staged, appended, sink } = createSink();
  let fetchCalls = 0;
  await complete(request, retryConfig, {
    invocation: invocation(sink),
    fetch: async () => {
      fetchCalls += 1;
      if (fetchCalls === 1) throw new Error("fetch failed");
      return new Response(JSON.stringify({ choices: [{ message: { role: "assistant", content: "ok" } }] }), { status: 200 });
    },
  });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(staged.length, 2);
  assert.deepEqual(staged.map((record) => record.attempt), [1, 2]);
  assert.equal(appended.length, 2);
  assert.equal(appended[0]?.outcome, "transport_error");
  assert.equal(appended[1]?.outcome, "success");
});

test("streamModel records the complete raw response", async () => {
  const { staged, appended, sink } = createSink();
  const events = [];
  for await (const event of streamModel(request, config, {
    invocation: invocation(sink),
    fetch: async (_input, init) => {
      assert.equal(staged.length, 1);
      assert.equal(staged[0]?.requestBody, String(init?.body));
      return new Response('data: {"choices":[{"delta":{"content":"ok"}}]}\n\ndata: [DONE]\n\n', {
        status: 200,
        headers: { "content-type": "text/event-stream" },
      });
    },
  })) {
    events.push(event);
  }
  assert.equal(events.some((event) => event.type === "text_delta" && event.text === "ok"), true);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(appended.length, 1);
  assert.equal(appended[0]?.responseComplete, true);
  assert.equal(appended[0]?.responseBody?.includes("[DONE]"), true);
});

test("streamModel records an incomplete provider stream before returning its error event", async () => {
  const incompleteConfig = parseModelConfig({
    providers: {
      test: {
        protocol: "openai",
        url: "https://example.test/v1",
        apiKey: "test-key",
        retry: { requestMaxRetries: 0, streamMaxRetries: 0, baseDelayMs: 1 },
        models: { "test-model": {} },
      },
    },
  });
  const { appended, sink } = createSink();
  const events = [];
  for await (const event of streamModel(request, incompleteConfig, {
    invocation: invocation(sink),
    fetch: async () => new Response('data: {"choices":[{"delta":{"content":"partial"}}]}\n\n', {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    }),
  })) {
    events.push(event);
  }
  await new Promise((resolve) => setImmediate(resolve));
  assert.ok(events.some((event) => event.type === "error"));
  assert.equal(appended.length, 1);
  assert.equal(appended[0]?.outcome, "incomplete");
  assert.equal(appended[0]?.responseComplete, false);
});

test("streamModel audits an incomplete stream retry as separate attempts", async () => {
  const retryConfig = parseModelConfig({
    providers: {
      test: {
        protocol: "openai",
        url: "https://example.test/v1",
        apiKey: "test-key",
        retry: { requestMaxRetries: 0, streamMaxRetries: 1, baseDelayMs: 1 },
        models: { "test-model": {} },
      },
    },
  });
  const { staged, appended, sink } = createSink();
  let fetchCalls = 0;
  const events = [];
  for await (const event of streamModel(request, retryConfig, {
    invocation: invocation(sink),
    fetch: async () => {
      fetchCalls += 1;
      if (fetchCalls === 1) {
        return new Response('data: {"choices":[{"delta":{"content":"partial"}}]}\n\n', {
          status: 200,
          headers: { "content-type": "text/event-stream" },
        });
      }
      return new Response('data: {"choices":[{"delta":{"content":"ok"}}]}\n\ndata: [DONE]\n\n', {
        status: 200,
        headers: { "content-type": "text/event-stream" },
      });
    },
  })) {
    events.push(event);
  }
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(fetchCalls, 2);
  assert.deepEqual(staged.map((record) => record.attempt), [1, 2]);
  assert.deepEqual(appended.map((record) => record.outcome), ["incomplete", "success"]);
  assert.equal(appended[0]?.responseComplete, false);
  assert.equal(appended[1]?.responseComplete, true);
  assert.equal(events.some((event) => event.type === "text_delta" && event.text === "ok"), true);
});

test("complete preserves subagent invocation provenance in the audit record", async () => {
  const { appended, sink } = createSink();
  await complete(request, config, {
    invocation: {
      sink,
      context: {
        workspaceId: "workspace",
        sessionId: "parent-session",
        subSessionId: "child-session",
        turnId: "child-turn",
        runId: "child-run",
        logicalCallId: "child-call",
        caller: "subagent",
        parentToolCallId: "parent-tool-call",
      },
    },
    fetch: async () => new Response(JSON.stringify({ choices: [{ message: { role: "assistant", content: "child" } }] }), { status: 200 }),
  });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(appended.length, 1);
  assert.equal(appended[0]?.caller, "subagent");
  assert.equal(appended[0]?.subSessionId, "child-session");
  assert.equal(appended[0]?.parentToolCallId, "parent-tool-call");
  assert.equal(appended[0]?.runId, "child-run");
});

test("invocation audit finalization failures are visible without changing the provider result", async () => {
  const errors: unknown[][] = [];
  const originalError = console.error;
  console.error = (...args: unknown[]) => { errors.push(args); };
  try {
    const result = await complete(request, config, {
      invocation: invocation({
        stage: () => {},
        append: async () => { throw new Error("audit append failed"); },
      }),
      fetch: async () => new Response(JSON.stringify({ choices: [{ message: { role: "assistant", content: "ok" } }] }), { status: 200 }),
    });
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(result.content[0]?.type, "text");
    assert.equal(result.content[0]?.text, "ok");
    assert.equal(errors.length, 1);
    assert.match(String(errors[0]?.[0]), /failed to persist model invocation audit/);
    assert.match(String(errors[0]?.[0]), /requestLogId=/);
    assert.equal(errors[0]?.some((value) => String(value).includes("audit append failed")), true);
  } finally {
    console.error = originalError;
  }
});

test("JsonlInvocationLogSink persists staged records under Gateway-owned scope", async () => {
  const root = await mkdtemp(join(tmpdir(), "pilotdeck-invocation-audit-"));
  try {
    const sink = new JsonlInvocationLogSink({ root });
    const record: InvocationLogRecord = {
      ...invocation(sink).context,
      requestLogId: "request-log",
      requestId: "request",
      attempt: 1,
      provider: "test",
      protocol: "openai",
      model: "test-model",
      stream: false,
      requestBody: "{}",
      responseBody: "{}",
      requestBytes: 2,
      responseBytes: 2,
      outcome: "success",
      responseComplete: true,
      startedAt: new Date(0).toISOString(),
      completedAt: new Date(0).toISOString(),
    };
    sink.stage(record);
    await sink.append(record);
    const path = join(root, "workspaces", "workspace", "sessions", "session", "llm", "invocations.jsonl");
    assert.deepEqual(JSON.parse(await readFile(path, "utf8")), record);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

describe("session sandbox invocation regressions", () => {
  const config = parseModelConfig({
    providers: {
      test: {
        protocol: "openai",
        url: "https://example.test/v1",
        apiKey: "test-key",
        retry: { requestMaxRetries: 0, streamMaxRetries: 0 },
        models: { "test-model": {} },
      },
    },
  });

  const request: CanonicalModelRequest = {
    provider: "test",
    model: "test-model",
    messages: [{ role: "user", content: [{ type: "text", text: "hello" }] }],
  };

  const googleConfig = parseModelConfig({
    providers: {
      google: {
        protocol: "google",
        url: "https://generativelanguage.googleapis.com/v1beta",
        apiKey: "test-key",
        retry: { requestMaxRetries: 0, streamMaxRetries: 0 },
        models: { "test-model": {} },
      },
    },
  });

  const googleRequest: CanonicalModelRequest = {
    ...request,
    provider: "google",
  };

  function assertRequestWasStaged(root: string, body: BodyInit | null | undefined): void {
    const pendingDir = join(root, "workspaces", "workspace", "sessions", "session", "llm", ".pending");
    const pendingFiles = readdirSync(pendingDir);
    assert.equal(pendingFiles.length, 1);
    const staged = JSON.parse(readFileSync(join(pendingDir, pendingFiles[0]!), "utf8")) as {
      requestBody: string;
    };
    assert.equal(staged.requestBody, String(body));
  }

  function splitUtf8Response(payload: string, character: string): Response {
    const bytes = Buffer.from(payload, "utf8");
    const characterBytes = Buffer.from(character, "utf8");
    const characterOffset = bytes.indexOf(characterBytes);
    assert.notEqual(characterOffset, -1);
    const splitOffset = characterOffset + characterBytes.byteLength - 1;

    return new Response(new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(bytes.subarray(0, splitOffset));
        controller.enqueue(bytes.subarray(splitOffset));
        controller.close();
      },
    }), {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    });
  }

  function captureInvocationRecords(): {
    records: InvocationLogRecord[];
    sink: ModelInvocationLogSink;
  } {
    const records: InvocationLogRecord[] = [];
    return {
      records,
      sink: {
        stage: () => {},
        append: async (record) => { records.push(record); },
      },
    };
  }

  function invocationContext() {
    return {
      workspaceId: "workspace",
      sessionId: "session",
      turnId: "turn",
      runId: "turn",
      logicalCallId: "call",
      caller: "agent" as const,
    };
  }

  async function waitForPersistedInvocation(
    root: string,
    sessionId: string,
  ): Promise<{ line: string; record: InvocationLogRecord }> {
    const path = join(root, "workspaces", "workspace", "sessions", sessionId, "llm", "invocations.jsonl");
    for (let attempt = 0; attempt < 100; attempt += 1) {
      try {
        const content = await readFile(path, "utf8");
        const line = content.split("\n").find(Boolean);
        if (line) {
          return { line, record: JSON.parse(line) as InvocationLogRecord };
        }
      } catch {
        // Invocation persistence is asynchronous; retry until the file is visible.
      }
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
    throw new Error(`Timed out waiting for invocation log: ${path}`);
  }

  test("complete synchronously stages the invocation before sending HTTP", async (t) => {
    const root = await mkdtemp(join(tmpdir(), "pilotdeck-stage-barrier-"));
    t.after(() => rm(root, { recursive: true, force: true }));
    let fetchCalls = 0;
    await complete(request, config, {
      invocation: {
        context: {
          workspaceId: "workspace",
          sessionId: "session",
          turnId: "turn",
          runId: "turn",
          logicalCallId: "call",
          caller: "agent",
        },
        sink: new JsonlInvocationLogSink({ root }),
      },
      fetch: async (_input, init) => {
        fetchCalls++;
        assertRequestWasStaged(root, init?.body);
        return new Response(JSON.stringify({
          choices: [{ message: { role: "assistant", content: "ok" } }],
        }), { status: 200 });
      },
    });

    assert.equal(fetchCalls, 1);
  });

  test("streamModel synchronously stages the invocation before sending HTTP", async (t) => {
    const root = await mkdtemp(join(tmpdir(), "pilotdeck-stage-barrier-"));
    t.after(() => rm(root, { recursive: true, force: true }));
    let fetchCalls = 0;
    const stream = streamModel(request, config, {
      invocation: {
        context: {
          workspaceId: "workspace",
          sessionId: "session",
          turnId: "turn",
          runId: "turn",
          logicalCallId: "call",
          caller: "agent",
        },
        sink: new JsonlInvocationLogSink({ root }),
      },
      fetch: async (_input, init) => {
        fetchCalls++;
        assertRequestWasStaged(root, init?.body);
        return new Response("data: [DONE]\n\n", {
          status: 200,
          headers: { "content-type": "text/event-stream" },
        });
      },
    });

    const iterator = stream[Symbol.asyncIterator]();
    const firstEvent = await iterator.next();
    assert.equal(firstEvent.value?.type, "request_started");
    await iterator.next();
    while (!(await iterator.next()).done) {
      // Drain the stream so the invocation is finalized.
    }
    assert.equal(fetchCalls, 1);
  });

  test("streamModel preserves UTF-8 response bytes split across chunks", async () => {
    const payload = 'data: {"choices":[{"delta":{"content":"中"}}]}\n\ndata: [DONE]\n\n';
    const { records, sink } = captureInvocationRecords();
    const events = [];

    for await (const event of streamModel(request, config, {
      invocation: { context: invocationContext(), sink },
      fetch: async () => splitUtf8Response(payload, "中"),
    })) {
      events.push(event);
    }

    assert.equal(events.some((event) => event.type === "text_delta" && event.text === "中"), true);
    assert.equal(records.length, 1);
    assert.equal(records[0]?.responseBody, payload);
    assert.equal(records[0]?.responseBody?.includes("\uFFFD"), false);
    assert.equal(records[0]?.responseBytes, Buffer.byteLength(payload));
    assert.equal(records[0]?.outcome, "success");
    assert.equal(records[0]?.responseComplete, true);
  });

  test("streamModel preserves completed UTF-8 characters in an interrupted response", async () => {
    const payload = 'data: {"choices":[{"delta":{"content":"中"}}]}\n\n';
    const { records, sink } = captureInvocationRecords();

    for await (const _event of streamModel(request, config, {
      invocation: { context: invocationContext(), sink },
      fetch: async () => splitUtf8Response(payload, "中"),
    })) {
      // Drain the incomplete stream so the invocation is finalized.
    }

    assert.equal(records.length, 1);
    assert.equal(records[0]?.responseBody, payload);
    assert.equal(records[0]?.responseBody?.includes("\uFFFD"), false);
    assert.equal(records[0]?.responseBytes, Buffer.byteLength(payload));
    assert.equal(records[0]?.outcome, "incomplete");
    assert.equal(records[0]?.responseComplete, false);
  });

  test("JsonlInvocationLogSink round-trips split UTF-8 and provider unicode escapes", async (t) => {
    const root = await mkdtemp(join(tmpdir(), "pilotdeck-utf8-jsonl-"));
    t.after(() => rm(root, { recursive: true, force: true }));
    const sink = new JsonlInvocationLogSink({ root });
    const cases = [
      {
        sessionId: "literal-utf8",
        payload: 'data: {"choices":[{"delta":{"content":"中"}}]}\n\ndata: [DONE]\n\n',
        splitMarker: "中",
        serializedMarker: "中",
      },
      {
        sessionId: "provider-unicode-escape",
        payload: 'data: {"choices":[{"delta":{"content":"\\u4e2d"}}]}\n\ndata: [DONE]\n\n',
        splitMarker: "\\u4e2d",
        serializedMarker: "\\\\u4e2d",
      },
    ];

    for (const item of cases) {
      for await (const _event of streamModel(request, config, {
        invocation: {
          context: { ...invocationContext(), sessionId: item.sessionId },
          sink,
        },
        fetch: async () => splitUtf8Response(item.payload, item.splitMarker),
      })) {
        // Drain the stream so the invocation is persisted.
      }

      const { line, record } = await waitForPersistedInvocation(root, item.sessionId);
      assert.equal(line.includes(item.serializedMarker), true);
      assert.equal(record.responseBody, item.payload);
      assert.equal(Buffer.from(record.responseBody ?? "").equals(Buffer.from(item.payload)), true);
      assert.equal(record.responseBody?.includes("\uFFFD"), false);
      assert.equal(record.responseBytes, Buffer.byteLength(item.payload));
    }
  });

  test("does not send HTTP when synchronous invocation staging fails", async () => {
    let fetchCalls = 0;
    await assert.rejects(
      complete(request, config, {
        invocation: {
          context: {
            workspaceId: "workspace",
            sessionId: "session",
            turnId: "turn",
            runId: "turn",
            logicalCallId: "call",
            caller: "agent",
          },
          sink: {
            stage: () => { throw new Error("storage unavailable"); },
            append: async () => {},
          },
        },
        fetch: async () => {
          fetchCalls++;
          throw new Error("HTTP must not be sent");
        },
      }),
      /storage unavailable/,
    );
    assert.equal(fetchCalls, 0);
  });

  test("Google complete stages synchronously before invoking the SDK", async () => {
    const order: string[] = [];
    const googleClientFactory: GoogleClientFactory = (_provider: ProviderConfig) => ({
      models: {
        generateContent: async () => {
          order.push("sdk");
          return { candidates: [{ content: { parts: [{ text: "ok" }] } }] } as never;
        },
        generateContentStream: async () => (async function* () {})(),
      },
    });

    await complete(googleRequest, googleConfig, {
      invocation: {
        context: {
          workspaceId: "workspace",
          sessionId: "session",
          turnId: "turn",
          runId: "turn",
          logicalCallId: "call",
          caller: "agent",
        },
        sink: {
          stage: () => { order.push("stage"); },
          append: async () => {},
        },
      },
      googleClientFactory,
    });

    assert.deepEqual(order, ["stage", "sdk"]);
  });

  test("Google streaming stages synchronously before invoking the SDK", async () => {
    const order: string[] = [];
    const googleClientFactory: GoogleClientFactory = (_provider: ProviderConfig) => ({
      models: {
        generateContent: async () => ({} as never),
        generateContentStream: async () => {
          order.push("sdk");
          return (async function* () {
            yield { candidates: [{ content: { parts: [{ text: "ok" }] } }] } as never;
            yield { candidates: [{ finishReason: "STOP", content: { parts: [] } }] } as never;
          })();
        },
      },
    });

    const iterator = streamModel(googleRequest, googleConfig, {
      invocation: {
        context: {
          workspaceId: "workspace",
          sessionId: "session",
          turnId: "turn",
          runId: "turn",
          logicalCallId: "call",
          caller: "agent",
        },
        sink: {
          stage: () => { order.push("stage"); },
          append: async () => {},
        },
      },
      googleClientFactory,
    })[Symbol.asyncIterator]();

    while (!(await iterator.next()).done) {
      // Drain the SDK stream.
    }
    assert.deepEqual(order, ["stage", "sdk"]);
  });
});
