import { createServer } from "node:http";
import { chmod, rm } from "node:fs/promises";
import type { Socket } from "node:net";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { SSEServerTransport } from "@modelcontextprotocol/sdk/server/sse.js";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";

/** A real SDK server on an unmounted, harness-owned socket; each fixture has one owner. */
export async function createSessionMcpHttpMock(socketPath: string, owner: string) {
  const requests: unknown[] = [];
  const clients = new Set<Server>();
  const sockets = new Set<Socket>();
  const sse = new Map<string, SSEServerTransport>();
  const createMcp = () => {
    const mcp = new Server({ name: `session-mock-${owner}`, version: "1.0.0" }, { capabilities: { tools: {} } });
    mcp.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: [{
      name: "session_marker", description: "Return this fixture's session marker", inputSchema: { type: "object", properties: {} },
    }] }));
    mcp.setRequestHandler(CallToolRequestSchema, async (request) => {
      if (request.params.name !== "session_marker") throw new Error("Unknown mock tool");
      return { content: [{ type: "text", text: `MCP_MARKER_${owner}` }] };
    });
    clients.add(mcp);
    return mcp;
  };
  const server = createServer(async (request, response) => {
    try {
      const url = new URL(request.url!, "http://mock.invalid");
      if (url.pathname === "/redirect") {
        response.writeHead(302, { location: url.searchParams.get("to")! }).end();
        return;
      }
      if (request.method === "GET" && url.pathname === "/sse") {
        const transport = new SSEServerTransport("/messages", response);
        sse.set(transport.sessionId, transport);
        const mcp = createMcp();
        response.once("close", () => { sse.delete(transport.sessionId); clients.delete(mcp); void mcp.close(); });
        requests.push({ owner, path: url.pathname, method: "SSE-connect" });
        await mcp.connect(transport);
        return;
      }
      if (request.method !== "POST" || !["/mcp", "/messages"].includes(url.pathname)) {
        response.writeHead(404).end();
        return;
      }
      const chunks: Buffer[] = [];
      let bytes = 0;
      for await (const chunk of request) {
        bytes += chunk.length;
        if (bytes > 1024 * 1024) throw new Error("Mock request too large");
        chunks.push(Buffer.from(chunk));
      }
      const body = JSON.parse(Buffer.concat(chunks).toString("utf8"));
      requests.push({ owner, path: url.pathname, method: body.method, params: body.params, timestamp: new Date().toISOString() });
      if (url.pathname === "/messages") {
        const transport = sse.get(url.searchParams.get("sessionId")!);
        if (!transport) { response.writeHead(404).end(); return; }
        await transport.handlePostMessage(request, response, body);
      } else {
        const mcp = createMcp();
        const transport = new StreamableHTTPServerTransport({ sessionIdGenerator: undefined, enableJsonResponse: true });
        response.once("close", () => { clients.delete(mcp); void mcp.close(); });
        await mcp.connect(transport);
        await transport.handleRequest(request, response, body);
      }
    } catch (error) {
      if (!response.headersSent) response.writeHead(500);
      response.end(String(error));
    }
  });
  server.on("connection", (socket) => { sockets.add(socket); socket.once("close", () => sockets.delete(socket)); });
  await new Promise<void>((resolve, reject) => { server.once("error", reject); server.listen(socketPath, () => { server.off("error", reject); resolve(); }); });
  await chmod(socketPath, 0o600);
  return {
    socketPath, requests,
    async dispose() {
      await Promise.all([...clients].map((client) => client.close()));
      for (const socket of sockets) socket.destroy();
      await new Promise<void>((resolve) => server.close(() => resolve()));
      await rm(socketPath, { force: true });
    },
  };
}
