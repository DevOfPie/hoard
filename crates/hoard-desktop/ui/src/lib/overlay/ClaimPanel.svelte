<script lang="ts">
  /**
   * The claim prompt inside the HUD (HRD-D-0014): a strip across the top,
   * above the three columns, one block per game still asking. It draws the
   * `prompts` store, which `Overlay.svelte` fills from every snapshot, and
   * hides on its own once a game is answered because the store empties. The
   * HUD itself stays up until Escape: the log is still worth reading.
   */
  import { _ } from "svelte-i18n";

  import ClaimWorldChoices from "../components/ClaimWorldChoices.svelte";
  import { prompts } from "../stores/groups";
  import { claimCountdown, claimTitle } from "../utils/claimText";

  /** Epoch ms from the HUD's tick, so the countdown moves. */
  let { now }: { now: number } = $props();
</script>

{#each $prompts as prompt (prompt.game_slug + prompt.raised_at)}
  {@const countdown = claimCountdown(prompt, now, $_)}
  <section
    class="shrink-0 border-b border-emerald-500/50 bg-emerald-500/[0.06] px-6 py-4"
    aria-live="polite"
  >
    <div class="mb-3 flex flex-wrap items-baseline justify-between gap-x-6 gap-y-1">
      <h2 class="font-display font-semibold tracking-tight" style="font-size: 1.1em;">
        {claimTitle(prompt, $_)}
      </h2>
      {#if countdown}
        <span class="text-zinc-400 tabular-nums" style="font-size: 0.85em;">{countdown}</span>
      {/if}
    </div>
    <ClaimWorldChoices {prompt} />
  </section>
{/each}
