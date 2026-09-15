<script lang="ts">
  /**
   * The rows of one claim prompt: a box per shared world of the game with
   * its group, its lease and the two roles, and "Not playing" for the lot.
   * Shared by the HUD's panel (`overlay/ClaimPanel.svelte`) and the main
   * window's dialog (`ClaimWorldModal.svelte`). It renders what the engine
   * offered and sends a verb; whether hosting is possible is the lease the
   * engine reported, nothing computed here.
   */
  import { _ } from "svelte-i18n";
  import { Eye, Radio } from "@lucide/svelte";

  import Button from "./Button.svelte";
  import type { WorldPrompt, WorldRole } from "../api";
  import { answerPrompt, declinePrompt } from "../stores/groups";
  import { showError } from "../stores/error_dialog";

  let { prompt }: { prompt: WorldPrompt } = $props();

  /** The save whose verb is out, so a double click does not send two. */
  let busy = $state<string | null>(null);

  async function answer(saveId: string, role: WorldRole) {
    if (busy) return;
    busy = saveId;
    try {
      await answerPrompt(prompt, saveId, role);
    } catch (e) {
      showError(e);
    } finally {
      busy = null;
    }
  }

  async function decline() {
    if (busy) return;
    busy = "*";
    try {
      await declinePrompt(prompt);
    } catch (e) {
      showError(e);
    } finally {
      busy = null;
    }
  }

  function leasePill(w: WorldPrompt["worlds"][number]): { text: string; chip: string } {
    switch (w.lease) {
      case "mine":
        return {
          text: $_("lease.pill_mine"),
          chip: "bg-emerald-500/10 text-emerald-300 ring-emerald-500/30",
        };
      case "other":
        return {
          text: $_("lease.hosted_by", { values: { name: w.holder ?? "" } }),
          chip: "bg-amber-500/10 text-amber-300 ring-amber-500/30",
        };
      case "free":
        return {
          text: $_("lease.free"),
          chip: "bg-white/[0.05] text-zinc-300 ring-white/[0.10]",
        };
      default:
        return {
          text: $_("lease.unknown"),
          chip: "bg-white/[0.05] text-zinc-400 ring-white/[0.08]",
        };
    }
  }
</script>

<div class="flex flex-col gap-2">
  {#each prompt.worlds as w (w.save_id)}
    {@const pill = leasePill(w)}
    {@const taken = w.lease === "other"}
    <div
      class="flex flex-wrap items-center gap-3 rounded-lg border border-white/[0.10] bg-white/[0.03] px-3 py-2"
    >
      <span class="min-w-0 flex-1 truncate font-medium text-zinc-100">{w.label}</span>
      {#if w.group_name}
        <span
          class="inline-flex shrink-0 items-center rounded-full bg-white/[0.06] px-2 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-zinc-300 ring-1 ring-inset ring-white/[0.10]"
        >
          {w.group_name}
        </span>
      {/if}
      <span
        class="inline-flex shrink-0 items-center rounded-full px-2 py-0.5 text-[10px] font-medium ring-1 ring-inset {pill.chip}"
      >
        {pill.text}
      </span>
      <div class="flex shrink-0 items-center gap-2">
        <Button
          onclick={() => void answer(w.save_id, "host")}
          disabled={taken || busy !== null}
          loading={busy === w.save_id}
          title={taken ? $_("claim.host_taken", { values: { name: w.holder ?? "" } }) : undefined}
        >
          <Radio size={14} />
          {$_("claim.host")}
        </Button>
        <Button
          variant="secondary"
          onclick={() => void answer(w.save_id, "view")}
          disabled={busy !== null}
        >
          <Eye size={14} />
          {$_("claim.view")}
        </Button>
      </div>
    </div>
  {/each}
  <div class="flex justify-end">
    <Button variant="ghost" onclick={() => void decline()} disabled={busy !== null} loading={busy === "*"}>
      {$_("claim.not_playing")}
    </Button>
  </div>
</div>
