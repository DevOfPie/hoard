<script lang="ts">
  /**
   * Share one save into a group. For a game that shares by world (the
   * service says which, through `listWorlds`) the user picks the world and
   * sees the files that travel with it; any other game shares whole and the
   * world section is simply absent. The rules live in the service: this
   * dialog lists what it answered and sends the verb.
   */
  import { _ } from "svelte-i18n";

  import Button from "./Button.svelte";
  import Modal from "./Modal.svelte";
  import * as api from "../api";
  import type { TrackedSave, WorldFiles } from "../api";
  import { groups, refreshGroups } from "../stores/groups";
  import { showError } from "../stores/error_dialog";
  import { prettifySlug } from "../utils/format";

  let {
    save,
    onClose,
    onShared,
  }: {
    /** The save to share, or `null` while closed. */
    save: TrackedSave | null;
    onClose: () => void;
    /** After the service answered: the caller re-lists its rows. */
    onShared: (save: TrackedSave) => void;
  } = $props();

  let groupId = $state("");
  let worlds = $state<WorldFiles[]>([]);
  let world = $state("");
  let loadingWorlds = $state(false);
  let sharing = $state(false);

  const byWorld = $derived(worlds.length > 0);
  const picked = $derived(worlds.find((w) => w.name === world) ?? null);
  const canShare = $derived(!!groupId && (!byWorld || !!picked) && !sharing);

  const open = $derived(save !== null);
  const saveId = $derived(save?.save_id ?? null);

  // The groups list may have changed since the last opening: read it once
  // per opening. Keyed on `open` alone, never on `$groups`, which every
  // `refreshGroups` sets anew.
  $effect(() => {
    if (open) void refreshGroups().catch(() => {});
  });

  // The worlds belong to this save's folder: each save starts clean.
  $effect(() => {
    const id = saveId;
    if (!open || !id) return;
    world = "";
    worlds = [];
    loadingWorlds = true;
    api
      .listWorlds(id)
      .then((list) => {
        worlds = list;
        if (list.length === 1) world = list[0].name;
      })
      .catch((e) => showError(e))
      .finally(() => (loadingWorlds = false));
  });

  $effect(() => {
    if (!groupId && $groups.length > 0) groupId = $groups[0].id;
  });

  async function confirm() {
    if (!save || !canShare) return;
    const target = save;
    sharing = true;
    try {
      await api.shareSave(target.save_id, groupId, byWorld ? world : null);
      onShared(target);
      onClose();
    } catch (e) {
      showError(e);
    } finally {
      sharing = false;
    }
  }
</script>

<Modal
  {open}
  title={$_("share.title")}
  description={save ? `${prettifySlug(save.game_slug)} · ${save.local_path}` : ""}
  dismissible={!sharing}
  scrollBody
  onClose={() => {
    if (!sharing) onClose();
  }}
>
  {#if save}
    <div class="flex flex-col gap-4">
      <label class="flex flex-col gap-1.5">
        <span class="text-sm font-medium text-zinc-200">{$_("share.group")}</span>
        {#if $groups.length === 0}
          <p class="text-sm text-zinc-500">{$_("share.no_groups")}</p>
        {:else}
          <select
            bind:value={groupId}
            disabled={sharing}
            class="w-full rounded-md border border-white/[0.08] bg-black/30 px-3 py-2 text-sm text-zinc-100 outline-none focus:border-emerald-500/60"
          >
            {#each $groups as g (g.id)}
              <option value={g.id}>{g.name}</option>
            {/each}
          </select>
        {/if}
      </label>

      {#if loadingWorlds}
        <p class="text-xs text-zinc-500">{$_("common.loading")}</p>
      {:else if byWorld}
        <fieldset class="flex flex-col gap-1.5">
          <legend class="mb-1.5 text-xs uppercase tracking-wide text-zinc-500">
            {$_("share.world")}
          </legend>
          {#each worlds as w (w.name)}
            <label
              class="flex cursor-pointer items-center gap-2 rounded-md border px-3 py-2 text-sm transition-colors {world === w.name
                ? 'border-emerald-500/50 bg-emerald-500/[0.06] text-zinc-50'
                : 'border-white/[0.08] text-zinc-300 hover:bg-white/[0.04]'}"
            >
              <input
                type="radio"
                name="share-world"
                value={w.name}
                bind:group={world}
                disabled={sharing}
                class="accent-emerald-500"
              />
              <span class="truncate">{w.name}</span>
            </label>
          {/each}
        </fieldset>

        {#if picked}
          <div class="rounded-lg border border-white/[0.08] bg-black/20 px-3 py-2">
            <p class="mb-1 text-xs uppercase tracking-wide text-zinc-500">
              {$_("share.what_travels")}
            </p>
            <ul class="space-y-0.5 font-mono text-[11px] text-zinc-300">
              {#each picked.include as pattern (pattern)}
                <li class="truncate" title={pattern}>{pattern}</li>
              {/each}
            </ul>
          </div>
        {/if}
      {:else}
        <p class="text-xs text-zinc-500">{$_("share.whole_folder")}</p>
      {/if}

      <p class="text-xs text-zinc-400">{$_("share.characters_note")}</p>
    </div>
  {/if}
  {#snippet footer()}
    <Button variant="ghost" onclick={onClose} disabled={sharing}>
      {$_("common.cancel")}
    </Button>
    <Button onclick={confirm} disabled={!canShare} loading={sharing}>
      {#if byWorld && picked}
        {$_("share.confirm_world", { values: { world: picked.name } })}
      {:else}
        {$_("share.confirm")}
      {/if}
    </Button>
  {/snippet}
</Modal>
