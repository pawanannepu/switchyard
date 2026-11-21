import { afterEach, describe, expect, it, vi } from "vitest";
import { decodeBase64, encodeBase64 } from "./base64.ts";

const bytes = (...values: number[]) => new Uint8Array(values);

describe("base64", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("should round trip every byte value", () => {
    const all = new Uint8Array(256).map((_, i) => i);
    expect(decodeBase64(encodeBase64(all))).toEqual(all);
  });

  it("should match the standard alphabet with padding", () => {
    expect(encodeBase64(bytes(0x66, 0x6f))).toBe("Zm8=");
    expect(decodeBase64("Zm8=")).toEqual(bytes(0x66, 0x6f));
  });

  it("should handle empty input", () => {
    expect(encodeBase64(new Uint8Array())).toBe("");
    expect(decodeBase64("")).toEqual(new Uint8Array());
  });

  it("should encode payloads larger than one fallback chunk", () => {
    // 64 KiB is the run.input cap; the fallback encodes in 32 KiB chunks.
    const big = new Uint8Array(64 * 1024).map((_, i) => (i * 7) % 256);
    expect(decodeBase64(encodeBase64(big))).toEqual(big);
  });

  it("should use the native codecs when the runtime has them", () => {
    const decoded = bytes(1, 2, 3);
    const fromBase64 = vi.fn(() => decoded);
    vi.stubGlobal("Uint8Array", Object.assign(class extends Uint8Array {}, { fromBase64 }));
    expect(decodeBase64("AQID")).toBe(decoded);
    expect(fromBase64).toHaveBeenCalledWith("AQID");
  });
});
