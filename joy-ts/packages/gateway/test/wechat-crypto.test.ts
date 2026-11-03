import assert from "node:assert/strict";
import test from "node:test";

import {
  aesKeyOf,
  decryptMessage,
  encryptMessage,
  signatureOf,
} from "../src/wechat-crypto.ts";

// Examples from the official docs' "message encryption" page. These are the only things that
// prove "our algorithm matches theirs" — everything else only proves we agree with ourselves.
// The docs' EncodingAESKey is 43 A's.
const DOC_KEY = "A".repeat(43);
const DOC_APPID = "wxba5fad812f8e6fb9";

test("签名：官方文档那三条都算得出来", () => {
  // Inbound in plain mode: token + timestamp + nonce.
  assert.equal(
    signatureOf("AAAAA", "1714037059", "486452656"),
    "899cf89e464efb63f54ddac96b0a0a235f53aa78",
  );

  // Reply: one more Encrypt. This also pins "sort all four elements together".
  assert.equal(
    signatureOf(
      "AAAAA",
      "1713424427",
      "415670741",
      "ELGduP2YcVatjqIS+eZbp80MNLoAUWvzzyJxgGzxZO/5sAvd070Bs6qrLARC9nVHm48Y4hyRbtzve1L32tmxSQ==",
    ),
    "1b9339964ed2e271e7c7b6ff2b0ef902fc94dea1",
  );

  // Argument order must not affect the result — sorting happens inside anyway.
  assert.equal(
    signatureOf("486452656", "AAAAA", "1714037059"),
    "899cf89e464efb63f54ddac96b0a0a235f53aa78",
  );
});

test("解密：官方文档那条密文解得开", () => {
  // One assertion pinning four things: the AESKey is "append = then base64-decode", CBC with
  // the IV from the first 16 bytes, padding in 32-byte blocks, and the 16+4-byte plaintext layout.
  assert.equal(
    decryptMessage(
      "ELGduP2YcVatjqIS+eZbp80MNLoAUWvzzyJxgGzxZO/5sAvd070Bs6qrLARC9nVHm48Y4hyRbtzve1L32tmxSQ==",
      { encodingAesKey: DOC_KEY, appId: DOC_APPID },
    ),
    '{"demo_resp":"good luck"}',
  );
});

test("加密再解密：中文、emoji、空串都回得来", () => {
  for (const message of ["", "嗯", "好的，我看一下这个文件 🎉", "a".repeat(500)]) {
    const encrypted = encryptMessage(message, {
      encodingAesKey: DOC_KEY,
      appId: DOC_APPID,
    });
    assert.equal(
      decryptMessage(encrypted, { encodingAesKey: DOC_KEY, appId: DOC_APPID }),
      message,
      `「${message.slice(0, 20)}」没能原样回来`,
    );
  }
});

test("补齐的边界：正好整除时要补满一整块", () => {
  // Plaintext length = 16 + 4 + msg + appid. With a two-char appId and a 10-byte message the
  // total is an exact multiple of 32 — then **a full 32-byte pad is mandatory**: with zero pad
  // WeChat's side decrypts to nothing at the tail and treats the message as broken.
  const appId = "wg";
  const message = "0123456789";
  const encrypted = encryptMessage(message, {
    encodingAesKey: DOC_KEY,
    appId,
  });
  assert.equal(Buffer.from(encrypted, "base64").length, 64);
  assert.equal(
    decryptMessage(encrypted, { encodingAesKey: DOC_KEY, appId }),
    message,
  );
});

test("AppID 对不上就抛：这是「这条确实是发给我的」的唯一证据", () => {
  const encrypted = encryptMessage("你好", {
    encodingAesKey: DOC_KEY,
    appId: DOC_APPID,
  });
  assert.throws(
    () =>
      decryptMessage(encrypted, {
        encodingAesKey: DOC_KEY,
        appId: "wx0000000000000000",
      }),
    /AppID/,
  );
});

test("EncodingAESKey 长度不对：当场就说清楚", () => {
  // Unchecked, the symptom is "signature check failed" or "decrypted to garbage", with the cause dozens of lines away.
  assert.throws(() => aesKeyOf("A".repeat(40)), /应该是 32/);
  assert.throws(() => aesKeyOf("A".repeat(44)), /应该是 32/);
  assert.equal(aesKeyOf(DOC_KEY).length, 32);

  // Say what this check can't catch: it only counts bytes. A key with **the same length but one
  // wrong character** (still 43 chars) also yields 32 bytes and passes silently here — that error
  // is only caught by the appid check after decryption, or by "it never decrypts at all".
  assert.equal(aesKeyOf(`${"A".repeat(42)}B`).length, 32);
});
