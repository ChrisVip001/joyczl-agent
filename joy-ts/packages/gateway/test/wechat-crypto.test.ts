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

test("signature: all three from the official docs compute", () => {
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

test("decrypt: the official docs' ciphertext opens", () => {
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

test("encrypt then decrypt: text, emoji and the empty string all come back", () => {
  for (const message of ["", "hm", "fine, let me look at this file 🎉", "a".repeat(500)]) {
    const encrypted = encryptMessage(message, {
      encodingAesKey: DOC_KEY,
      appId: DOC_APPID,
    });
    assert.equal(
      decryptMessage(encrypted, { encodingAesKey: DOC_KEY, appId: DOC_APPID }),
      message,
      `"${message.slice(0, 20)}" did not come back unchanged`,
    );
  }
});

test("padding edge: an exact multiple still gets a whole block", () => {
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

test("a mismatched AppID throws: that is the only proof the message was really for me", () => {
  const encrypted = encryptMessage("hello", {
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

test("a wrong EncodingAESKey length says so on the spot", () => {
  // Unchecked, the symptom is "signature check failed" or "decrypted to garbage", with the cause dozens of lines away.
  assert.throws(() => aesKeyOf("A".repeat(40)), /expected 32/);
  assert.throws(() => aesKeyOf("A".repeat(44)), /expected 32/);
  assert.equal(aesKeyOf(DOC_KEY).length, 32);

  // Say what this check can't catch: it only counts bytes. A key with **the same length but one
  // wrong character** (still 43 chars) also yields 32 bytes and passes silently here — that error
  // is only caught by the appid check after decryption, or by "it never decrypts at all".
  assert.equal(aesKeyOf(`${"A".repeat(42)}B`).length, 32);
});
