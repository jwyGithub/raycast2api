import net from 'node:net';
import tls from 'node:tls';
import { lookup } from 'node:dns/promises';

class SocketReader {
  constructor(socket) {
    this.socket = socket;
    this.buffer = Buffer.alloc(0);
    this.waiter = null;
    this.error = null;
    this.data = chunk => { this.buffer = Buffer.concat([this.buffer, chunk]); this.wake(); };
    this.failed = error => { this.error = error; this.wake(); };
    this.closed = () => this.failed(new Error('proxy closed during negotiation'));
    socket.on('data', this.data).on('error', this.failed).on('end', this.closed).on('close', this.closed);
  }
  wake() { this.waiter?.(); this.waiter = null; }
  async read(size) {
    while (this.buffer.length < size) {
      if (this.error) throw this.error;
      await new Promise(resolve => { this.waiter = resolve; });
    }
    const bytes = this.buffer.subarray(0, size);
    this.buffer = this.buffer.subarray(size);
    return bytes;
  }
  async headers() {
    while (!this.buffer.includes('\r\n\r\n')) {
      if (this.buffer.length > 64 * 1024) throw new Error('proxy headers too large');
      if (this.error) throw this.error;
      await new Promise(resolve => { this.waiter = resolve; });
    }
    return this.read(this.buffer.indexOf('\r\n\r\n') + 4);
  }
  release() {
    this.socket.pause();
    this.socket.off('data', this.data).off('error', this.failed).off('end', this.closed).off('close', this.closed);
    if (this.buffer.length) this.socket.unshift(this.buffer);
  }
}

function connectSocket(proxy, created) {
  return new Promise((resolve, reject) => {
    const host = proxy.hostname.replace(/^\[|\]$/g, '');
    const socket = proxy.protocol === 'https:'
      ? tls.connect({ host, port: Number(proxy.port || 443), servername: net.isIP(host) ? undefined : host, rejectUnauthorized: true })
      : net.connect({ host, port: Number(proxy.port || (proxy.protocol === 'socks5:' ? 1080 : 80)) });
    created(socket);
    const closed = () => reject(new Error('proxy closed before connect'));
    socket.once('error', reject).once('close', closed);
    socket.once(proxy.protocol === 'https:' ? 'secureConnect' : 'connect', () => { socket.off('error', reject).off('close', closed); resolve(socket); });
  });
}

function ipv6Bytes(address) {
  const halves = address.split('::');
  const left = halves[0] ? halves[0].split(':') : [];
  const right = halves[1] ? halves[1].split(':') : [];
  const parts = halves.length === 2 ? [...left, ...Array(8 - left.length - right.length).fill('0'), ...right] : left;
  const bytes = Buffer.alloc(16);
  parts.forEach((part, i) => bytes.writeUInt16BE(parseInt(part, 16), i * 2));
  return bytes;
}

export async function openTunnel(target, proxyUrl, timeout) {
  const proxy = new URL(proxyUrl);
  if (!['http:', 'https:', 'socks5:'].includes(proxy.protocol)) throw new Error('unsupported proxy protocol');
  let socket, reader;
  // Includes DNS, proxy TCP/TLS and negotiation; clean up even on timeout.
  let expired = false;
  let rejectTimeout;
  const deadline = new Promise((_, reject) => { rejectTimeout = reject; });
  const timer = setTimeout(() => { expired = true; socket?.destroy(); rejectTimeout(new Error('proxy connect timeout')); }, timeout);
  const negotiate = async () => {
    socket = await connectSocket(proxy, created => { socket = created; });
    if (expired) { socket.destroy(); throw new Error('proxy connect timeout'); }
    socket.on('error', () => {});
    reader = new SocketReader(socket);
    const host = target.hostname.replace(/^\[|\]$/g, '');
    const port = Number(target.port || (target.protocol === 'https:' ? 443 : 80));
    if (proxy.protocol !== 'socks5:') {
      const authority = `${net.isIP(host) === 6 ? '[' + host + ']' : host}:${port}`;
      const username = decodeURIComponent(proxy.username), password = decodeURIComponent(proxy.password);
      const auth = username || password ? `Proxy-Authorization: Basic ${Buffer.from(username + ':' + password).toString('base64')}\r\n` : '';
      socket.write(`CONNECT ${authority} HTTP/1.1\r\nHost: ${authority}\r\n${auth}\r\n`);
      const response = (await reader.headers()).toString('latin1');
      if (!/^HTTP\/1\.[01] 200(?: |\r)/.test(response)) throw new Error('proxy CONNECT rejected');
    } else {
      const username = Buffer.from(decodeURIComponent(proxy.username)), password = Buffer.from(decodeURIComponent(proxy.password));
      if (username.length > 255 || password.length > 255) throw new Error('SOCKS credentials too long');
      socket.write(username.length || password.length ? Buffer.from([5, 2, 0, 2]) : Buffer.from([5, 1, 0]));
      const greeting = await reader.read(2);
      if (greeting[0] !== 5 || ![0, 2].includes(greeting[1])) throw new Error('SOCKS authentication rejected');
      if (greeting[1] === 2) {
        socket.write(Buffer.concat([Buffer.from([1, username.length]), username, Buffer.from([password.length]), password]));
        const auth = await reader.read(2);
        if (auth[0] !== 1 || auth[1] !== 0) throw new Error('SOCKS authentication rejected');
      }
      // reqwest socks5:// resolves locally (socks5h:// would resolve remotely).
      const address = net.isIP(host) ? host : (await lookup(host)).address;
      const ip = net.isIP(address) === 6 ? ipv6Bytes(address) : Buffer.from(address.split('.').map(Number));
      const portBytes = Buffer.alloc(2); portBytes.writeUInt16BE(port);
      socket.write(Buffer.concat([Buffer.from([5, 1, 0, ip.length === 16 ? 4 : 1]), ip, portBytes]));
      const reply = await reader.read(4);
      if (reply[0] !== 5 || reply[1] !== 0) throw new Error('SOCKS CONNECT rejected');
      const size = reply[3] === 1 ? 4 : reply[3] === 4 ? 16 : reply[3] === 3 ? (await reader.read(1))[0] : -1;
      if (size < 0) throw new Error('invalid SOCKS address');
      await reader.read(size + 2);
    }
    reader.release(); reader = null;
    return socket;
  };
  try { return await Promise.race([negotiate(), deadline]); }
  catch (error) { reader?.release(); socket?.destroy(); throw error; }
  finally { clearTimeout(timer); }
}
