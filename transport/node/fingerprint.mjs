// Capture actual local ClientHello bytes; no fake TLS fingerprint responses.
import net from 'node:net';
import https from 'node:https';
import { createHash } from 'node:crypto';
import { readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

const digest = (algorithm, value) => createHash(algorithm).update(value).digest('hex');
const grease = value => (value & 0x0f0f) === 0x0a0a && (value >> 8) === (value & 255);
const hex = value => value.toString(16).padStart(4, '0');
const words = bytes => Array.from({ length: bytes.length / 2 }, (_, i) => bytes.readUInt16BE(i * 2));

export function parseHello(handshake) {
  if (handshake[0] !== 1) throw new Error('not a ClientHello');
  let offset = 4;
  const version = handshake.readUInt16BE(offset);
  offset += 34;
  offset += 1 + handshake[offset];
  const cipherLength = handshake.readUInt16BE(offset);
  offset += 2;
  const ciphers = words(handshake.subarray(offset, offset + cipherLength)).filter(v => !grease(v));
  offset += cipherLength;
  offset += 1 + handshake[offset];
  const extensionLength = handshake.readUInt16BE(offset);
  offset += 2;
  const end = offset + extensionLength;
  const extensions = [];
  const payloads = {};
  while (offset < end) {
    const type = handshake.readUInt16BE(offset);
    const size = handshake.readUInt16BE(offset + 2);
    const payload = handshake.subarray(offset + 4, offset + 4 + size);
    if (!grease(type)) extensions.push(type);
    payloads[type] = payload;
    offset += 4 + size;
  }
  const groups = payloads[10] ? words(payloads[10].subarray(2)).filter(v => !grease(v)) : [];
  const pointFormats = payloads[11] ? [...payloads[11].subarray(1)] : [];
  const signatureAlgorithms = payloads[13] ? words(payloads[13].subarray(2)) : [];
  const supportedVersions = payloads[43] ? words(payloads[43].subarray(1)).filter(v => !grease(v)) : [version];
  const alpn = [];
  if (payloads[16]) {
    for (let p = 2; p < payloads[16].length;) {
      const size = payloads[16][p++];
      alpn.push(payloads[16].subarray(p, p + size).toString());
      p += size;
    }
  }
  const keyShares = [];
  if (payloads[51]) {
    for (let p = 2; p < payloads[51].length;) {
      const group = payloads[51].readUInt16BE(p);
      const length = payloads[51].readUInt16BE(p + 2);
      keyShares.push({ group, length });
      p += 4 + length;
    }
  }
  const ja3Raw = [version, ciphers.join('-'), extensions.join('-'), groups.join('-'), pointFormats.join('-')].join(',');
  const tlsVersion = Math.max(...supportedVersions) === 772 ? '13' : '12';
  const firstAlpn = alpn[0];
  const alpnCode = firstAlpn ? firstAlpn[0] + firstAlpn.at(-1) : '00';
  const ja4a = `t${tlsVersion}${extensions.includes(0) ? 'd' : 'i'}${String(ciphers.length).padStart(2, '0')}${String(extensions.length).padStart(2, '0')}${alpnCode}`;
  const ja4b = digest('sha256', [...ciphers].sort((a, b) => a - b).map(hex).join(',')).slice(0, 12);
  const ext = extensions.filter(v => v !== 0 && v !== 16).sort((a, b) => a - b).map(hex).join(',');
  const ja4c = digest('sha256', ext + (signatureAlgorithms.length ? '_' + signatureAlgorithms.map(hex).join(',') : '')).slice(0, 12);
  return { ja3: digest('md5', ja3Raw), ja3Raw, ja4: `${ja4a}_${ja4b}_${ja4c}`, ciphers, extensions, groups, pointFormats, signatureAlgorithms, supportedVersions, keyShares, alpn,
    extensionPayloads: Object.fromEntries(Object.entries(payloads).filter(([type]) => ![0, 51].includes(Number(type))).map(([type, bytes]) => [type, bytes.toString('hex')])) };
}

export async function captureHello(send) {
  let resolveCapture, rejectCapture;
  const captured = new Promise((resolve, reject) => { resolveCapture = resolve; rejectCapture = reject; });
  const onConnection = socket => {
    let records = Buffer.alloc(0), handshake = Buffer.alloc(0);
    socket.on('error', () => {});
    socket.on('data', chunk => {
      records = Buffer.concat([records, chunk]);
      while (records.length >= 5) {
        const size = records.readUInt16BE(3);
        if (records.length < size + 5) break;
        if (records[0] !== 22) { rejectCapture(new Error('unexpected TLS record')); socket.destroy(); return; }
        handshake = Buffer.concat([handshake, records.subarray(5, size + 5)]);
        records = records.subarray(size + 5);
        if (handshake.length >= 4 && handshake.length >= 4 + handshake.readUIntBE(1, 3)) {
          try { resolveCapture(parseHello(handshake)); } catch (error) { rejectCapture(error); }
          socket.destroy();
          return;
        }
      }
    });
  };
  const listen = (server, host, port) => new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen({ host, port, ipv6Only: true }, () => { server.off('error', reject); resolve(); });
  });
  // Keep localhost TLS SNI, and accept either local-DNS or TCP fallback choice.
  // Separate loopback listeners avoid exposing this probe on external interfaces.
  const server = net.createServer(onConnection);
  await listen(server, '127.0.0.1', 0);
  const ipv6 = net.createServer(onConnection);
  try { await listen(ipv6, '::1', server.address().port); }
  catch (error) {
    if (!['EAFNOSUPPORT', 'EADDRNOTAVAIL'].includes(error.code)) {
      await new Promise(resolve => server.close(resolve));
      throw error;
    }
  }
  const timer = setTimeout(() => rejectCapture(new Error('ClientHello capture timed out')), 5000);
  const sending = send(`https://localhost:${server.address().port}/`).catch(() => {});
  try { return await captured; } finally {
    clearTimeout(timer);
    await sending;
    await Promise.all([server, ipv6].filter(listener => listener.listening).map(listener => new Promise(resolve => listener.close(resolve))));
  }
}

export function httpsProbe(url) {
  return new Promise((resolve, reject) => {
    const request = https.get(url, { agent: false }, response => { response.resume(); response.on('end', resolve); });
    request.on('error', reject);
  });
}

export async function verifyRuntime() {
  const profile = JSON.parse(await readFile(new URL('./profiles/raycast-windows-2.13.1.0.json', import.meta.url)));
  const chat = await captureHello(url => fetch(url));
  const got = await captureHello(httpsProbe);
  const differences = [];
  for (const kind of ['chat', 'got']) {
    const result = kind === 'chat' ? chat : got;
    for (const field of ['ja3', 'ja4', 'alpn']) {
      if (JSON.stringify(result[field]) !== JSON.stringify(profile[kind][field])) differences.push(`${kind}.${field}`);
    }
    for (const field of ['groups', 'pointFormats']) {
      if (JSON.stringify(result[field]) !== JSON.stringify(profile[field])) differences.push(`${kind}.${field}`);
    }
  }
  for (const [key, value] of Object.entries(profile.runtime)) if (process.versions[key] !== value) differences.push(`runtime.${key}`);
  return { profile: profile.id, measuredAt: new Date().toISOString(), platform: process.platform, arch: process.arch,
    binarySha256: digest('sha256', await readFile(process.execPath)), runtime: process.versions, chat, got, differences, passed: differences.length === 0 };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const report = await verifyRuntime();
  const output = process.argv.indexOf('--output');
  if (output !== -1) await writeFile(process.argv[output + 1], JSON.stringify(report, null, 2) + '\n');
  console.log(JSON.stringify({ passed: report.passed, runtime: report.runtime.node, differences: report.differences, chat: report.chat.ja3, got: report.got.ja3, ja4: [report.chat.ja4, report.got.ja4] }));
  if (!report.passed) process.exitCode = 1;
}
