// Joy's service worker: network first for pages and styles, cache first for the rest of the
// static assets, and /api always straight through.
//
// Three rules:
// 1. /api/* is never cached, not one frame — conversations, memory and approvals are live; the
//    server already sends no-store, and this goes straight through too.
// 2. index.html / style.css are network first: cache first produces «fixed it, but the browser
//    still shows the old one», most visibly on a layout fix. Only when the network fails does it
//    fall back to the cache, so offline still opens.
// 3. Every other static asset is cache first with a background update; bump the version and the
//    whole old cache is void.
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

  // Navigation requests and styles have to hit the network first: one refresh has to bring the new
// layout, not wait for a background update to swap it in quietly.
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
