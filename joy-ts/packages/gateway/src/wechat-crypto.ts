import {
  createCipheriv,
  createDecipheriv,
  createHash,
  randomBytes,
} from "node:crypto";

/**
 * WeChat message encryption, decryption, and signatures.
 *
 * A separate file of pure functions, because this is the least tolerant of "close enough":
 * one byte off and WeChat silently ignores you — it never says "your padding is wrong", it just
 * times out or errors with no information. So no HTTP, no business logic in here: one input,
 * one output, testable without starting a server.
 */

/**
 * WeChat pads in **32-byte** blocks, not AES's built-in 16.
 *
 * This is the easiest one to get led astray by "standard PKCS#7": the ciphertext still decrypts,
 * the content is right, just a few bytes off at the tail — and what WeChat sees is "signature check failed".
 */
const BLOCK = 32;

/**
 * Signature: `SHA1(sorted lexicographically, concatenated directly)`.
 *
 * Inbound plaintext uses `signature`, inbound encrypted uses `msg_signature` (plus an `Encrypt`),
 * the reply uses `MsgSignature` — the same rule with different elements to sort, so it's written
 * once and callers decide how many parts to pass.
 */
export function signatureOf(...parts: string[]): string {
  return createHash("sha1").update([...parts].sort().join("")).digest("hex");
}

/** `EncodingAESKey` (43 chars) → the 32-byte `AESKey`. */
export function aesKeyOf(encodingAesKey: string): Buffer {
  const key = Buffer.from(`${encodingAesKey}=`, "base64");
  if (key.length !== 32) {
    // `Buffer.from` **silently drops** invalid base64 instead of erroring, so the length check
    // is the only line of defense. When the length is far off, the symptom drifts all the way to
    // "signature check failed", and this is the only place that can name a cause.
    //
    // But it doesn't catch everything: a 42-char (one short) input also yields 32 bytes (40 give
    // 30, the extra 3 give 2) — that error can only be caught by the appid check after decryption.
    throw new Error(
      `EncodingAESKey 解出来是 ${key.length} 字节，应该是 32 —— 多半是复制的时候没弄干净（它应该是 43 个字符）。`,
    );
  }
  return key;
}

export interface WeChatCryptoOptions {
  encodingAesKey: string;
  appId: string;
}

/** Pad to a multiple of 32 per WeChat's rule (a full block even when already aligned). */
function pad(data: Buffer): Buffer {
  const size = BLOCK - (data.length % BLOCK);
  return Buffer.concat([data, Buffer.alloc(size, size)]);
}

/**
 * Encrypt into the Base64 string WeChat expects.
 *
 * Plaintext layout: `random(16) + msg_len(4, network order) + msg + appid`. The 16 random bytes
 * are WeChat's requirement (the same reply must not encrypt identically every time), the length
 * field is **network order**, and the trailing appid lets the receiver verify "this was for me".
 */
export function encryptMessage(
  message: string,
  options: WeChatCryptoOptions,
): string {
  const key = aesKeyOf(options.encodingAesKey);
  const body = Buffer.from(message, "utf8");
  const head = Buffer.alloc(20);
  randomBytes(16).copy(head, 0);
  head.writeUInt32BE(body.length, 16);

  const full = pad(
    Buffer.concat([head, body, Buffer.from(options.appId, "utf8")]),
  );
  const cipher = createCipheriv("aes-256-cbc", key, key.subarray(0, 16));
  // We padded ourselves; don't let node pad again in 16-byte blocks — that adds a whole block
  // and WeChat sees "garbage at the end of the decrypted output".
  cipher.setAutoPadding(false);
  return Buffer.concat([cipher.update(full), cipher.final()]).toString("base64");
}

/**
 * Decrypt back. **The trailing appid must be verified** — decrypting isn't the end. That check
 * is the only evidence "this message was really sent to this account"; skipping it leaves the
 * question "is the key right" to luck.
 */
export function decryptMessage(
  encrypted: string,
  options: WeChatCryptoOptions,
): string {
  const key = aesKeyOf(options.encodingAesKey);
  const decipher = createDecipheriv("aes-256-cbc", key, key.subarray(0, 16));
  decipher.setAutoPadding(false);

  const plain = Buffer.concat([
    decipher.update(Buffer.from(encrypted, "base64")),
    decipher.final(),
  ]);

  // The pad length is written in the last byte itself.
  const size = plain.readUInt8(plain.length - 1);
  const full = plain.subarray(0, plain.length - size);
  const length = full.readUInt32BE(16);
  const message = full.subarray(20, 20 + length).toString("utf8");
  const appId = full.subarray(20 + length).toString("utf8");

  if (appId !== options.appId) {
    throw new Error(
      `解密出来的 AppID 是 ${appId}，跟配的 ${options.appId} 对不上 —— 这条不是发给这个公众号的（或者跟别的号串了密钥）。`,
    );
  }
  return message;
}
