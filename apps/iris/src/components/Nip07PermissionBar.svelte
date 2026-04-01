<script lang="ts">
  import type { Nip07PermissionPrompt } from '../lib/tauri';

  type PermissionDecision = 'deny' | 'allowSession' | 'allowAlways' | 'blockSite';

  interface Props {
    prompt: Nip07PermissionPrompt;
    busy?: boolean;
    error?: string;
    compact?: boolean;
    permissionMethodLabel: (method: string) => string;
    permissionOriginLabel: (origin: string) => string;
    respond: (decision: PermissionDecision) => void;
  }

  let {
    prompt,
    busy = false,
    error = '',
    compact = false,
    permissionMethodLabel,
    permissionOriginLabel,
    respond,
  }: Props = $props();
</script>

<div
  data-testid="nip07-permission-prompt"
  data-tauri-drag-region="false"
  class={`rounded-2xl border border-surface-3 bg-surface-0/95 shadow-lg backdrop-blur-sm ${compact ? 'mt-3 px-3 py-3' : 'mt-3 px-4 py-3'}`}
>
  <div class={`flex ${compact ? 'flex-col gap-3' : 'items-start gap-4'}`}>
    <div class="min-w-0 flex-1">
      <div class="text-[11px] font-medium uppercase tracking-wide text-text-3">NIP-07 Permission</div>
      <h2 class={`mt-1 font-semibold text-text-1 ${compact ? 'text-sm leading-5' : 'text-sm'}`}>
        Allow this site to {permissionMethodLabel(prompt.method)}?
      </h2>
      <p class="mt-1 text-xs leading-relaxed text-text-2">
        Iris is asking on behalf of <span class="font-medium text-text-1">{permissionOriginLabel(prompt.origin)}</span>.
      </p>
      <p class="mt-2 break-all rounded-xl bg-surface-1 px-2.5 py-2 text-[11px] text-text-3">
        {prompt.origin}
      </p>
      {#if error}
        <div class="mt-2 rounded-xl bg-danger/10 px-3 py-2 text-xs text-danger">
          {error}
        </div>
      {/if}
    </div>

    <div class={`grid gap-2 ${compact ? 'grid-cols-2' : 'w-56 shrink-0'}`}>
      <button
        data-testid="nip07-permission-allow-session"
        class={`btn bg-accent text-white hover:opacity-90 disabled:opacity-50 ${compact ? 'col-span-2' : 'w-full'}`}
        onclick={() => respond('allowSession')}
        disabled={busy}
      >
        Allow This Session
      </button>
      <button
        data-testid="nip07-permission-allow-always"
        class={`btn bg-surface-1 text-text-1 hover:bg-surface-2 disabled:opacity-50 ${compact ? 'col-span-2' : 'w-full'}`}
        onclick={() => respond('allowAlways')}
        disabled={busy}
      >
        Always Allow
      </button>
      <button
        data-testid="nip07-permission-deny"
        class={`btn bg-surface-1 text-text-1 hover:bg-surface-2 disabled:opacity-50 ${compact ? 'w-full' : 'w-full'}`}
        onclick={() => respond('deny')}
        disabled={busy}
      >
        Deny
      </button>
      <button
        data-testid="nip07-permission-block-site"
        class={`btn bg-danger/10 text-danger hover:bg-danger/15 disabled:opacity-50 ${compact ? 'w-full' : 'w-full'}`}
        onclick={() => respond('blockSite')}
        disabled={busy}
      >
        Block Site
      </button>
    </div>
  </div>
</div>
