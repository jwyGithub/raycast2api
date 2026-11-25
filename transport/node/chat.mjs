import { Agent, buildConnector, fetch } from 'undici';
import { Readable } from 'node:stream';
import { openTunnel } from './proxy.mjs';

export function createChatChannel(proxy, timeout) {
  const connector = buildConnector({ timeout, allowH2: true, preferH2: false, rejectUnauthorized: true });
  const agent = new Agent({ allowH2: true, preferH2: false, keepAliveTimeout: 20_000, keepAliveMaxTimeout: 20_000,
    // The IPC service enforces each request's header deadline (images use 60s).
    // A pool-wide timeout would incorrectly cap that longer request at 20s.
    headersTimeout: 0, bodyTimeout: 0,
    connect: proxy ? (options, callback) => {
      const target = new URL(`${options.protocol}//${options.hostname.includes(':') ? '[' + options.hostname + ']' : options.hostname}:${options.port}`);
      openTunnel(target, proxy, timeout).then(socket => {
        connector({ ...options, httpSocket: socket }, (error, connected) => { if (error) socket.destroy(); callback(error, connected); });
      }, error => callback(error));
    } : connector });
  return {
    async execute(metadata, body, signal) {
      const response = await fetch(metadata.url, { method: metadata.method, headers: metadata.headers, dispatcher: agent,
        body: ['GET', 'HEAD'].includes(metadata.method) ? undefined : body, duplex: 'half', signal, redirect: 'manual' });
      return { status: response.status, headers: [...response.headers], decoded: true,
        body: response.body ? Readable.fromWeb(response.body) : Readable.from([]) };
    },
    close: () => agent.destroy()
  };
}
