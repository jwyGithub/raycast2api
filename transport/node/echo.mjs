// Optional public echo verification. Never sends gateway/account credentials.
import { createChatChannel } from './chat.mjs';
import { createGotChannel } from './got.mjs';
import { Readable } from 'node:stream';
import { readFile, writeFile } from 'node:fs/promises';

const report = { measuredAt: new Date().toISOString(), platform: process.platform, arch: process.arch, runtime: process.versions, target: 'https://tls.peet.ws/api/all' };
for (const [kind, create] of [['chat', createChatChannel], ['got', createGotChannel]]) {
  const channel = create(null, 15_000);
  try {
    const response = await channel.execute({ url: report.target, method: 'GET', headers: [], timeoutMs: 15_000 }, Readable.from([]), AbortSignal.timeout(15_000));
    const chunks = []; for await (const chunk of response.body) chunks.push(chunk);
    if (response.status !== 200) throw new Error(`echo returned HTTP ${response.status}`);
    const echo = JSON.parse(Buffer.concat(chunks));
    report[kind] = { httpVersion: echo.http_version, ja3: echo.tls?.ja3_hash, ja3Raw: echo.tls?.ja3,
      ja4: echo.tls?.ja4, akamai: echo.http2?.akamai_fingerprint, akamaiHash: echo.http2?.akamai_fingerprint_hash };
  } finally { await channel.close(); }
}
const profile = JSON.parse(await readFile(new URL('./profiles/raycast-windows-2.13.1.0.json', import.meta.url)));
const differences = [];
for (const kind of ['chat', 'got']) {
  if (report[kind].ja3 !== profile[kind].ja3) differences.push(`${kind}.ja3`);
  // The echo tool omits the absent ALPN code; our local parser uses JA4's 00.
  const expectedJa4 = kind === 'got' ? [profile.got.ja4, profile.got.ja4.replace('t13d521100_', 't13d5211_')] : [profile.chat.ja4];
  if (!expectedJa4.includes(report[kind].ja4)) differences.push(`${kind}.ja4`);
}
if (report.chat.httpVersion !== 'h2' || report.chat.akamai !== profile.chat.akamai) differences.push('chat.http2');
if (report.got.httpVersion !== 'HTTP/1.1') differences.push('got.httpVersion');
report.differences = differences;
report.passed = differences.length === 0;
const output = process.argv.indexOf('--output');
if (output !== -1) await writeFile(process.argv[output + 1], JSON.stringify(report, null, 2) + '\n');
console.log(JSON.stringify({ passed: report.passed, differences, chat: report.chat, got: report.got }));
if (!report.passed) process.exitCode = 1;
