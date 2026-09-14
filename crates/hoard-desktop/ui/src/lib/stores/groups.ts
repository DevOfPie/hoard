/**
 * The groups this account belongs to, who hosts each shared world, and the
 * claim prompt.
 *
 * Small stores fed by the service, on the `devices.ts` model: the page asks
 * once (`refreshGroups`, `refreshLease`) and the `agent://world-*` events
 * keep `leases` current afterwards. Nothing here decides a lease rule; it
 * mirrors what the engine reported so the cards can draw it.
 *
 * The prompt (`prompts`) is the engine's `EngineStatus.prompts`, mirrored.
 * Two windows draw it from the same store: the HUD over the game, which
 * cannot listen (it is born after the event went out) and so adopts it from
 * the snapshot it polls; and the main window, which hears
 * `agent://world-claim-wanted` and then reads the same snapshot, because the
 * event carries the worlds but the status carries the clock. Answering is
 * sending the verb (`claimWorld`, `dismissWorld`); whether to host is never
 * computed here.
 */
import { get, writable, type Readable } from "svelte/store";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { _ } from "svelte-i18n";
import * as api from "../api";
import type { AgentEvent, Group, WorldPrompt, WorldRole } from "../api";
import { prettifySlug } from "../utils/format";
import { isReplaying } from "./agent";
import { auth } from "./auth";
import { pushNotification, updateNotification } from "./notifications";
import { toastInfo } from "./toasts";

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
      // The engine answered for this game (the user, the clock or a write):
      // the question is closed whichever window asked it.
      dropPrompt(ev.game_slug);
      break;
    case "world_released":
      patch(ev.save_id, { state: "free" });
      break;
    case "world_hosted_elsewhere":
      patch(ev.save_id, { state: "other", holder: ev.holder, since: at });
      break;
    case "world_lease_lost":
      patch(
        ev.save_id,
        ev.holder ? { state: "other", holder: ev.holder, since: at } : { state: "free" },
      );
      break;
    case "game_stopped":
      dropPrompt(ev.game_slug);
      break;
    default:
      break;
  }
}

// ── The claim prompt ──────────────────────────────────────────────────────

/** The prompts the engine is still waiting on, as last read. */
export const prompts = writable<WorldPrompt[]>([]);

/** The main window's dialog is up. Only meaningful there; the HUD draws its
 *  panel whenever `prompts` has rows. */
export const claimModalOpen = writable(false);

// The dialog follows the prompts: once none is left, however it was answered
// (either window, the clock, the game stopping), it is closed.
prompts.subscribe((list) => {
  if (list.length === 0) claimModalOpen.set(false);
});

/** Prompts answered from this window, by game, with the `raised_at` they
 *  had. A snapshot read between the answer and the engine's next status still
 *  lists the question; without this the panel would flash back for a poll. A
 *  re-prompt (the same game started again) has a new `raised_at` and shows. */
const answered = new Map<string, string>();

/** Adopt what the status says. Both windows come through here: the HUD with
 *  every snapshot, the main window on `world-claim-wanted`. */
export function adoptPrompts(list: WorldPrompt[]): void {
  for (const [game, raised] of answered) {
    if (!list.some((p) => p.game_slug === game && p.raised_at === raised)) {
      answered.delete(game);
    }
  }
  prompts.set(list.filter((p) => answered.get(p.game_slug) !== p.raised_at));
}

function dropPrompt(gameSlug: string): void {
  prompts.update((list) => {
    const gone = list.find((p) => p.game_slug === gameSlug);
    if (!gone) return list;
    answered.set(gameSlug, gone.raised_at);
    return list.filter((p) => p !== gone);
  });
}

/** Host or view one world of a prompt. The prompt leaves the store at once;
 *  the engine's status confirms it on the next read. */
export async function answerPrompt(prompt: WorldPrompt, saveId: string, role: WorldRole): Promise<void> {
  await api.claimWorld(saveId, role);
  dropPrompt(prompt.game_slug);
}

/** "Not playing" for the whole prompt: every world of it is dismissed. */
export async function declinePrompt(prompt: WorldPrompt): Promise<void> {
  await Promise.all(prompt.worlds.map((w) => api.dismissWorld(w.save_id)));
  dropPrompt(prompt.game_slug);
}

async function mainWindowFocused(): Promise<boolean> {
  try {
    return await getCurrentWindow().isFocused();
  } catch {
    return false;
  }
}

/** The engine asked which world (main window only; the HUD polls). The
 *  question is shown where the user is: in this window when it is the one in
 *  front, otherwise in the HUD over the game, which the Rust side raises.
 *  Never both. A prompt replayed from the journal raises nothing: the status
 *  says whether it is still open, and the HUD's poll draws that. */
async function onClaimWanted(ev: AgentEvent): Promise<void> {
  if (ev.type !== "world_claim_wanted" || isReplaying()) return;
  // The status carries the clock the event does not; the relay re-reads it
  // before emitting this event, so the snapshot is current. The status is
  // the only source: a game it does not list (already answered, or its world
  // held here) is not asked about.
  let list: WorldPrompt[];
  try {
    list = (await api.agentSnapshot()).prompts;
  } catch (e) {
    console.debug("claim prompt: status read failed, nothing raised:", e);
    return;
  }
  if (!list.some((p) => p.game_slug === ev.game_slug)) {
    console.debug(`claim prompt: status has no prompt for ${ev.game_slug}, nothing raised`);
    return;
  }
  answered.delete(ev.game_slug);
  adoptPrompts(list);
  if (await mainWindowFocused()) {
    claimModalOpen.set(true);
  } else {
    claimModalOpen.set(false);
    await api.overlayShow().catch((e) => console.warn("couldn't raise the HUD for the claim prompt:", e));
  }
}

// ── The bell ──────────────────────────────────────────────────────────────

type InterpolationValue = string | number | boolean | Date | null | undefined;
function tr(key: string, values?: Record<string, InterpolationValue>): string {
  return get(_)(key, values ? { values } : undefined);
}

/** The notification id for a hold on a save, so the side copy's folder can
 *  be attached to it when it lands. */
function hostedElsewhereId(saveId: string): string {
  return `world-hosted-elsewhere-${saveId}`;
}

/** The bell items and toasts for the world events that need the user. Live
 *  events only: the relay does not fan the backlog out to these topics, and
 *  a hold from last night is not news. */
async function noticeWorldEvent(ev: AgentEvent): Promise<void> {
  switch (ev.type) {
    case "world_hosted_elsewhere": {
      const world = prettifySlug(ev.game_slug);
      // A later hold on the same save refreshes the row (holder, text); its
      // side-copy button stays.
      const id = hostedElsewhereId(ev.save_id);
      const title = tr("claim.notice_hosted_elsewhere_title", { holder: ev.holder, world });
      const body = tr("claim.notice_hosted_elsewhere_body");
      pushNotification({ id, title, body, priority: "normal" });
      updateNotification(id, { title, body, at: Date.now() });
      if (await mainWindowFocused()) {
        toastInfo(tr("claim.toast_hosted_elsewhere", { holder: ev.holder, world }));
      }
      break;
    }
    case "world_lease_lost": {
      const world = prettifySlug(ev.game_slug);
      // One row per world, refreshed rather than stacked: the bell persists
      // across launches and a high row never expires on its own.
      const id = `world-lease-lost-${ev.save_id}`;
      const title = tr("claim.notice_lease_lost_title", { world });
      const body = tr("claim.notice_lease_lost_body");
      pushNotification({ id, title, body, priority: "high" });
      updateNotification(id, { title, body, at: Date.now() });
      if (await mainWindowFocused()) {
        toastInfo(tr("claim.toast_lease_lost", { world }));
      }
      break;
    }
    case "view_session_writing": {
      const world = prettifySlug(ev.game_slug);
      pushNotification({
        id: `view-session-writing-${ev.save_id}`,
        title: tr("claim.notice_view_writing_title", { world }),
        body: tr("claim.notice_view_writing_body"),
        priority: "high",
        actions: [
          {
            url: `hoard-op:claim-host:${ev.save_id}`,
            label: tr("claim.notice_host_it"),
            op: { kind: "claim_world", save_id: ev.save_id, role: "host" },
          },
        ],
      });
      break;
    }
    default:
      break;
  }
}

/** The side copy of a session under somebody else's lease landed: the hold's
 *  bell item gains the button that opens it. Called from the
 *  `save-conflicts-backed-up` listener (`automatic.ts`), which is where the
 *  folder arrives. Nothing happens for a save with no such item. */
export function noteSideCopy(saveId: string, dir: string): void {
  updateNotification(hostedElsewhereId(saveId), {
    actions: [
      {
        url: `hoard-op:open-folder:${saveId}`,
        label: tr("claim.notice_open_side_copy"),
        op: { kind: "open_folder", path: dir },
      },
    ],
  });
}

let unlisteners: UnlistenFn[] = [];

/** Subscribe to the world events. Called from `subscribeAgent()` before the
 *  relay is attached, so the backlog lands on a listening store. Main window
 *  only: the HUD reads the snapshot instead. */
export async function subscribeWorldEvents(): Promise<void> {
  await unsubscribeWorldEvents();
  const topics = [
    "agent://world-claimed",
    "agent://world-released",
    "agent://world-hosted-elsewhere",
    "agent://world-lease-lost",
    "agent://view-session-writing",
    "agent://game-stopped",
  ];
  try {
    unlisteners = await Promise.all([
      ...topics.map((t) =>
        listen<AgentEvent>(t, (event) => {
          applyWorldEvent(event.payload);
          void noticeWorldEvent(event.payload);
        }),
      ),
      listen<AgentEvent>("agent://world-claim-wanted", (event) => {
        void onClaimWanted(event.payload);
      }),
    ]);
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
