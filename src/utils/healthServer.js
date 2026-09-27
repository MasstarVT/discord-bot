import { createServer } from 'http';

/**
 * Liveness and readiness endpoints for Docker and Uptime Kuma. It runs in the
 * main thread next to the ShardingManager, so /healthz keeps answering while
 * the restart brake waits or the bot is parked.
 *
 *   GET /healthz → 200 {"status":"alive"} whenever the event loop runs
 *   GET /readyz  → 200 {"status":"ready",...} when getReadiness() says so,
 *                  otherwise 503 {"status":"not-ready","reason":...}
 *
 * @param {object} opts
 * @param {number} opts.port
 * @param {() => Promise<{ ready: boolean, reason?: string }>} opts.getReadiness
 * @param {(err: Error) => void} opts.onError
 * @returns {import('http').Server}
 */
export function startHealthServer({ port, getReadiness, onError }) {
  const server = createServer(async (req, res) => {
    const send = (status, body) => {
      res.writeHead(status, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' });
      res.end(req.method === 'HEAD' ? undefined : JSON.stringify(body));
    };

    if (req.method !== 'GET' && req.method !== 'HEAD') return send(405, { error: 'method not allowed' });

    const path = req.url.split('?')[0];
    if (path === '/healthz') return send(200, { status: 'alive' });
    if (path !== '/readyz') return send(404, { error: 'not found' });

    let result;
    try {
      result = await getReadiness();
    } catch (err) {
      result = { ready: false, reason: 'shard-not-ready', error: err.message };
    }
    const { ready, ...details } = result;
    send(ready ? 200 : 503, { status: ready ? 'ready' : 'not-ready', ...details });
  });

  server.headersTimeout   = 5_000;
  server.requestTimeout   = 10_000;
  server.keepAliveTimeout = 5_000;
  server.on('error', onError);

  try {
    server.listen(port);
  } catch (err) {
    onError(err); // e.g. an invalid HEALTH_PORT
  }
  return server;
}
