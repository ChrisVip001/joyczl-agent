import assert from "node:assert/strict";
import test from "node:test";

import {
  METHOD_CONTROL,
  METHOD_DATA,
  decodeFrame,
  emptyFrame,
  encodeFrame,
  headerValue,
} from "../src/lark-proto.ts";
import type { LarkFrame } from "../src/lark-proto.ts";

function frameWith(patch: Partial<LarkFrame>): LarkFrame {
  return { ...emptyFrame(), ...patch };
}

/**
 * 手算的一串字节，钉住**全部字段号**。
 *
 * 这是这个文件里最要紧的一条。手写编解码最大的风险是「字段号写错了」，
 * 而它的表现是「连上了但什么都不来」—— 没有报错，没有堆栈，只有安静。
 * 所以这里把一帧 ping 的二进制手工摊开：字节对不上的话，就是字段号或
 * wire type 错了。
 *
 *   08 01       字段 1（varint）= 1        SeqID
 *   10 00       字段 2（varint）= 0        LogID
 *   18 01       字段 3（varint）= 1        service
 *   20 00       字段 4（varint）= 0        method（控制帧）
 *   2a 0c       字段 5（bytes）长 12       headers[0]
 *     0a 04 74 79 70 65   字段 1 = "type"
 *     12 04 70 69 6e 67   字段 2 = "ping"
 */
const PING_BYTES = [
  0x08, 0x01,
  0x10, 0x00,
  0x18, 0x01,
  0x20, 0x00,
  0x2a, 0x0c,
  0x0a, 0x04, 0x74, 0x79, 0x70, 0x65,
  0x12, 0x04, 0x70, 0x69, 0x6e, 0x67,
];

test("编码：一帧 ping 的字节是手算的那一串", () => {
  const frame = frameWith({
    seqId: 1n,
    service: 1,
    method: METHOD_CONTROL,
    headers: [{ key: "type", value: "ping" }],
  });
  assert.deepEqual([...encodeFrame(frame)], PING_BYTES);
});

test("解码：手算的那一串读回来是一样的", () => {
  const frame = decodeFrame(Uint8Array.from(PING_BYTES));
  assert.ok(frame !== null);
  assert.equal(frame.seqId, 1n);
  assert.equal(frame.logId, 0n);
  assert.equal(frame.service, 1);
  assert.equal(frame.method, METHOD_CONTROL);
  assert.deepEqual(frame.headers, [{ key: "type", value: "ping" }]);
  assert.equal(frame.payload, undefined);
});

test("往返：数据帧该带的全带回来", () => {
  const payload = new TextEncoder().encode('{"hello":"世界"}');
  const frame = frameWith({
    seqId: 42n,
    logId: 7n,
    service: 33_554_678,
    method: METHOD_DATA,
    headers: [
      { key: "type", value: "event" },
      { key: "message_id", value: "om_abc" },
      { key: "sum", value: "1" },
      { key: "seq", value: "0" },
    ],
    payload,
  });

  const back = decodeFrame(encodeFrame(frame));
  assert.ok(back !== null);
  assert.equal(back.seqId, 42n);
  assert.equal(back.logId, 7n);
  assert.equal(back.service, 33_554_678);
  assert.equal(back.method, METHOD_DATA);
  assert.deepEqual(back.headers, frame.headers);
  assert.deepEqual(back.payload, payload);
});

test("往返：headers 的顺序留着", () => {
  // ACK 是把原帧整体回显，所以顺序错了也还是「能对上」，但排过序的 headers
  // 会让人怀疑中间是不是丢过一项。顺序是保序的，这条把它说死。
  const headers = [
    { key: "a", value: "1" },
    { key: "b", value: "2" },
    { key: "a", value: "3" },
  ];
  const back = decodeFrame(encodeFrame(frameWith({ headers })));
  assert.ok(back !== null);
  assert.deepEqual(back.headers, headers);
});

test("往返：字段乱序发过来也认", () => {
  // protobuf 不要求字段按号排。服务端怎么排都得读得出来。
  const shuffled = Uint8Array.from([
    0x20, 0x01, // method = 1
    0x18, 0x0e, // service = 14
    0x08, 0x05, // seqId = 5
  ]);
  const frame = decodeFrame(shuffled);
  assert.ok(frame !== null);
  assert.equal(frame.method, METHOD_DATA);
  assert.equal(frame.service, 14);
  assert.equal(frame.seqId, 5n);
});

test("往返：logId 超过 2^53 也不丢精度", () => {
  // 这是 ACK 必须原样回填的那个字段。用 number 装的话这里就已经悄悄变了 ——
  // 回填一个被改过的 id，平时看不出，出事时查不动。
  const big = 18_446_744_073_709_551_615n; // 2^64 - 1
  // 这个量级下 `number` 连加一都记不住 —— 精度就是这么丢的。
  assert.equal(Number(big) + 1, Number(big));

  const back = decodeFrame(encodeFrame(frameWith({ logId: big })));
  assert.ok(back !== null);
  assert.equal(back.logId, big);
});

test("varint：几个边界值都对", () => {
  for (const seqId of [0n, 1n, 127n, 128n, 300n, 16_383n, 16_384n]) {
    const back = decodeFrame(encodeFrame(frameWith({ seqId })));
    assert.ok(back !== null);
    assert.equal(back.seqId, seqId);
  }
});

test("service 是负数时按 int32 截回来", () => {
  // 协议里 service 是正数，但负数的编码形式（10 字节、符号扩展到 64 位）
  // 必须认得 —— 认不出的话，畸形/错位的输入会被解成一个巨大的正数。
  const back = decodeFrame(encodeFrame(frameWith({ service: -1 })));
  assert.ok(back !== null);
  assert.equal(back.service, -1);
  // -1 的值编出来是 10 个字节的 varint（符号扩展到 64 位），加上 tag 一共 11；
  // 另外三个 0 字段各 2 字节。按「普通 varint」写会只有 8 —— 这就是差别。
  assert.equal(encodeFrame(frameWith({ service: -1 })).length, 17);
});

test("认不出的字段：跳过它，别的照读", () => {
  // 飞书哪天加个字段，不该表现成「连着连着突然什么都不来了」。protobuf
  // 自己的规矩就是跳过。这里塞了三种 wire type 的陌生字段。
  const extra = Uint8Array.from([
    0x50, 0x2a, // 字段 10，varint
    0x59, 1, 2, 3, 4, 5, 6, 7, 8, // 字段 11，fixed64
    0x65, 1, 2, 3, 4, // 字段 12，fixed32
  ]);
  const frame = decodeFrame(
    Uint8Array.from([...PING_BYTES, ...extra, 0x08, 0x02]),
  );
  assert.ok(frame !== null);
  assert.equal(frame.method, METHOD_CONTROL);
  assert.deepEqual(frame.headers, [{ key: "type", value: "ping" }]);
  // 跳过完还得接着读：这一帧最后那个 seqId 覆盖了前面那个 1。
  assert.equal(frame.seqId, 2n);
});

test("畸形帧一律返回 null，不抛", () => {
  // 一帧坏数据不该有能力把整条连接带走 —— 调用方记一行日志、丢掉、接着读。
  const cases: Array<[string, number[]]> = [
    ["只剩一个 tag", [0x08]],
    ["varint 没写完", [0x08, 0x80]],
    ["varint 超过 10 字节", [0x08, ...Array<number>(11).fill(0x80), 0x01]],
    ["长度前缀指着数组外面", [0x2a, 0x7f, 0x01, 0x02]],
    ["header 里长度说谎", [0x2a, 0x03, 0x0a, 0x09, 0x41]],
    ["wire type 认不出", [0x0f, 0x01]],
  ];

  for (const [what, bytes] of cases) {
    assert.equal(decodeFrame(Uint8Array.from(bytes)), null, what);
  }
});

test("零字节读出来是「全默认」，不是 null", () => {
  // 这不是畸形 —— protobuf 里空消息就是「每个字段都是默认值」的合法编码。
  // 适配器那边不用特判：`method` 是 0、也没有 `type`，走过去就被忽略了。
  const frame = decodeFrame(new Uint8Array(0));
  assert.ok(frame !== null);
  assert.deepEqual(frame, emptyFrame());
});

test("长度前缀说谎时，一帧都别给我", () => {
  // 这个设计是故意的：半截的帧会被拿去 ACK 一个错的东西，比丢掉更糟。
  const good = encodeFrame(
    frameWith({ headers: [{ key: "type", value: "event" }] }),
  );
  const frame = decodeFrame(good);
  assert.ok(frame !== null);

  // 只说对了前一半。
  assert.equal(decodeFrame(good.subarray(0, good.length - 1)), null);
});

test("payload 是空的意思就是没有 payload", () => {
  // proto3 里空 bytes 不写进线里，读回来是 undefined 而不是空数组 ——
  // 适配器那句 `payload === undefined` 的判断靠的就是这条。
  const back = decodeFrame(
    encodeFrame(frameWith({ payload: new Uint8Array(0) })),
  );
  assert.ok(back !== null);
  assert.equal(back.payload, undefined);
});

test("headerValue：取第一个，没有就是空串", () => {
  const frame = frameWith({
    headers: [
      { key: "type", value: "event" },
      { key: "type", value: "card" },
    ],
  });
  assert.equal(headerValue(frame, "type"), "event");
  assert.equal(headerValue(frame, "message_id"), "");
});

test("坏字节不炸：文本字段按有损解码", () => {
  // 跟官方 SDK 一个态度 —— 一个坏字节换成一个替换字符，而不是把连接带走。
  // 这一段是 header（字段 5）里 key 的前两个字节不合法的 UTF-8。
  const frame = decodeFrame(
    Uint8Array.from([0x2a, 0x06, 0x0a, 0x04, 0xff, 0xfe, 0x74, 0x65]),
  );
  assert.ok(frame !== null);
  assert.equal(frame.headers.length, 1);
  assert.equal(frame.headers[0]?.key, "\u{fffd}\u{fffd}te");
});
