import test, { before, after } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import https from 'node:https';
import http2 from 'node:http2';
import net from 'node:net';
import tls from 'node:tls';
import { Readable } from 'node:stream';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { once } from 'node:events';
import { gzipSync, deflateSync, brotliCompressSync, zstdCompressSync } from 'node:zlib';
import { createService, validateProfile } from '../main.mjs';
import { createChatChannel } from '../chat.mjs';
import { createGotChannel } from '../got.mjs';
import { captureHello } from '../fingerprint.mjs';
import { frame, readMetadata } from '../protocol.mjs';

let directory, credentials, originalCa;
before(() => {
  directory = mkdtempSync(join(tmpdir(), 'raycast-tls-test-'));
  execFileSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', join(directory, 'key.pem'), '-out', join(directory, 'cert.pem'), '-days', '1', '-subj', '/CN=localhost', '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1'], { stdio: 'ignore' });
  credentials = { key: readFileSync(join(directory, 'key.pem')), cert: readFileSync(join(directory, 'cert.pem')) };
  originalCa = tls.getCACertificates();
  tls.setDefaultCACertificates([...originalCa, credentials.cert]);
});
after(() => { tls.setDefaultCACertificates(originalCa); rmSync(directory, { recursive: true, force: true }); });

async function listen(server) {
  const sockets = new Set();
  server.on('connection', socket => { sockets.add(socket); socket.on('error', () => {}); socket.on('close', () => sockets.delete(socket)); });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  return { server, port: server.address().port, async close() { for (const socket of sockets) socket.destroy(); await new Promise(resolve => server.close(resolve)); } };
}

async function proxy(scheme) {
  let connects = 0, authenticated = false;
  const outgoing = new Set();
  const tunnel = (client, host, port, head, connected) => {
    const socket = net.connect(port, host);
    outgoing.add(socket);
    socket.on('error', () => client.destroy());
    client.on('error', () => socket.destroy());
    socket.on('close', () => { outgoing.delete(socket); client.destroy(); });
    client.on('close', () => socket.destroy());
    socket.on('connect', () => { connects++; connected(); if (head.length) socket.write(head); client.pipe(socket).pipe(client); });
  };
  let server;
  if (scheme === 'socks5') {
    server = net.createServer(client => {
      let state = 'greeting', bytes = Buffer.alloc(0);
      const consume = size => { const result = bytes.subarray(0, size); bytes = bytes.subarray(size); return result; };
      const data = chunk => {
        bytes = Buffer.concat([bytes, chunk]);
        if (state === 'greeting' && bytes.length >= 2 && bytes.length >= 2 + bytes[1]) { consume(2 + bytes[1]); client.write(Buffer.from([5, 2])); state = 'auth'; }
        if (state === 'auth' && bytes.length >= 2 && bytes.length >= 3 + bytes[1] && bytes.length >= 3 + bytes[1] + bytes[2 + bytes[1]]) {
          const userLength = bytes[1], passwordLength = bytes[2 + userLength];
          authenticated = bytes.subarray(2, 2 + userLength).toString() === 'u' && bytes.subarray(3 + userLength, 3 + userLength + passwordLength).toString() === 'p';
          consume(3 + userLength + passwordLength); client.write(Buffer.from([1, authenticated ? 0 : 1])); state = 'connect';
        }
        if (state === 'connect' && bytes.length >= 10) {
          assert.equal(bytes[3], 1, 'SOCKS5 uses local IPv4 DNS in this fixture');
          const host = [...bytes.subarray(4, 8)].join('.'), port = bytes.readUInt16BE(8);
          consume(10); client.off('data', data);
          tunnel(client, host, port, bytes, () => client.write(Buffer.from([5, 0, 0, 1, 127, 0, 0, 1, 0, 0])));
        }
      };
      client.on('data', data);
    });
  } else {
    server = scheme === 'https' ? https.createServer(credentials) : http.createServer();
    server.on('connect', (request, client, head) => {
      authenticated = request.headers['proxy-authorization'] === 'Basic dTpw';
      const target = new URL('http://' + request.url);
      tunnel(client, target.hostname, Number(target.port), head, () => client.write('HTTP/1.1 200 Connection Established\r\n\r\n'));
    });
  }
  const listener = await listen(server);
  return { url: `${scheme}://u:p@127.0.0.1:${listener.port}`, get connects() { return connects; }, get authenticated() { return authenticated; },
    async close() { for (const socket of outgoing) socket.destroy(); await listener.close(); } };
}

function executeChannel(channel, url, headers = [], method = 'GET', data = '') {
  return channel.execute({ url, method, headers, timeoutMs: 3000 }, Readable.from(data ? [Buffer.from(data)] : []), AbortSignal.timeout(3000));
}
async function text(body) { const chunks = []; for await (const chunk of body) chunks.push(chunk); return Buffer.concat(chunks).toString(); }

test('fixed runtime and real dual ClientHello match the profile', async () => { assert.deepEqual((await validateProfile()).differences, []); });

for (const scheme of ['direct', 'http', 'https', 'socks5']) {
  test(`both TLS profiles survive ${scheme} without proxy credentials reaching target`, async () => {
    const p = scheme === 'direct' ? null : await proxy(scheme);
    const chat = createChatChannel(p?.url, 3000), got = createGotChannel(p?.url, 3000);
    try {
      const chatHello = await captureHello(url => executeChannel(chat, url));
      const gotHello = await captureHello(url => executeChannel(got, url));
      assert.equal(chatHello.ja3, 'd67b094811e5145139d7cea5f014309f');
      assert.equal(chatHello.ja4, 't13d5212h1_b262b3658495_8e6e362c5eac');
      assert.deepEqual(chatHello.alpn, ['http/1.1', 'h2']);
      assert.equal(gotHello.ja3, '944d1e1858cd278718f8a46b65d3212f');
      assert.equal(gotHello.ja4, 't13d521100_b262b3658495_8e6e362c5eac');
      assert.deepEqual(gotHello.alpn, []);
      if (p) { assert.equal(p.connects, 2); assert.equal(p.authenticated, true); }
    } finally { await chat.close(); got.close(); await p?.close(); }
  });
}

test('chat negotiates HTTP2 and got remains HTTP1; raw body and headers survive', async () => {
  const versions = [];
  const upstream = await listen(http2.createSecureServer({ ...credentials, allowHTTP1: true }, async (req, res) => {
    versions.push(req.httpVersion);
    const body = await text(req);
    assert.equal(req.headers['proxy-authorization'], undefined);
    res.end(JSON.stringify({ body, ua: req.headers['user-agent'] || null, signature: req.headers['x-raycast-signature-v2'] }));
  }));
  const chat = createChatChannel(null, 3000), got = createGotChannel(null, 3000);
  try {
    const url = `https://localhost:${upstream.port}/`;
    const body = '{ "precise": 123456789012345678901 }';
    const c = await executeChannel(chat, url, [['x-raycast-signature-v2', 'signature'], ['content-length', String(Buffer.byteLength(body))]], 'POST', body);
    assert.equal(JSON.parse(await text(c.body)).body, body);
    const g = await executeChannel(got, url, [['x-raycast-signature-v2', 'signature'], ['content-length', String(Buffer.byteLength(body))]], 'PUT', body);
    const parsed = JSON.parse(await text(g.body));
    assert.equal(parsed.body, body); assert.equal(parsed.signature, 'signature'); assert.equal(parsed.ua, null);
    assert.deepEqual(versions, ['2.0', '1.1']);
  } finally { await chat.close(); got.close(); await upstream.close(); }
});

test('framing preserves bytes beyond metadata including binary uploads', async () => {
  const data = Buffer.from([0, 255, 13, 10, 128]);
  const encoded = Buffer.concat([frame({ version: 1 }), data]);
  for (const chunks of [[encoded], [encoded.subarray(0, 2), encoded.subarray(2, 7), encoded.subarray(7)]]) {
    const stream = Readable.from(chunks);
    assert.deepEqual(await readMetadata(stream), { version: 1 });
    const body = []; for await (const chunk of stream) body.push(chunk);
    assert.deepEqual(Buffer.concat(body), data);
  }
});

async function relay(service, metadata, body = '') {
  const request = http.request(`http://127.0.0.1:${service.port}/v1/execute`, { method: 'POST', headers: { authorization: `Bearer ${'x'.repeat(64)}` } });
  request.end(Buffer.concat([frame({ version: 1, channel: -1, method: 'GET', headers: [], timeoutMs: 3000, ...metadata }), Buffer.from(body)]));
  const [response] = await once(request, 'response');
  if (response.statusCode !== 200) throw new Error(await text(response));
  return { metadata: await readMetadata(response), body: response };
}

test('relay preserves HTTP failures and decompresses all advertised encodings once', async () => {
  const encode = { gzip: gzipSync, deflate: deflateSync, br: brotliCompressSync, zstd: zstdCompressSync };
  const upstream = await listen(http.createServer((req, res) => {
    const encoding = req.url.slice(1);
    const body = encode[encoding](Buffer.from('payload'));
    res.writeHead(429, { 'content-encoding': encoding, 'content-length': body.length, 'retry-after': '7' }); res.end(body);
  }));
  const service = await createService({ token: 'x'.repeat(64), proxies: [], timeoutMs: 3000 });
  try {
    for (const transport of ['chat', 'got']) for (const encoding of Object.keys(encode)) {
      const result = await relay(service, { transport, url: `http://127.0.0.1:${upstream.port}/${encoding}` });
      assert.equal(result.metadata.status, 429);
      assert.equal(await text(result.body), 'payload');
      assert.equal(result.metadata.headers.some(([key]) => ['content-encoding', 'content-length'].includes(key.toLowerCase())), false);
      assert.equal(result.metadata.headers.find(([key]) => key.toLowerCase() === 'retry-after')[1], '7');
    }
  } finally { await service.close(); await upstream.close(); }
});

test('relay forwards binary POST after metadata parsing without disturbing fetch body', async () => {
  const upstream = await listen(http.createServer(async (req, res) => {
    const chunks = []; for await (const chunk of req) chunks.push(chunk);
    res.end(Buffer.concat(chunks));
  }));
  const service = await createService({ token: 'x'.repeat(64), proxies: [], timeoutMs: 3000 });
  try {
    for (const transport of ['chat', 'got']) {
      const body = Buffer.from([0, 255, 13, 10, 128]);
      const result = await relay(service, { transport, method: 'POST', bodyLength: body.length, url: `http://127.0.0.1:${upstream.port}/` }, body);
      const chunks = []; for await (const chunk of result.body) chunks.push(chunk);
      assert.deepEqual(Buffer.concat(chunks), body);
    }
  } finally { await service.close(); await upstream.close(); }
});

test('partial SSE is delivered immediately and truncation remains a stream error', async () => {
  const upstream = await listen(http.createServer((req, res) => {
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    res.write('data: {"text":"partial"}\n\n');
    setTimeout(() => res.destroy(), 100);
  }));
  const service = await createService({ token: 'x'.repeat(64), proxies: [], timeoutMs: 3000 });
  try {
    for (const transport of ['chat', 'got']) {
      const result = await relay(service, { transport, url: `http://127.0.0.1:${upstream.port}/` });
      const iterator = result.body[Symbol.asyncIterator]();
      const first = await iterator.next(); assert.match(first.value.toString(), /partial/);
      await assert.rejects(async () => { while (!(await iterator.next()).done) {} });
    }
  } finally { await service.close(); await upstream.close(); }
});

test('cancelling one relay request closes that upstream and leaves pool usable', async () => {
  let cancelled;
  const closed = new Promise(resolve => { cancelled = resolve; });
  const upstream = await listen(http.createServer((req, res) => {
    if (req.url === '/ok') { res.end('ok'); return; }
    res.write('first'); req.on('close', cancelled);
  }));
  const service = await createService({ token: 'x'.repeat(64), proxies: [], timeoutMs: 3000 });
  try {
    const result = await relay(service, { transport: 'chat', url: `http://127.0.0.1:${upstream.port}/slow` });
    result.body.destroy();
    await Promise.race([closed, new Promise((_, reject) => setTimeout(() => reject(new Error('upstream cancellation missing')), 1500))]);
    const next = await relay(service, { transport: 'chat', url: `http://127.0.0.1:${upstream.port}/ok` });
    assert.equal(await text(next.body), 'ok');
  } finally { await service.close(); await upstream.close(); }
});

test('redirects strip credentials across origins and never replay signed POST or PUT', async () => {
  const received = [];
  const destination = await listen(http.createServer((req, res) => {
    received.push(req.headers); res.end('redirected');
  }));
  let calls = 0;
  const origin = await listen(http.createServer((req, res) => {
    calls++; res.writeHead(307, { location: `http://127.0.0.1:${destination.port}/` }); res.end('redirect');
  }));
  const service = await createService({ token: 'x'.repeat(64), proxies: [], timeoutMs: 3000 });
  try {
    for (const transport of ['chat', 'got']) {
      const headers = [['authorization', 'Bearer secret'], ['cookie', 'session=secret'], ['x-raycast-signature-v2', 'signature']];
      const result = await relay(service, { transport, headers, url: `http://127.0.0.1:${origin.port}/` });
      assert.equal(await text(result.body), 'redirected');
      const target = received.at(-1);
      assert.equal(target.authorization, undefined); assert.equal(target.cookie, undefined); assert.equal(target['x-raycast-signature-v2'], undefined);
      for (const method of ['POST', 'PUT']) {
        const result = await relay(service, { transport, method, headers, bodyLength: 0, url: `http://127.0.0.1:${origin.port}/` });
        assert.equal(result.metadata.status, 307); assert.equal(await text(result.body), 'redirect');
      }
    }
    assert.equal(received.length, 2); assert.equal(calls, 6);
  } finally { await service.close(); await origin.close(); await destination.close(); }
});

test('IPC rejects requests without the per-process token before sending upstream', async () => {
  let calls = 0;
  const upstream = await listen(http.createServer((req, res) => { calls++; res.end('unexpected'); }));
  const service = await createService({ token: 'x'.repeat(64), proxies: [], timeoutMs: 3000 });
  try {
    const request = http.request(`http://127.0.0.1:${service.port}/v1/execute`, { method: 'POST' });
    request.end(frame({ version: 1, channel: -1, transport: 'got', method: 'GET', headers: [], timeoutMs: 3000, url: `http://127.0.0.1:${upstream.port}/` }));
    const [response] = await once(request, 'response');
    assert.equal(response.statusCode, 401); await text(response); assert.equal(calls, 0);
  } finally { await service.close(); await upstream.close(); }
});

test('per-request header deadline can exceed the pool connection timeout', async () => {
  const upstream = await listen(http.createServer((req, res) => { setTimeout(() => res.end('slow headers'), 250); }));
  const service = await createService({ token: 'x'.repeat(64), proxies: [], timeoutMs: 100 });
  try {
    for (const transport of ['chat', 'got']) {
      const result = await relay(service, { transport, timeoutMs: 1000, url: `http://127.0.0.1:${upstream.port}/` });
      assert.equal(await text(result.body), 'slow headers');
      await assert.rejects(relay(service, { transport, timeoutMs: 50, url: `http://127.0.0.1:${upstream.port}/` }));
    }
  } finally { await service.close(); await upstream.close(); }
});
