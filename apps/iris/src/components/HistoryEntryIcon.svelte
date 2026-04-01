<script lang="ts">
  import type { HistoryEntry } from '../lib/tauri';

  interface Props {
    entry: HistoryEntry;
  }

  let { entry }: Props = $props();
  let imgError = $state(false);

  let faviconUrl = $derived.by(() => {
    try {
      const url = new URL(entry.path);
      if (url.protocol !== 'http:' && url.protocol !== 'https:') return null;
      return `${url.origin}/favicon.ico`;
    } catch {
      return null;
    }
  });

  let fallbackIconClass = $derived('i-lucide-globe');

  $effect(() => {
    entry.path;
    imgError = false;
  });
</script>

{#if faviconUrl && !imgError}
  <img
    src={faviconUrl}
    alt=""
    class="h-4 w-4 shrink-0 rounded-sm object-cover"
    loading="lazy"
    onerror={() => (imgError = true)}
  />
{:else}
  <span class={`${fallbackIconClass} text-sm text-text-3 shrink-0`}></span>
{/if}
