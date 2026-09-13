<script lang="ts">
  /**
   * The invite token, shown once. The server keeps only its hash, so there is
   * no second look: the user copies it here or mints another.
   */
  import { Check, Copy } from "@lucide/svelte";
  import { _ } from "svelte-i18n";

  import Button from "./Button.svelte";
  import Modal from "./Modal.svelte";
  import type { InviteOut } from "../api";
  import { formatDateTime } from "../utils/format";

  let {
    invite,
    groupName,
    onClose,
  }: {
    invite: InviteOut | null;
    groupName: string;
    onClose: () => void;
  } = $props();

  let copied = $state(false);

  async function copy() {
    if (!invite) return;
    try {
      await navigator.clipboard.writeText(invite.token);
      copied = true;
      setTimeout(() => (copied = false), 1500);
    } catch {
      /* the token is selectable below; nothing else to do */
    }
  }
</script>

<Modal
  open={invite !== null}
  title={$_("groups.invite_title", { values: { group: groupName } })}
  {onClose}
>
  {#if invite}
    <p class="mb-3 text-sm text-zinc-300">{$_("groups.invite_shown_once")}</p>
    <div
      class="flex items-center gap-2 rounded-lg border border-white/[0.08] bg-black/30 px-3 py-2"
    >
      <code class="min-w-0 flex-1 select-all break-all font-mono text-xs text-zinc-100">
        {invite.token}
      </code>
      <button
        type="button"
        onclick={copy}
        aria-label={$_("groups.invite_copy")}
        title={$_("groups.invite_copy")}
        class="flex h-7 w-7 shrink-0 items-center justify-center rounded-md text-zinc-400 transition-colors hover:bg-white/[0.06] hover:text-zinc-100"
      >
        {#if copied}
          <Check size={14} class="text-emerald-400" />
        {:else}
          <Copy size={14} />
        {/if}
      </button>
    </div>
    <p class="mt-3 text-xs text-zinc-500">
      {$_("groups.invite_expires", { values: { date: formatDateTime(invite.expires_at) } })}
    </p>
    <p class="mt-1 text-xs text-zinc-500">{$_("groups.invite_where")}</p>
  {/if}
  {#snippet footer()}
    <Button variant="secondary" onclick={onClose}>{$_("common.close")}</Button>
  {/snippet}
</Modal>
