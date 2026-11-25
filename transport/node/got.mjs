import https from 'node:https';
import http from 'node:http';
import { pipeline } from 'node:stream/promises';
import { createGunzip, createInflate, createBrotliDecompress, createZstdDecompress } from 'node:zlib';
import { openTunnel } from './proxy.mjs';

export function createGotChannel(proxy, timeout) {
  const agent = new https.Agent({ keepAlive: true, timeout: 20_000, rejectUnauthorized: true });
  const plainAgent = new http.Agent({ keepAlive: true, timeout: 20_000 });
  if (proxy) {
    agent.createConnection = (options, callback) => {
      const target = new URL(`https://${options.host.includes(':') ? '[' + options.host + ']' : options.host}:${options.port}`);
      openTunnel(target, proxy, timeout).then(socket => {
        const connected = https.Agent.prototype.createConnection.call(agent, { ...options, socket, ALPNProtocols: undefined, rejectUnauthorized: true });
        callback(null, connected);
      }, callback);
    };
    plainAgent.createConnection = (options, callback) => {
      const target = new URL(`http://${options.host.includes(':') ? '[' + options.host + ']' : options.host}:${options.port}`);
      openTunnel(target, proxy, timeout).then(socket => { socket.resume(); callback(null, socket); }, callback);
    };
  }
  return {
    execute(metadata, body, signal) {
      return new Promise((resolve, reject) => {
        const url = new URL(metadata.url);
        const transport = url.protocol === 'https:' ? https : http;
        const headers = [...metadata.headers];
        // A raw header array suppresses Node's automatic Host insertion.
        if (!headers.some(([key]) => key.toLowerCase() === 'host')) headers.push(['Host', url.host]);
        const request = transport.request(url, { method: metadata.method, headers: headers.flat(),
          agent: url.protocol === 'https:' ? agent : plainAgent, signal }, response => {
          clearTimeout(timer);
          const headers = [];
          for (let i = 0; i < response.rawHeaders.length; i += 2) headers.push([response.rawHeaders[i], response.rawHeaders[i + 1]]);
          const encoding = response.headers['content-encoding'];
          const decompress = { gzip: createGunzip, deflate: createInflate, br: createBrotliDecompress, zstd: createZstdDecompress }[encoding];
          if (encoding && !decompress && encoding !== 'identity') { response.destroy(); reject(new Error('unsupported response encoding')); return; }
          let stream = response;
          if (decompress && metadata.method !== 'HEAD' && ![204, 304].includes(response.statusCode)) {
            stream = decompress();
            pipeline(response, stream, { signal }).catch(error => stream.destroy(error));
          }
          resolve({ status: response.statusCode, headers, decoded: !!decompress, body: stream });
        });
        const timer = setTimeout(() => request.destroy(new Error('upstream response header timeout')), metadata.timeoutMs);
        request.once('socket', socket => {
          const connected = url.protocol === 'https:' ? 'secureConnect' : 'connect';
          if (!socket.connecting && (url.protocol !== 'https:' || socket._secureEstablished)) return;
          const timer = setTimeout(() => request.destroy(new Error('upstream connect timeout')), timeout);
          socket.once(connected, () => clearTimeout(timer));
          request.once('close', () => clearTimeout(timer));
        });
        request.on('error', error => { clearTimeout(timer); reject(error); });
        pipeline(body, request, { signal }).catch(error => request.destroy(error));
      });
    },
    close() { agent.destroy(); plainAgent.destroy(); }
  };
}
