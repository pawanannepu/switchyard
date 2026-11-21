// Terminal bytes travel as base64 inside JSON. Uses the native codecs where the
// webview has them (Safari 18.2+), with a portable fallback.

interface NativeBase64 {
  fromBase64?: (text: string) => Uint8Array;
}
interface NativeToBase64 {
  toBase64?: () => string;
}

/** Decodes standard, padded base64 (as sent in `data_b64` fields) to bytes. */
export function decodeBase64(text: string): Uint8Array {
  const native = (Uint8Array as unknown as NativeBase64).fromBase64;
  if (native) return native(text);
  const binary = atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

/** Encodes bytes as standard, padded base64 (for `run.input`). */
export function encodeBase64(bytes: Uint8Array): string {
  const native = (bytes as unknown as NativeToBase64).toBase64;
  if (native) return native.call(bytes);
  let binary = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(binary);
}
