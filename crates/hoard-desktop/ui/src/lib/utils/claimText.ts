/**
 * The claim prompt's two lines, shared by the HUD's panel and the main
 * window's dialog so the two cannot phrase the same question differently.
 * The translator comes in as a parameter for the reason `feedText.ts` gives.
 */
import type { WorldPrompt } from "../api";
import { prettifySlug } from "./format";
import type { Translate } from "./feedText";

/** "<Game> started. Which world?" */
export function claimTitle(prompt: WorldPrompt, $_: Translate): string {
  return $_("claim.title", { values: { game: prettifySlug(prompt.game_slug) } });
}

/** "Hosting <world> in <n> s unless you choose", or `null` when the engine's
 *  clock is not armed (two worlds, or a lease that is not free). `now` is
 *  epoch ms from the caller's tick, so the figure moves. The engine's clock
 *  is the one that fires; this only reads it. */
export function claimCountdown(
  prompt: WorldPrompt,
  now: number,
  $_: Translate,
): string | null {
  if (!prompt.auto_host_at) return null;
  const at = new Date(prompt.auto_host_at).getTime();
  if (!Number.isFinite(at)) return null;
  const seconds = Math.max(0, Math.round((at - now) / 1000));
  // The world the clock would host: the one free world of the game.
  const world =
    prompt.worlds.find((w) => w.lease === "free" || w.lease === "unknown") ??
    prompt.worlds[0];
  return $_("claim.auto_host_in", {
    values: { world: world?.label ?? "", seconds },
  });
}
