// Joy 的 service worker：静态资源缓存优先，/api 永远直连。
//
// 两条纪律：
// 1. /api/* 一帧也不缓存——对话、记忆、审批都是活的；服务端已发 no-store，这里同样直通。
// 2. 静态资源缓存优先、后台更新（stale-while-revalidate 的手写最小版）：安装包秒开，
//    新版本在下次访问时悄悄换上。版本号一变，旧缓存整体作废。
const VERSION = "joy-v1";
const CORE = ["./", "./index.html", "./style.css", "./main.js", "./icon.svg", "./manifest.webmanifest"];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches.open(VERSION).then((cache) => cache.addAll(CORE)).then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((key) => key !== VERSION).map((key) => caches.delete(key))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (event.request.method !== "GET" || url.pathname.startsWith("/api/")) return;

  event.respondWith(
    caches.open(VERSION).then(async (cache) => {
      const cached = await cache.match(event.request);
      const fresh = fetch(event.request)
        .then((response) => {
          if (response.ok && new URL(response.url).origin === self.location.origin) {
            cache.put(event.request, response.clone());
          }
          return response;
        })
        .catch(() => cached);
      return cached ?? fresh;
    }),
  );
});
