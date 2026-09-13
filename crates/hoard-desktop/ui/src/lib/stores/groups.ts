/**
 * The groups this account belongs to, and who hosts each shared world.
 *
 * Two small stores fed by the service, on the `devices.ts` model: the page
 * asks once (`refreshGroups`, `refreshLease`) and the `agent://world-*`
 * events keep `leases` current afterwards. Nothing here decides a lease
 * rule; it mirrors what the engine reported so the cards can draw it.
 */
import { get, writable, type Readable } from "svelte/store";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import * as api from "../api";
import type { AgentEvent, Group } from "../api";
import { auth } from "./auth";

/** The lease on one shared save, as last heard. `holder` and `since` only
 *  mean something for `other` (who, since when). */
export type LeaseState = {
  state: "unknown" | "free" | "mine" | "other";
  holder?: string;
  since?: string;
  /** The holder has pushed under the lease: it cannot be taken over. Only
   *  known from a server read; an event leaves it undefined. */
  pushed?: boolean;
};

export const groups = writable<Group[]>([]);
export const leases = writable<Record<string, LeaseState>>({});

/** This account's user id, to tell "yours" from "<owner>'s". Hand-rolled
 *  rather than `derived(auth)`: `auth` imports `agent`, which imports this
 *  file, so `auth` must not be touched while the module loads. */
export const myUserId: Readable<string | null> = {
  subscribe: (run) => auth.subscribe(($a) => run($a.user?.user_id ?? null)),
};

/** Ask the service for the group list. Silent on failure: a list that did
 *  not load stays as it was, the page shows its own error. */
export async function refreshGroups(): Promise<Group[]> {
  const list = await api.listGroups();
  groups.set(list);
  return list;
}

/** Read one save's lease back from the server and fold it into `leases`. */
export async function refreshLease(saveId: string): Promise<LeaseState> {
  const view = await api.getLease(saveId);
  const next = leaseFromView(view);
  patch(saveId, next);
  return next;
}

/** The store's shape for a server row: nobody, this machine, or someone. A
 *  lease that went quiet reads as free, which is what the server does too. */
export function leaseFromView(view: api.LeaseView): LeaseState {
  const lease = view.lease;
  if (!lease || !lease.live) return { state: "free" };
  if (view.here) return { state: "mine", since: lease.acquired_at };
  return {
    state: "other",
    holder: lease.holder_username,
    since: lease.acquired_at,
    pushed: lease.pushed_since,
  };
}

function patch(saveId: string, next: LeaseState) {
  leases.update((m) => ({ ...m, [saveId]: next }));
}

/** Fold one world event into `leases`. `at` is when it happened (ISO), so a
 *  replayed hold reads as old as it is. Exported for the reducer tests. */
export function applyWorldEvent(ev: AgentEvent, at: string = new Date().toISOString()) {
  switch (ev.type) {
    case "world_claimed":
      // A view role changes nothing about who hosts; the lease is whoever's it
      // was, and `refreshLease` says. Only a host claim is a hold here.
      if (ev.role === "host") patch(ev.save_id, { state: "mine", since: at });
      break;
    case "world_released":
      patch(ev.save_id, { state: "free" });
      break;
    case "world_hosted_elsewhere":
      patch(ev.save_id, { state: "other", holder: ev.holder, since: at });
      break;
    case "world_lease_lost": {
      // Forced by a member or expired: not ours any more, and who holds it
      // now is the server's to say. Unknown until asked.
      patch(ev.save_id, { state: "unknown" });
      break;
    }
    default:
      break;
  }
}

let unlisteners: UnlistenFn[] = [];

/** Subscribe to the world events. Called from `subscribeAgent()` before the
 *  relay is attached, so the backlog lands on a listening store. The claim
 *  prompt's two topics (`agent://world-claim-wanted`,
 *  `agent://view-session-writing`) are the prompt's, not this store's: they
 *  are registered by the modal that answers them. */
export async function subscribeWorldEvents(): Promise<void> {
  await unsubscribeWorldEvents();
  const topics = [
    "agent://world-claimed",
    "agent://world-released",
    "agent://world-hosted-elsewhere",
    "agent://world-lease-lost",
    // "agent://world-claim-wanted"  -> the claim prompt (step 5)
    // "agent://view-session-writing" -> the claim prompt (step 5)
  ];
  try {
    unlisteners = await Promise.all(
      topics.map((t) =>
        listen<AgentEvent>(t, (event) => applyWorldEvent(event.payload)),
      ),
    );
  } catch {
    /* Tauri not available (dev in browser): the stores stay empty. */
  }
}

export async function unsubscribeWorldEvents(): Promise<void> {
  for (const u of unlisteners) {
    try {
      u();
    } catch {
      /* ignore */
    }
  }
  unlisteners = [];
}

/** The lease chip's state for a save, `unknown` until something said. */
export function leaseOf(saveId: string): LeaseState {
  return get(leases)[saveId] ?? { state: "unknown" };
}
