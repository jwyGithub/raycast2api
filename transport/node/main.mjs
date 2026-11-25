import http from 'node:http';
import { createInterface } from 'node:readline';
import { once } from 'node:events';
import { pipeline } from 'node:stream/promises';
import { fileURLToPath } from 'node:url';
import { readFile } from 'node:fs/promises';
import { createChatChannel } from './chat.mjs';
import { createGotChannel } from './got.mjs';
import { cleanHeaders, frame, readMetadata } from './protocol.mjs';
import { verifyRuntime, captureHello } from './fingerprint.mjs';
import { Readable } from 'node:stream';

export async function validateProfile() {
  if (process.env.NODE_TLS_REJECT_UNAUTHORIZED === '0') throw new Error('TLS certificate verification must be enabled');
  const report = await verifyRuntime();
  const dependency = JSON.parse(await readFile(new URL('./node_modules/undici/package.json', import.meta.url)));
  if (dependency.version !== '8.12.4') throw new Error('undici must be 8.12.4');
  const chat = createChatChannel(null, 5000);
  try {
    const hello = await captureHello(url => chat.execute({ url, method: 'GET', headers: [], timeoutMs: 5000 }, Readable.from([]), AbortSignal.timeout(5000)));
    if (hello.ja3 !== report.chat.ja3 || hello.ja4 !== report.chat.ja4 || JSON.stringify(hello.alpn) !== JSON.stringify(report.chat.alpn)) report.differences.push('external-undici');
  } finally { await chat.close(); }
  if (report.differences.length) throw new Error(`TLS profile mismatch: ${report.differences.join(', ')}`);
  return report;
}

export async function createService(config) {
  if (typeof config.token !== 'string' || config.token.length < 32) throw new Error('invalid IPC token');
  const channels = new Map();
  const timeout = config.timeoutMs;
  if (!Number.isSafeInteger(timeout) || timeout <= 0) throw new Error('invalid connect timeout');
  const proxies = config.proxies || [];
  for (const proxy of proxies) if (!['http:', 'https:', 'socks5:'].includes(new URL(proxy).protocol)) throw new Error('unsupported proxy protocol');
  const controllers = new Set();
  const getChannel = (transport, channel) => {
    if (!['chat', 'got'].includes(transport)) throw new Error('invalid transport');
    if (channel !== -1 && (!Number.isInteger(channel) || channel < 0 || channel >= proxies.length)) throw new Error('invalid channel');
    const key = `${transport}:${channel}`;
    if (!channels.has(key)) channels.set(key, (transport === 'chat' ? createChatChannel : createGotChannel)(channel === -1 ? null : proxies[channel], timeout));
    return channels.get(key);
  };
  const server = http.createServer(async (request, response) => {
    if (request.headers.authorization !== `Bearer ${config.token}`) { response.writeHead(401).end(); return; }
    if (request.url === '/v1/health' && request.method === 'GET') { response.writeHead(200).end('{"ready":true}'); return; }
    if (request.url !== '/v1/execute' || request.method !== 'POST') { response.writeHead(404).end(); return; }
    const controller = new AbortController();
    controllers.add(controller);
    response.on('close', () => { if (!response.writableFinished) controller.abort(); });
    request.on('aborted', () => controller.abort());
    let sent = false, timer;
    try {
      timer = setTimeout(() => { controller.abort(); request.destroy(); }, timeout);
      let metadata = await readMetadata(request);
      clearTimeout(timer);
      if (metadata.version !== 1 || !Number.isSafeInteger(metadata.timeoutMs) || metadata.timeoutMs <= 0
        || !Array.isArray(metadata.headers) || !metadata.headers.every(h => Array.isArray(h) && h.length === 2 && h.every(v => typeof v === 'string'))
        || typeof metadata.method !== 'string') throw new Error('invalid request metadata');
      const target = new URL(metadata.url);
      if (!['http:', 'https:'].includes(target.protocol) || target.username || target.password) throw new Error('invalid target URL');
      metadata.headers = cleanHeaders(metadata.headers);
      if (Number.isSafeInteger(metadata.bodyLength) && !['GET', 'HEAD'].includes(metadata.method)
        && !metadata.headers.some(([key]) => key.toLowerCase() === 'content-length')) metadata.headers.push(['content-length', String(metadata.bodyLength)]);
      const channel = getChannel(metadata.transport, metadata.channel);
      timer = setTimeout(() => controller.abort(new Error('upstream response header timeout')), metadata.timeoutMs);
      sent = true;
      // Metadata parsing has already disturbed IncomingMessage. Fetch requires
      // a fresh body stream; wrap rather than passing that consumed stream.
      const inputBody = Readable.from(request, { objectMode: false });
      let upstream = await channel.execute(metadata, inputBody, controller.signal);
      // GET/HEAD redirects can be followed safely; signed POST and PUT never replay.
      for (let redirects = 0; ['GET', 'HEAD'].includes(metadata.method) && [301, 302, 303, 307, 308].includes(upstream.status); redirects++) {
        const location = upstream.headers.find(([key]) => key.toLowerCase() === 'location')?.[1];
        if (!location) break;
        if (redirects >= 10) throw new Error('too many redirects');
        upstream.body.destroy();
        const previous = new URL(metadata.url), next = new URL(location, previous);
        if (!['http:', 'https:'].includes(next.protocol) || next.username || next.password || (previous.protocol === 'https:' && next.protocol !== 'https:')) throw new Error('invalid redirect');
        const headers = metadata.headers.filter(([key]) => key.toLowerCase() !== 'host' && (previous.origin === next.origin
          || !['authorization', 'cookie'].includes(key.toLowerCase()) && !key.toLowerCase().startsWith('x-raycast-')));
        metadata = { ...metadata, url: next.href, headers };
        upstream = await channel.execute(metadata, Readable.from([]), controller.signal);
      }
      clearTimeout(timer);
      const headers = cleanHeaders(upstream.headers, upstream.decoded);
      response.writeHead(200, { 'content-type': 'application/octet-stream' });
      response.write(frame({ version: 1, status: upstream.status, headers, decoded: upstream.decoded }));
      await pipeline(upstream.body, response, { signal: controller.signal });
    } catch (error) {
      if (response.headersSent || response.destroyed) response.destroy(error);
      else {
        response.writeHead(502, { 'content-type': 'application/json' });
        // Do not serialize URLs, proxy credentials, tokens or request headers.
        response.end(JSON.stringify({ code: sent ? 'upstream_transport' : 'invalid_request', sent, message: controller.signal.aborted ? 'upstream timeout or cancellation' : 'transport execution failed', cause: error.code || error.cause?.code || error.name }));
      }
    } finally { clearTimeout(timer); controllers.delete(controller); }
  });
  server.requestTimeout = 0; // SSE/upload lifetime is controlled per request, not globally.
  server.headersTimeout = 10_000;
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  return {
    port: server.address().port,
    async close() {
      for (const controller of controllers) controller.abort();
      server.closeAllConnections();
      await Promise.allSettled([...channels.values()].map(channel => channel.close()));
      await new Promise(resolve => server.close(resolve));
    }
  };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  try {
    const lines = createInterface({ input: process.stdin });
    let inputClosed = false;
    lines.on('close', () => { inputClosed = true; });
    const [line] = await Promise.race([once(lines, 'line'), once(lines, 'close').then(() => { throw new Error('parent input closed'); })]);
    if (line.length > 64 * 1024) throw new Error('initialization too large');
    const config = JSON.parse(line);
    if (config.profile !== 'raycast-windows-2.13.1.0') throw new Error('unsupported TLS profile');
    const report = await validateProfile();
    if (inputClosed) throw new Error('parent input closed');
    const service = await createService(config);
    if (inputClosed) { await service.close(); throw new Error('parent input closed'); }
    console.log(JSON.stringify({ version: 1, port: service.port, profile: report.profile, runtime: report.runtime, binarySha256: report.binarySha256 }));
    let closing = false;
    const close = async () => { if (closing) return; closing = true; await service.close(); lines.close(); process.exit(0); };
    lines.on('line', value => { if (value === 'shutdown') void close(); });
    lines.on('close', () => void close());
    process.on('SIGTERM', () => void close());
    process.on('SIGINT', () => void close());
  } catch (error) { console.error(error.message); process.exit(1); }
}
