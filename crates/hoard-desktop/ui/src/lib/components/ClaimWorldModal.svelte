<script lang="ts">
  /**
   * The claim prompt in the main window, for when this window is the one in
   * front (HRD-D-0014): otherwise the same question is raised in the HUD over
   * the game and this stays shut. Mounted once in `App.svelte`; opened by
   * `stores/groups.ts` on `world-claim-wanted`. Closing it answers nothing:
   * the prompt stays pending in the engine and its clock keeps running.
   */
  import { _ } from "svelte-i18n";

  import ClaimWorldChoices from "./ClaimWorldChoices.svelte";
  import Modal from "./Modal.svelte";
  import { claimModalOpen, prompts } from "../stores/groups";
  import { claimCountdown, claimTitle } from "../utils/claimText";

  // The Dashboard's clock: one tick a second while the window is visible.
  let now = $state(Date.now());
  $effect(() => {
    const id = setInterval(() => {
      if (!document.hidden) now = Date.now();
    }, 1000);
    return () => clearInterval(id);
  });

  // One game at a time, oldest question first; the next shows once this one
  // is answered, because the store drops it.
  const prompt = $derived($prompts[0] ?? null);
  const open = $derived($claimModalOpen && prompt !== null);
  const countdown = $derived(prompt ? claimCountdown(prompt, now, $_) : null);
</script>

{#if prompt}
  <Modal
    {open}
    title={claimTitle(prompt, $_)}
    description={countdown ?? undefined}
    onClose={() => claimModalOpen.set(false)}
  >
    <ClaimWorldChoices {prompt} />
  </Modal>
{/if}
