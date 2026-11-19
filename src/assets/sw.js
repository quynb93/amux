// The phone app's service worker: shows amux's push notifications (`amux phone webpush`)
// and opens the run when one is tapped. It caches nothing: the app is always live.
"use strict";
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));

self.addEventListener("push", (e) => {
  let d = {};
  try { d = e.data ? e.data.json() : {}; } catch (_) { d = { body: e.data ? e.data.text() : "" }; }
  // A newer message about the same run replaces the older one.
  e.waitUntil(self.registration.showNotification(d.title || "amux", {
    body: d.body || "",
    tag: d.tag || undefined,
    renotify: !!d.tag,
    icon: "/icon.svg",
    data: { url: d.url || "/" },
  }));
});

self.addEventListener("notificationclick", (e) => {
  e.notification.close();
  const url = new URL((e.notification.data && e.notification.data.url) || "/", self.location.origin);
  e.waitUntil((async () => {
    const wins = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    const win = wins.find((w) => new URL(w.url).origin === url.origin);
    if (win) {
      await win.focus();
      win.postMessage({ open: url.href });
      return;
    }
    await self.clients.openWindow(url.href);
  })());
});
