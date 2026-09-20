import {
  createCipheriv,
  createDecipheriv,
  createHash,
  randomBytes,
} from "node:crypto";

/**
 * 微信消息的加解密与签名。
 *
 * 单独一个文件、而且全是纯函数，因为它是最不能「差不多就行」的一段：
 * 差一个字节微信那头就是静默不理你 —— 它不会回一句「你补齐错了」，
 * 只会超时，或者报一句什么信息都没有的错。所以这里不掺 HTTP、不掺业务，
 * 一进一出，测起来不用起服务器。
 */

/**
 * 微信的补齐按 **32 字节** 一块，不是 AES 自带的 16。
 *
 * 这条最容易被「标准 PKCS#7」带偏：密文照样能解出来、内容也对，只是末尾
 * 多几个或少几个字节 —— 而微信看到的结果就是「验签不过」。
 */
const BLOCK = 32;

/**
 * 签名：`SHA1(按字典序排序后直接拼接)`。
 *
 * 入站明文用 `signature`，入站加密用 `msg_signature`（多带一个 `Encrypt`），
 * 回包用 `MsgSignature` —— 同一条规矩，只是排序的元素不同，所以只写一份，
 * 由调用方决定传几个。
 */
export function signatureOf(...parts: string[]): string {
  return createHash("sha1").update([...parts].sort().join("")).digest("hex");
}

/** `EncodingAESKey`（43 位）→ 32 字节的 `AESKey`。 */
export function aesKeyOf(encodingAesKey: string): Buffer {
  const key = Buffer.from(`${encodingAesKey}=`, "base64");
  if (key.length !== 32) {
    // `Buffer.from` 遇到不合法的 base64 是**默默丢掉**而不是报错，所以长度
    // 检查是唯一的防线。位数差得远的时候症状会一路飘到「验签不过」，而这里
    // 是唯一说得出原因的地方。
    //
    // 但它管不全：少一位的 42 个字符也能解出 32 字节（40 个给 30，多出的 3 个
    // 给 2），那种错只能等解密之后那条 appid 校验兜住。
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

/** 按微信的规矩补齐到 32 的整数倍（正好整除时补满一整块）。 */
function pad(data: Buffer): Buffer {
  const size = BLOCK - (data.length % BLOCK);
  return Buffer.concat([data, Buffer.alloc(size, size)]);
}

/**
 * 加密成微信要的那串 Base64。
 *
 * 明文结构是 `random(16) + msg_len(4, 网络序) + msg + appid`：那 16 字节
 * 随机数是微信要求的（同样的回复不能每次都密文一样），长度字段是**网络序**，
 * 后面的 appid 是给收方校验「这条确实是发给我的」用的。
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
  // 自己补齐了，就别让 node 再按 16 字节补一遍 —— 那会多出一整块，
  // 微信那边就成了「解密出来尾巴上有垃圾」。
  cipher.setAutoPadding(false);
  return Buffer.concat([cipher.update(full), cipher.final()]).toString("base64");
}

/**
 * 解回来。**末尾的 appid 是要校验的**，不是解出来就完事 —— 校验这一步
 * 是「这条消息确实发给这个公众号」的唯一证据，跳过它等于把密钥对不对
 * 这件事交给运气。
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

  // 补齐的字节数就写在最后一个字节上。
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
