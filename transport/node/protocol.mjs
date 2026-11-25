export const MAX_METADATA = 64 * 1024;
export function frame(metadata) {
  const json = Buffer.from(JSON.stringify(metadata));
  if (json.length > MAX_METADATA) throw new Error('metadata exceeds 64 KiB');
  const prefix = Buffer.alloc(4);
  prefix.writeUInt32BE(json.length);
  return Buffer.concat([prefix, json]);
}

// Leave every byte beyond the metadata available to the body consumer.
export async function readMetadata(readable) {
  let buffered = Buffer.alloc(0);
  const iterator = readable.iterator({ destroyOnReturn: false });
  try {
    for await (const chunk of iterator) {
      buffered = Buffer.concat([buffered, chunk]);
      if (buffered.length < 4) continue;
      const length = buffered.readUInt32BE(0);
      if (length > MAX_METADATA) throw new Error('metadata exceeds 64 KiB');
      if (buffered.length < 4 + length) continue;
      const metadata = JSON.parse(buffered.subarray(4, 4 + length));
      const rest = buffered.subarray(4 + length);
      if (rest.length) readable.unshift(rest);
      return metadata;
    }
    throw new Error('truncated metadata');
  } finally { await iterator.return(); }
}

const hopHeaders = new Set(['connection', 'keep-alive', 'proxy-authenticate', 'proxy-authorization', 'te', 'trailer', 'transfer-encoding', 'upgrade']);
export function cleanHeaders(headers, decoded = false) {
  const connection = headers.filter(([key]) => key.toLowerCase() === 'connection').flatMap(([, value]) => value.toLowerCase().split(',').map(s => s.trim()));
  return headers.filter(([key]) => !hopHeaders.has(key.toLowerCase()) && !connection.includes(key.toLowerCase())
    && !(decoded && ['content-encoding', 'content-length'].includes(key.toLowerCase())));
}
