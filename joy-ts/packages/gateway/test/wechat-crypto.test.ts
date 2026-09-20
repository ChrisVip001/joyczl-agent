import assert from "node:assert/strict";
import test from "node:test";

import {
  aesKeyOf,
  decryptMessage,
  encryptMessage,
  signatureOf,
} from "../src/wechat-crypto.ts";

// 官方文档「消息加解密说明」那页上的例子。这几条是这次唯一能证明
// 「我们的算法跟他家一样」的东西 —— 其余的都只能证明自己跟自己对得上。
// 文档里的 EncodingAESKey 就是 43 个 A。
const DOC_KEY = "A".repeat(43);
const DOC_APPID = "wxba5fad812f8e6fb9";

test("签名：官方文档那三条都算得出来", () => {
  // 明文模式入站：token + timestamp + nonce。
  assert.equal(
    signatureOf("AAAAA", "1714037059", "486452656"),
    "899cf89e464efb63f54ddac96b0a0a235f53aa78",
  );

  // 回包：多一个 Encrypt。这条同时钉住了「四个元素一起排序」。
  assert.equal(
    signatureOf(
      "AAAAA",
      "1713424427",
      "415670741",
      "ELGduP2YcVatjqIS+eZbp80MNLoAUWvzzyJxgGzxZO/5sAvd070Bs6qrLARC9nVHm48Y4hyRbtzve1L32tmxSQ==",
    ),
    "1b9339964ed2e271e7c7b6ff2b0ef902fc94dea1",
  );

  // 参数给的顺序不该影响结果 —— 里面本来就排过序了。
  assert.equal(
    signatureOf("486452656", "AAAAA", "1714037059"),
    "899cf89e464efb63f54ddac96b0a0a235f53aa78",
  );
});

test("解密：官方文档那条密文解得开", () => {
  // 这一条一次钉住四件事：AESKey 是「补一个 = 再 base64 解」、CBC 与
  // IV 取前 16 字节、补齐按 32 字节、以及明文前面那 16+4 字节的布局。
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
  // 明文体长 = 16 + 4 + msg + appid。用两个字符的 appId，再给一条
  // 10 字节的正文，正好是 32 的整数倍 —— 这时候**必须补满 32 个字节**，
  // 补 0 个的话微信那边解出来尾巴上什么都没有，它会当这条坏了。
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
  // 不检查的话，症状是「验签不过」或者「解出来是乱码」，而原因在几十行之外。
  assert.throws(() => aesKeyOf("A".repeat(40)), /应该是 32/);
  assert.throws(() => aesKeyOf("A".repeat(44)), /应该是 32/);
  assert.equal(aesKeyOf(DOC_KEY).length, 32);

  // 说清这条检查管不到哪儿：它只看字节数。而**位数一样、里面有个错字**的
  // 密钥（同样是 43 个字符）照样解出 32 字节，这里一声不响地放过 —— 那种
  // 错只能等解密之后那条 appid 校验兜住，或者干脆靠「怎么都解不开」发现。
  assert.equal(aesKeyOf(`${"A".repeat(42)}B`).length, 32);
});
