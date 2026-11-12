// Fixed-baseline probeServer characterization; all servers use owned ephemeral
// loopback ports. Run with isolated HOME/PILOT_HOME and pass the baseline root.
import net from 'node:net';
import {pathToFileURL} from 'node:url';
import {resolve} from 'node:path';

const baseline = process.argv[2];
if (!baseline || !process.env.HOME || !process.env.PILOT_HOME) {
  throw new Error('Pass the fixed baseline root and isolate HOME/PILOT_HOME.');
}
const {probeGatewayServer} = await import(pathToFileURL(resolve(baseline, 'dist/src/gateway/client/probeServer.js')));
const cases = [];
for (const mode of ['complete', 'head-only', 'body-pending', 'bad-header', 'redirect', 'redirect-loop', 'redirect-credentials', 'redirect-port', 'status-error']) {
  const sockets = new Set();
  let requests = 0;
  const server = net.createServer(socket => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
    socket.once('data', raw => {
      requests++;
      const path = String(raw).split(' ')[1];
      switch (mode) {
        case 'head-only': socket.write('HTTP/1.1 200 OK\r\n'); break;
        case 'body-pending': socket.write('HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx'); break;
        case 'bad-header': socket.end('HTTP/1.1 200 OK\r\ninvalid header\r\n\r\n'); break;
        case 'status-error': socket.end('HTTP/1.1 503 Busy\r\nContent-Length: 0\r\n\r\n'); break;
        case 'redirect-loop': socket.end('HTTP/1.1 302 Found\r\nLocation: /health\r\nContent-Length: 0\r\nConnection: close\r\n\r\n'); break;
        case 'redirect-credentials': socket.end(`HTTP/1.1 302 Found\r\nLocation: http://user@127.0.0.1:${server.address().port}/healthy\r\nContent-Length: 0\r\nConnection: close\r\n\r\n`); break;
        case 'redirect-port': socket.end('HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:25/healthy\r\nContent-Length: 0\r\nConnection: close\r\n\r\n'); break;
        default:
          if (mode === 'redirect' && path === '/health') socket.end('HTTP/1.1 302 Found\r\nLocation: /healthy\r\nContent-Length: 0\r\nConnection: close\r\n\r\n');
          else socket.end('HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n');
      }
    });
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  try {
    const result = await probeGatewayServer({url: `http://127.0.0.1:${server.address().port}`, token: 'fixture', timeoutMs: 200});
    cases.push({mode, ok: result.ok, requests});
  } finally {
    for (const socket of sockets) socket.destroy();
    await new Promise(resolve => server.close(resolve));
  }
}
console.log(JSON.stringify({node: process.version, baseline: 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85', cases}, null, 2));
