import { lookup } from "node:dns/promises";
import { chmod, lstat, mkdir, open, rm, writeFile } from "node:fs/promises";
import { createServer, request as httpRequest, type IncomingHttpHeaders } from "node:http";
import { request as httpsRequest } from "node:https";
import { connect, isIP, type Socket } from "node:net";
import { networkInterfaces } from "node:os";
import { isAbsolute, join } from "node:path";
import { Readable } from "node:stream";
import type { NetworkPort } from "../../tool/execution-world/NetworkPort.js";

export type SessionEgressRule = {
  hostname: string;
  ports: readonly number[];
  addresses?: readonly string[];
};

/** Trusted host capability: an exact virtual origin backed by an owned Unix socket. */
export type SessionEgressLocalService = { origin: string; socketPath: string };

export type SessionEgressObservation = {
  timestamp: string;
  target: string;
  status: "ALLOW" | "DENY";
  address?: string;
  reason?: string;
};

const MAX_BODY_BYTES = 10 * 1024 * 1024;
const HOP_HEADERS = new Set(["connection", "proxy-connection", "proxy-authorization", "keep-alive", "upgrade", "transfer-encoding", "te", "trailer"]);

/** Policy is checked before DNS and the checked address is used for the actual connection. */
export class SessionEgressPolicy {
  private readonly rules: readonly SessionEgressRule[];
  private readonly hostAddresses = new Set(Object.values(networkInterfaces()).flatMap((entries) => entries?.map((entry) => entry.address) ?? []));

  constructor(rules: readonly SessionEgressRule[]) {
    this.rules = rules.map((rule) => {
      const hostname = new URL(`https://${rule.hostname}`).hostname;
      if (hostname !== rule.hostname.toLowerCase() || rule.ports.length === 0 || rule.ports.some((port) => !Number.isInteger(port) || port < 1 || port > 65535)) {
        throw new Error("Invalid session egress rule");
      }
      return { ...rule, hostname };
    });
  }

  async resolve(target: URL): Promise<{ address: string; family: number }> {
    if (!["http:", "https:"].includes(target.protocol) || target.username || target.password) throw new Error("Unsupported egress URL");
    const port = Number(target.port || (target.protocol === "https:" ? 443 : 80));
    const hostname = target.hostname.replace(/^\[|\]$/g, "");
    const rule = this.rules.find((entry) => entry.hostname.replace(/^\[|\]$/g, "") === hostname && entry.ports.includes(port));
    if (!rule) throw new Error("Target is not in the session egress allowlist");
    const addresses = isIP(hostname) ? [{ address: hostname, family: isIP(hostname) }] : await lookup(hostname, { all: true, verbatim: true });
    if (!addresses.length || addresses.some(({ address }) => !isPublicAddress(address) || this.hostAddresses.has(address))) {
      throw new Error("Host and private network addresses are denied");
    }
    const chosen = addresses.find(({ address }) => !rule.addresses || rule.addresses.includes(address));
    if (!chosen) throw new Error("Resolved IP is not in the session egress allowlist");
    return chosen;
  }
}

export function isPublicAddress(address: string): boolean {
  if (isIP(address) === 4) {
    const [a, b] = address.split(".").map(Number);
    return !(a === 0 || a === 10 || a === 127 || a! >= 224 || (a === 169 && b === 254)
      || (a === 172 && b! >= 16 && b! <= 31) || (a === 192 && (b === 168 || b === 0))
      || (a === 100 && b! >= 64 && b! <= 127) || (a === 198 && (b === 18 || b === 19))
      || (a === 192 && b === 2) || (a === 198 && b === 51) || (a === 203 && b === 0));
  }
  if (isIP(address) === 6) return /^[23][0-9a-f]{3}:/i.test(address) && !/^2001:db8:/i.test(address);
  return false;
}

export async function createSessionEgressProxy(options: {
  directory: string;
  rules: readonly SessionEgressRule[];
  localServices?: readonly SessionEgressLocalService[];
  signal: AbortSignal;
  observe?: (event: SessionEgressObservation) => void;
}) {
  const policy = new SessionEgressPolicy(options.rules);
  const localServices = new Map<string, string>();
  for (const service of options.localServices ?? []) {
    const origin = new URL(service.origin);
    if (origin.protocol !== "http:" || !origin.hostname.endsWith(".invalid") || origin.origin !== service.origin
      || !isAbsolute(service.socketPath) || localServices.has(origin.origin)) throw new Error("Invalid session local service");
    if (!(await lstat(service.socketPath)).isSocket()) throw new Error("Session local service must be a Unix socket");
    localServices.set(origin.origin, service.socketPath);
  }
  const sockets = new Set<Socket>();
  const socketPath = join(options.directory, "proxy.sock");
  const controller = new AbortController();
  const signal = AbortSignal.any([options.signal, controller.signal]);
  let disposed = false;
  const resolveTarget = async (url: URL) => {
    signal.throwIfAborted();
    try {
      if (url.username || url.password) throw new Error("Unsupported egress URL");
      const localSocket = localServices.get(url.origin);
      const resolved = localSocket
        ? { address: `unix:${localSocket}`, family: 0, socketPath: localSocket }
        : { ...await policy.resolve(url), socketPath: undefined };
      signal.throwIfAborted();
      options.observe?.({ timestamp: new Date().toISOString(), target: url.origin, status: "ALLOW", address: resolved.address });
      return resolved;
    } catch (error) {
      options.observe?.({ timestamp: new Date().toISOString(), target: url.origin, status: "DENY", reason: String(error) });
      throw error;
    }
  };
  const send = async (url: URL, init: RequestInit = {}) => {
    const address = await resolveTarget(url);
    const headers = Object.fromEntries([...new Headers(init.headers)].filter(([key]) => !HOP_HEADERS.has(key) && key !== "host"));
    const body = init.body;
    if (body !== undefined && body !== null && typeof body !== "string" && !Buffer.isBuffer(body) && !(body instanceof Uint8Array)) throw new Error("Unsupported egress request body");
    if (body && Buffer.byteLength(body as string) > MAX_BODY_BYTES) throw new Error("Egress request body limit exceeded");
    return await new Promise<Response>((resolve, reject) => {
      const request = (url.protocol === "https:" ? httpsRequest : httpRequest)(url, {
        method: init.method ?? "GET", headers, agent: false, family: address.family,
        socketPath: address.socketPath,
        lookup: (_hostname, _options, callback) => callback(null, address.address, address.family),
        signal: init.signal ? AbortSignal.any([signal, init.signal]) : signal,
      }, (response) => {
        const responseHeaders = cleanResponseHeaders(response.headers);
        const status = response.statusCode ?? 502;
        let closed = false;
        let received = 0;
        const stream = [204, 205, 304].includes(status) || init.method === "HEAD" ? null : new ReadableStream<Uint8Array>({
          start(controller) {
            response.on("data", (chunk: Buffer) => {
              if (closed) return;
              received += chunk.length;
              if (received > MAX_BODY_BYTES) {
                closed = true;
                controller.error(new Error("Egress response body limit exceeded"));
                response.destroy();
                return;
              }
              controller.enqueue(chunk);
              if ((controller.desiredSize ?? 0) <= 0) response.pause();
            });
            response.once("end", () => { if (!closed) { closed = true; controller.close(); } });
            response.once("error", (error) => { if (!closed) { closed = true; controller.error(error); } });
            response.once("aborted", () => { if (!closed) { closed = true; controller.error(new Error("Egress response aborted")); } });
          },
          pull() { if (!closed) response.resume(); },
          cancel() { closed = true; response.destroy(); },
        });
        resolve(new Response(stream, { status, statusText: response.statusMessage, headers: responseHeaders }));
        if (!stream) response.resume();
      });
      request.once("socket", (socket) => track(socket));
      request.once("error", reject);
      request.setTimeout(60_000, () => request.destroy(new Error("Egress request timeout")));
      request.end(body ?? undefined);
    });
  };
  const fetchImpl: typeof fetch = async (input, init) => {
    let url = new URL(input instanceof Request ? input.url : String(input));
    const request: RequestInit = input instanceof Request
      ? { method: input.method, headers: input.headers, redirect: input.redirect, signal: input.signal, ...init }
      : { ...init };
    if (input instanceof Request && init?.body === undefined && input.body) {
      const reader = input.body.getReader();
      const chunks: Buffer[] = [];
      let bytes = 0;
      try {
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          bytes += value.byteLength;
          if (bytes > MAX_BODY_BYTES) throw new Error("Egress request body limit exceeded");
          chunks.push(Buffer.from(value));
        }
        request.body = Buffer.concat(chunks);
      } finally { await reader.cancel().catch(() => undefined); reader.releaseLock(); }
    }
    for (let redirects = 0; redirects <= 10; redirects++) {
      const response = await send(url, request);
      if (![301, 302, 303, 307, 308].includes(response.status) || !response.headers.has("location")) return response;
      const next = new URL(response.headers.get("location")!, url);
      // Even manual redirects must not expose an unapproved destination as usable.
      try { await resolveTarget(next); }
      catch (error) { await response.body?.cancel(); throw error; }
      if (request.redirect === "manual") return response;
      await response.body?.cancel();
      if (request.redirect === "error") throw new Error("Egress redirect denied by request policy");
      if (next.origin !== url.origin) request.headers = Object.fromEntries([...new Headers(request.headers)].filter(([key]) => !["authorization", "cookie"].includes(key)));
      if (response.status === 303 || ([301, 302].includes(response.status) && request.method === "POST")) { request.method = "GET"; request.body = undefined; }
      url = next;
    }
    throw new Error("Egress redirect limit exceeded");
  };
  const server = createServer(async (request, response) => {
    try {
      const chunks: Buffer[] = [];
      let bytes = 0;
      for await (const chunk of request) {
        bytes += chunk.length;
        if (bytes > MAX_BODY_BYTES) throw new Error("Egress request body limit exceeded");
        chunks.push(Buffer.from(chunk));
      }
      const result = await fetchImpl(request.url!, {
        method: request.method, headers: cleanResponseHeaders(request.headers), redirect: "manual",
        ...(!["GET", "HEAD"].includes(request.method!) ? { body: Buffer.concat(chunks) } : {}),
      });
      response.writeHead(result.status, Object.fromEntries(result.headers));
      if (result.body) Readable.fromWeb(result.body as never).on("error", () => response.destroy()).pipe(response);
      else response.end();
    } catch (error) {
      response.writeHead(403, { "x-proxy-error": "blocked-by-allowlist" });
      response.end(String(error));
    }
  });
  const track = (socket: Socket) => { sockets.add(socket); socket.once("close", () => sockets.delete(socket)); };
  server.on("connection", track);
  server.on("connect", async (request, client, head) => {
    try {
      const url = new URL(`https://${request.url}`);
      if (url.pathname !== "/" || url.search || url.hash) throw new Error("Invalid CONNECT target");
      const address = await resolveTarget(url);
      if (address.socketPath) throw new Error("Local services require HTTP requests");
      const upstream = connect({ host: address.address, port: Number(url.port || 443), family: address.family });
      track(upstream);
      upstream.setTimeout(60_000, () => upstream.destroy());
      upstream.once("error", () => client.destroy());
      client.once("close", () => upstream.destroy());
      upstream.once("connect", () => {
        client.write("HTTP/1.1 200 Connection Established\r\n\r\n");
        if (head.length) upstream.write(head);
        upstream.pipe(client); client.pipe(upstream);
      });
    } catch {
      client.end("HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n");
    }
  });
  await mkdir(options.directory, { recursive: true, mode: 0o700 });
  await rm(socketPath, { force: true });
  await writeFile(join(options.directory, "launch.mjs"), GUEST_PROXY_LAUNCHER, { mode: 0o400 });
  // A directory FD avoids Linux's 108-byte sockaddr_un path limit without
  // putting any session endpoint outside its storage root.
  const directoryFd = process.platform === "linux" ? await open(options.directory, "r") : undefined;
  try {
    const listenPath = directoryFd ? `/proc/self/fd/${directoryFd.fd}/proxy.sock` : socketPath;
    await new Promise<void>((resolve, reject) => { server.once("error", reject); server.listen(listenPath, () => { server.off("error", reject); resolve(); }); });
  } finally { await directoryFd?.close(); }
  await chmod(socketPath, 0o600);
  const abort = () => { controller.abort(); for (const socket of sockets) socket.destroy(); server.close(); };
  options.signal.addEventListener("abort", abort, { once: true });
  return {
    directory: options.directory,
    network: { fetch: fetchImpl } satisfies NetworkPort,
    async dispose() {
      if (disposed) return;
      disposed = true;
      abort(); options.signal.removeEventListener("abort", abort);
      await new Promise<void>((resolve) => server.close(() => resolve()));
      await rm(options.directory, { recursive: true, force: true });
    },
  };
}

function cleanResponseHeaders(headers: IncomingHttpHeaders): Record<string, string> {
  return Object.fromEntries(Object.entries(headers).filter(([key, value]) => value !== undefined && !HOP_HEADERS.has(key)).map(([key, value]) => [key, Array.isArray(value) ? value.join(", ") : value!]));
}

// Each command keeps its own network namespace. A loopback relay exposes only
// the current session's Unix proxy, never the host's network namespace.
const GUEST_PROXY_LAUNCHER = `
import { createServer, connect } from 'node:net';
import { spawn } from 'node:child_process';
const server = createServer(client => {
  const upstream = connect('/run/pilotdeck-egress/proxy.sock');
  upstream.on('error', () => client.destroy());
  client.on('error', () => upstream.destroy());
  client.on('close', () => upstream.destroy());
  upstream.pipe(client); client.pipe(upstream);
});
server.listen(0, '127.0.0.1', () => {
  const proxy = 'http://127.0.0.1:' + server.address().port;
  const env = { ...process.env, HTTP_PROXY: proxy, HTTPS_PROXY: proxy, ALL_PROXY: proxy, http_proxy: proxy, https_proxy: proxy, all_proxy: proxy, NO_PROXY: '', no_proxy: '' };
  let args = process.argv.slice(3);
  if(process.env.PILOTDECK_SESSION_BROWSER_PROXY === '1') {
    const filtered=[];
    for(let i=0;i<args.length;i++) {
      if(['--proxy-server','--proxy-bypass'].includes(args[i])) { i++; continue; }
      if(args[i].startsWith('--proxy-server=') || args[i].startsWith('--proxy-bypass=')) continue;
      filtered.push(args[i]);
    }
    args=[...filtered,'--proxy-server',proxy,'--proxy-bypass','<-loopback>'];
  }
  const child = spawn(process.argv[2], args, { stdio: 'inherit', env });
  for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => child.kill(signal));
  child.on('error', () => process.exit(127));
  child.on('exit', (code, signal) => process.exit(code ?? (signal ? 128 : 1)));
});
`;
