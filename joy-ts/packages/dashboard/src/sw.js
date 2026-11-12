// Joy 的 service worker：页面与样式网络优先，其余静态资源缓存优先，/api 永远直连。
//
// 三条纪律：
// 1. /api/* 一帧也不缓存——对话、记忆、审批都是活的；服务端已发 no-store，这里同样直通。
// 2. index.html / style.css 网络优先：缓存优先会造成「改好了但浏览器还在显示旧版」，
//    布局修复尤其明显。网络拿不到才回落缓存，离线照样能开。
// 3. 其余静态资源缓存优先、后台更新；版本号一变，旧缓存整体作废。
// Bump on every shipped frontend change: a stale cache means a stale layout.
// Bump on every shipped change: a stale cache keeping the old main.js alive is the one bug
// the version string exists to prevent (it has happened once).
const VERSION = "joy-v29";
const CORE = ["./", "./index.html", "./style.css", "./icon.svg", "./manifest.webmanifest"];

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

  // 导航请求与样式必须先看网络：一次刷新就要拿到新布局，不能等后台更新悄悄换上。
  const pageLike = url.pathname.endsWith("/") || /\.(?:html|css)$/.test(url.pathname);
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
      return pageLike ? (await fresh) ?? cached : cached ?? fresh;
    }),
  );
});
