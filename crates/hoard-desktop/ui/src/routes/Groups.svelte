<script lang="ts">
  /**
   * Groups: the people this account shares worlds with.
   *
   * One card per group with its members and the worlds shared into it; a
   * card at the bottom to redeem an invite. Every action is one command to
   * the service, which owns the rules; the page lists what it answered.
   */
  import { onMount } from "svelte";
  import { Crown, Link2, LogOut, Plus, Trash2, User, UserMinus, Users } from "@lucide/svelte";
  import { _ } from "svelte-i18n";

  import Button from "../lib/components/Button.svelte";
  import Card from "../lib/components/Card.svelte";
  import Input from "../lib/components/Input.svelte";
  import Modal from "../lib/components/Modal.svelte";
  import InviteTokenModal from "../lib/components/InviteTokenModal.svelte";
  import * as api from "../lib/api";
  import type { Group, GroupMember, InviteOut, TrackedSave } from "../lib/api";
  import { groups, leases, myUserId, refreshGroups, refreshLease } from "../lib/stores/groups";
  import { showError } from "../lib/stores/error_dialog";
  import { toastSuccess } from "../lib/stores/toasts";
  import { formatBytes, prettifySlug } from "../lib/utils/format";

  let loading = $state(true);
  let tracked = $state<TrackedSave[]>([]);

  // New group.
  let creating = $state(false);
  let createDraft = $state("");
  let createBusy = $state(false);

  // Join.
  let joinDraft = $state("");
  let joining = $state(false);

  // Invite: the token shown once.
  let invite = $state<InviteOut | null>(null);
  let inviteGroup = $state<Group | null>(null);
  let inviting = $state<string | null>(null);

  // Confirms, one target each, in the Library's local-state pattern.
  let deleteTarget = $state<Group | null>(null);
  let leaveTarget = $state<Group | null>(null);
  let removeTarget = $state<{ group: Group; member: GroupMember } | null>(null);
  let confirming = $state(false);

  const SEVEN_DAYS = 7 * 24 * 3600;

  onMount(async () => {
    try {
      const [, saves] = await Promise.all([refreshGroups(), api.listTrackedSaves()]);
      tracked = saves;
    } catch (e) {
      showError(e);
    } finally {
      loading = false;
    }
    for (const s of tracked) {
      if (s.shared) void refreshLease(s.save_id).catch(() => {});
    }
  });

  async function reload() {
    try {
      const [, saves] = await Promise.all([refreshGroups(), api.listTrackedSaves()]);
      tracked = saves;
    } catch (e) {
      showError(e);
    }
  }

  function isOwner(g: Group): boolean {
    return $myUserId !== null && g.owner_user_id === $myUserId;
  }

  function ownerName(g: Group): string {
    return g.members.find((m) => m.user_id === g.owner_user_id)?.username ?? "";
  }

  /** The worlds shared into a group that this account can see. */
  function worldsOf(g: Group): TrackedSave[] {
    return tracked.filter((s) => s.shared?.group_id === g.id);
  }

  function bytesOf(g: Group): number {
    return worldsOf(g).reduce((n, s) => n + (s.total_size_bytes ?? 0), 0);
  }

  function joinedOn(iso: string): string {
    return new Date(iso).toLocaleDateString(undefined, {
      year: "numeric",
      month: "short",
      day: "numeric",
    });
  }

  function leaseLabel(s: TrackedSave): { text: string; chip: string } {
    const l = $leases[s.save_id];
    switch (l?.state) {
      case "mine":
        return {
          text: $_("lease.hosting_here"),
          chip: "bg-emerald-500/10 text-emerald-400 ring-emerald-500/30",
        };
      case "other":
        return {
          text: $_("lease.hosted_by", { values: { name: l.holder ?? "" } }),
          chip: "bg-amber-500/10 text-amber-400 ring-amber-500/30",
        };
      case "free":
        return {
          text: $_("lease.free"),
          chip: "bg-white/[0.05] text-zinc-400 ring-white/[0.08]",
        };
      default:
        return {
          text: $_("lease.unknown"),
          chip: "bg-white/[0.05] text-zinc-500 ring-white/[0.08]",
        };
    }
  }

  async function confirmCreate() {
    const name = createDraft.trim();
    if (!name || createBusy) return;
    createBusy = true;
    try {
      const g = await api.createGroup(name);
      groups.update((list) => [...list, g]);
      toastSuccess($_("groups.created_toast", { values: { name: g.name } }));
      creating = false;
      createDraft = "";
    } catch (e) {
      showError(e);
    } finally {
      createBusy = false;
    }
  }

  async function confirmJoin() {
    const token = joinDraft.trim();
    if (!token || joining) return;
    joining = true;
    try {
      const g = await api.joinGroup(token);
      toastSuccess($_("groups.joined_toast", { values: { name: g.name } }));
      joinDraft = "";
      await reload();
    } catch (e) {
      showError(e);
    } finally {
      joining = false;
    }
  }

  async function mintInvite(g: Group) {
    if (inviting) return;
    inviting = g.id;
    try {
      invite = await api.inviteToGroup(g.id, SEVEN_DAYS);
      inviteGroup = g;
    } catch (e) {
      showError(e);
    } finally {
      inviting = null;
    }
  }

  async function confirmDelete() {
    if (!deleteTarget || confirming) return;
    const g = deleteTarget;
    confirming = true;
    try {
      await api.deleteGroup(g.id);
      toastSuccess($_("groups.deleted_toast", { values: { name: g.name } }));
      deleteTarget = null;
      await reload();
    } catch (e) {
      showError(e);
    } finally {
      confirming = false;
    }
  }

  async function confirmLeave() {
    if (!leaveTarget || confirming) return;
    const g = leaveTarget;
    confirming = true;
    try {
      await api.leaveGroup(g.id);
      toastSuccess($_("groups.left_toast", { values: { name: g.name } }));
      leaveTarget = null;
      await reload();
    } catch (e) {
      showError(e);
    } finally {
      confirming = false;
    }
  }

  async function confirmRemove() {
    if (!removeTarget || confirming) return;
    const { group, member } = removeTarget;
    confirming = true;
    try {
      await api.removeMember(group.id, member.user_id);
      toastSuccess(
        $_("groups.removed_toast", { values: { name: member.username, group: group.name } }),
      );
      removeTarget = null;
      await reload();
    } catch (e) {
      showError(e);
    } finally {
      confirming = false;
    }
  }
</script>

<div class="mx-auto max-w-4xl px-6 py-8">
  <div class="mb-6 flex flex-wrap items-start justify-between gap-4">
    <div>
      <h1 class="font-display text-[28px] leading-tight font-semibold tracking-[-0.02em] text-zinc-50">
        {$_("groups.title")}
      </h1>
      <p class="mt-2 text-sm text-zinc-400">{$_("groups.subtitle")}</p>
    </div>
    <Button onclick={() => (creating = true)}>
      <Plus size={15} data-anim="pop" />
      {$_("groups.new_group")}
    </Button>
  </div>

  {#if loading}
    <p class="text-sm text-zinc-500">{$_("common.loading")}</p>
  {:else if $groups.length === 0}
    <Card class="mb-4">
      <div class="flex flex-col items-center py-6 text-center">
        <span
          class="mb-3 flex h-12 w-12 items-center justify-center rounded-full bg-emerald-500/10 text-emerald-300 ring-1 ring-emerald-500/30"
        >
          <Users size={22} />
        </span>
        <p class="text-sm font-medium text-zinc-100">{$_("groups.empty_title")}</p>
        <p class="mt-1 max-w-md text-sm text-zinc-400">{$_("groups.empty_body")}</p>
      </div>
    </Card>
  {:else}
    {#each $groups as g (g.id)}
      {@const owner = isOwner(g)}
      {@const worlds = worldsOf(g)}
      <Card class="mb-4">
        <div class="flex flex-wrap items-start justify-between gap-3">
          <div class="min-w-0">
            <p class="flex items-center gap-2 truncate text-lg font-medium text-zinc-50">
              <Users size={18} class="shrink-0 text-emerald-400" />
              {g.name}
            </p>
            <p class="mt-1 text-xs text-zinc-500">
              {owner
                ? $_("groups.summary_owned", {
                    values: { members: g.members.length, worlds: worlds.length, bytes: formatBytes(bytesOf(g)) },
                  })
                : $_("groups.summary_member", {
                    values: {
                      owner: ownerName(g),
                      members: g.members.length,
                      worlds: worlds.length,
                      bytes: formatBytes(bytesOf(g)),
                    },
                  })}
            </p>
          </div>
          <div class="flex shrink-0 items-center gap-2">
            {#if owner}
              <Button
                variant="secondary"
                onclick={() => mintInvite(g)}
                loading={inviting === g.id}
              >
                <Link2 size={14} />
                {$_("groups.invite_link")}
              </Button>
              <Button variant="ghost" onclick={() => (deleteTarget = g)}>
                <Trash2 size={14} class="text-red-400" />
                {$_("groups.delete_group")}
              </Button>
            {:else}
              <Button variant="ghost" onclick={() => (leaveTarget = g)}>
                <LogOut size={14} />
                {$_("groups.leave")}
              </Button>
            {/if}
          </div>
        </div>

        <div class="mt-4">
          <p class="mb-2 text-xs uppercase tracking-wide text-zinc-500">
            {$_("groups.members")}
          </p>
          <div class="flex flex-col gap-1.5">
            {#each g.members as m (m.user_id)}
              <div
                class="flex items-center gap-3 rounded-lg border border-white/[0.08] px-3 py-2 text-sm"
              >
                <User size={14} class="shrink-0 text-zinc-500" />
                <span class="min-w-0 flex-1 truncate text-zinc-100">
                  {m.username}
                  {#if m.user_id === $myUserId}
                    <span class="text-zinc-500">· {$_("groups.you")}</span>
                  {/if}
                </span>
                {#if m.user_id === g.owner_user_id}
                  <span
                    class="inline-flex items-center gap-1 rounded-full bg-amber-500/10 px-2 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-amber-300 ring-1 ring-inset ring-amber-500/30"
                  >
                    <Crown size={10} />
                    {$_("groups.owner_pill")}
                  </span>
                {/if}
                <span class="shrink-0 text-xs text-zinc-500">
                  {$_("groups.joined", { values: { date: joinedOn(m.joined_at) } })}
                </span>
                {#if owner && m.user_id !== g.owner_user_id}
                  <button
                    type="button"
                    onclick={() => (removeTarget = { group: g, member: m })}
                    aria-label={$_("groups.remove")}
                    title={$_("groups.remove")}
                    class="shrink-0 rounded p-1 text-zinc-500 transition-colors hover:bg-zinc-700/40 hover:text-red-400"
                  >
                    <UserMinus size={13} />
                  </button>
                {/if}
              </div>
            {/each}
          </div>
        </div>

        <div class="mt-4">
          <p class="mb-2 text-xs uppercase tracking-wide text-zinc-500">
            {$_("groups.shared_worlds")}
          </p>
          {#if worlds.length === 0}
            <p class="text-xs text-zinc-500">{$_("groups.no_worlds")}</p>
          {:else}
            <div class="flex flex-col gap-1.5">
              {#each worlds as s (s.save_id)}
                {@const lease = leaseLabel(s)}
                <div
                  class="flex items-center gap-3 rounded-lg border border-white/[0.08] px-3 py-2 text-sm"
                >
                  <span class="min-w-0 flex-1 truncate">
                    <span class="text-zinc-100">{s.label}</span>
                    <span class="text-zinc-500"> · {prettifySlug(s.game_slug)}</span>
                  </span>
                  <span
                    class="inline-flex shrink-0 items-center rounded-full px-2 py-0.5 text-[10px] font-medium ring-1 ring-inset {lease.chip}"
                  >
                    {lease.text}
                  </span>
                  <span class="shrink-0 text-xs text-zinc-500">
                    {s.shared?.owner_user_id === $myUserId
                      ? $_("groups.world_yours")
                      : $_("groups.world_owned_by", { values: { name: s.shared?.owner_username ?? "" } })}
                  </span>
                </div>
              {/each}
            </div>
          {/if}
        </div>
      </Card>
    {/each}
  {/if}

  <Card>
    <p class="text-sm font-medium text-zinc-100">{$_("groups.join_title")}</p>
    <p class="mt-1 text-xs text-zinc-500">{$_("groups.join_hint")}</p>
    <div class="mt-3 flex flex-wrap items-end gap-2">
      <Input
        class="min-w-0 flex-1"
        bind:value={joinDraft}
        placeholder={$_("groups.join_placeholder")}
        disabled={joining}
        onkeydown={(e) => {
          if (e.key === "Enter") void confirmJoin();
        }}
      />
      <Button onclick={confirmJoin} loading={joining} disabled={!joinDraft.trim()}>
        {$_("groups.join")}
      </Button>
    </div>
  </Card>
</div>

<Modal
  open={creating}
  title={$_("groups.new_group")}
  dismissible={!createBusy}
  onClose={() => {
    if (!createBusy) creating = false;
  }}
>
  <Input
    bind:value={createDraft}
    label={$_("groups.name_label")}
    placeholder={$_("groups.name_placeholder")}
    disabled={createBusy}
    onkeydown={(e) => {
      if (e.key === "Enter") void confirmCreate();
    }}
  />
  {#snippet footer()}
    <Button variant="ghost" onclick={() => (creating = false)} disabled={createBusy}>
      {$_("common.cancel")}
    </Button>
    <Button onclick={confirmCreate} loading={createBusy} disabled={!createDraft.trim()}>
      {$_("groups.create")}
    </Button>
  {/snippet}
</Modal>

<InviteTokenModal
  {invite}
  groupName={inviteGroup?.name ?? ""}
  onClose={() => {
    invite = null;
    inviteGroup = null;
  }}
/>

<Modal
  open={deleteTarget !== null}
  title={$_("groups.delete_title")}
  dismissible={!confirming}
  onClose={() => {
    if (!confirming) deleteTarget = null;
  }}
>
  <p class="text-sm text-zinc-300">
    {$_("groups.delete_body", { values: { name: deleteTarget?.name ?? "" } })}
  </p>
  {#snippet footer()}
    <Button variant="ghost" onclick={() => (deleteTarget = null)} disabled={confirming}>
      {$_("common.cancel")}
    </Button>
    <Button variant="danger" onclick={confirmDelete} loading={confirming}>
      {$_("groups.delete_group")}
    </Button>
  {/snippet}
</Modal>

<Modal
  open={leaveTarget !== null}
  title={$_("groups.leave_title")}
  dismissible={!confirming}
  onClose={() => {
    if (!confirming) leaveTarget = null;
  }}
>
  <p class="text-sm text-zinc-300">
    {$_("groups.leave_body", { values: { name: leaveTarget?.name ?? "" } })}
  </p>
  {#snippet footer()}
    <Button variant="ghost" onclick={() => (leaveTarget = null)} disabled={confirming}>
      {$_("common.cancel")}
    </Button>
    <Button variant="danger" onclick={confirmLeave} loading={confirming}>
      {$_("groups.leave")}
    </Button>
  {/snippet}
</Modal>

<Modal
  open={removeTarget !== null}
  title={$_("groups.remove_title")}
  dismissible={!confirming}
  onClose={() => {
    if (!confirming) removeTarget = null;
  }}
>
  <p class="text-sm text-zinc-300">
    {$_("groups.remove_body", {
      values: { name: removeTarget?.member.username ?? "", group: removeTarget?.group.name ?? "" },
    })}
  </p>
  {#snippet footer()}
    <Button variant="ghost" onclick={() => (removeTarget = null)} disabled={confirming}>
      {$_("common.cancel")}
    </Button>
    <Button variant="danger" onclick={confirmRemove} loading={confirming}>
      {$_("groups.remove")}
    </Button>
  {/snippet}
</Modal>
