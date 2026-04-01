export interface ImgProxyConfig {
  url: string;
  key: string;
  salt: string;
}

export interface ImgProxyOptions {
  width?: number;
  height?: number;
  square?: boolean;
}

export const DEFAULT_IMGPROXY_CONFIG: ImgProxyConfig = {
  url: 'https://imgproxy.iris.to',
  key: 'f66233cb160ea07078ff28099bfa3e3e654bc10aa4a745e12176c433d79b8996',
  salt: '5e608e60945dcd2a787e8465d76ba34149894765061d39287609fb9d776caa0c',
};

function urlSafeBase64(bytes: Uint8Array): string {
  const binString = Array.from(bytes, (byte) => String.fromCodePoint(byte)).join('');
  return btoa(binString).replace(/=/g, '').replace(/\+/g, '-').replace(/\//g, '_');
}

function hexToBytes(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2);
  for (let index = 0; index < bytes.length; index += 1) {
    bytes[index] = parseInt(hex.slice(index * 2, index * 2 + 2), 16);
  }
  return bytes;
}

function concatBytes(...arrays: Uint8Array[]): Uint8Array {
  const totalLength = arrays.reduce((sum, array) => sum + array.length, 0);
  const result = new Uint8Array(totalLength);
  let offset = 0;
  for (const array of arrays) {
    result.set(array, offset);
    offset += array.length;
  }
  return result;
}

function bytesToArrayBuffer(bytes: Uint8Array): ArrayBuffer {
  const copy = new Uint8Array(bytes.byteLength);
  copy.set(bytes);
  return copy.buffer;
}

async function signUrl(path: string, key: string, salt: string): Promise<string> {
  const encoder = new TextEncoder();
  const keyBytes = hexToBytes(key);
  const saltBytes = hexToBytes(salt);
  const data = concatBytes(saltBytes, encoder.encode(path));

  const cryptoKey = await crypto.subtle.importKey(
    'raw',
    bytesToArrayBuffer(keyBytes),
    { name: 'HMAC', hash: 'SHA-256' },
    false,
    ['sign'],
  );

  const signature = await crypto.subtle.sign('HMAC', cryptoKey, bytesToArrayBuffer(data));
  return urlSafeBase64(new Uint8Array(signature));
}

const urlCache = new Map<string, string>();

export async function generateProxyUrlAsync(
  originalSrc: string,
  options: ImgProxyOptions = {},
  config: ImgProxyConfig = DEFAULT_IMGPROXY_CONFIG,
): Promise<string> {
  try {
    if (
      originalSrc.startsWith(config.url) ||
      originalSrc.startsWith('data:') ||
      originalSrc.startsWith('blob:')
    ) {
      return originalSrc;
    }

    try {
      new URL(originalSrc);
    } catch {
      return originalSrc;
    }

    const cacheKey = `${originalSrc}:${options.width}:${options.height}:${options.square}`;
    const cached = urlCache.get(cacheKey);
    if (cached) return cached;

    const encoder = new TextEncoder();
    const encodedUrl = urlSafeBase64(encoder.encode(originalSrc));

    const transforms: string[] = [];
    if (options.width || options.height) {
      const resizeType = options.square ? 'fill' : 'fit';
      const width = options.width || options.height!;
      const height = options.height || options.width!;
      transforms.push(`rs:${resizeType}:${width}:${height}`);
      transforms.push('dpr:2');
    } else {
      transforms.push('dpr:2');
    }

    const path = `/${transforms.join('/')}/${encodedUrl}`;
    const signature = await signUrl(path, config.key, config.salt);
    const proxied = `${config.url}/${signature}${path}`;
    urlCache.set(cacheKey, proxied);
    return proxied;
  } catch (error) {
    console.error('Failed to generate proxy URL:', error);
    return originalSrc;
  }
}
