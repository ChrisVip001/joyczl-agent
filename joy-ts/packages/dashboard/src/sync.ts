// The cross-device pulse: hold /api/events open, turn pings into refreshes.
//
// A ping is a knock — several may arrive in a burst (turn started → approval → done),
// and each one only means "look again". So: coalesce knocks within 500 ms into one
// refresh, and reconnect forever with a flat pause — the server may be restarting, the
// laptop asleep; the channel's whole value is that it is always there when you look.

import { followEvents, type EventPing } from "./api";

export interface SyncHandlers {
  /** Coalesced activity: pull /api/data (and the open session) again. */
  onActivity: (ping: EventPing) => void;
}

export function startSync(handlers: SyncHandlers): void {
  let timer: number | null = null;
  let pending: EventPing | null = null;

  const knock = (ping: EventPing): void => {
    pending = ping;
    if (timer !== null) return;
    timer = window.setTimeout(() => {
      timer = null;
      const burst = pending;
      pending = null;
      if (burst !== null) handlers.onActivity(burst);
    }, 500);
  };

  void (async () => {
    for (;;) {
      try {
        for await (const ping of followEvents()) knock(ping);
      } catch {
        // Dropped (server restart, network blip) — the loop below reconnects.
      }
      await new Promise((resolve) => setTimeout(resolve, 3000));
    }
  })();
}
