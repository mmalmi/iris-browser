<script lang="ts">
  import type { MeshBandwidthHistoryPoint } from '../lib/mesh';

  interface Props {
    history: MeshBandwidthHistoryPoint[];
  }

  let { history }: Props = $props();

  const width = 320;
  const height = 72;
  const paddingX = 6;
  const paddingY = 6;

  function buildPath(values: number[], maxValue: number): string {
    if (values.length === 0) return '';

    return values
      .map((value, index) => {
        const x = paddingX + ((width - paddingX * 2) * index) / Math.max(values.length - 1, 1);
        const y = height - paddingY - ((height - paddingY * 2) * value) / Math.max(maxValue, 1);
        return `${index === 0 ? 'M' : 'L'} ${x.toFixed(2)} ${y.toFixed(2)}`;
      })
      .join(' ');
  }

  const recentHistory = $derived(history.slice(-30));
  const uploadSeries = $derived(recentHistory.map((point) => point.uploadBps));
  const downloadSeries = $derived(recentHistory.map((point) => point.downloadBps));
  const maxSeriesValue = $derived(Math.max(1, ...uploadSeries, ...downloadSeries));
  const uploadPath = $derived(buildPath(uploadSeries, maxSeriesValue));
  const downloadPath = $derived(buildPath(downloadSeries, maxSeriesValue));
  const spanSeconds = $derived(
    recentHistory.length > 1
      ? Math.max(
          1,
          Math.round(
            (recentHistory[recentHistory.length - 1]!.timestamp - recentHistory[0]!.timestamp) / 1000,
          ),
        )
      : 0,
  );
</script>

<div class="rounded border border-surface-3 bg-surface-1/70 px-2 py-2">
  <div class="mb-1 flex items-center justify-between text-[11px] uppercase tracking-wide text-text-3">
    <span>Recent Throughput</span>
    <span>{spanSeconds > 0 ? `${spanSeconds}s` : 'live'}</span>
  </div>
  {#if recentHistory.length === 0}
    <div class="flex h-[72px] items-center justify-center text-xs text-text-3">
      No traffic yet
    </div>
  {:else}
    <svg viewBox={`0 0 ${width} ${height}`} class="h-[72px] w-full overflow-visible">
      <path d={downloadPath} fill="none" stroke="#58a6ff" stroke-width="2" stroke-linecap="round" />
      <path d={uploadPath} fill="none" stroke="#3fb950" stroke-width="2" stroke-linecap="round" />
    </svg>
    <div class="mt-1 flex items-center gap-3 text-[11px] text-text-3">
      <span class="inline-flex items-center gap-1">
        <span class="h-2 w-2 rounded-full bg-success"></span>
        upload
      </span>
      <span class="inline-flex items-center gap-1">
        <span class="h-2 w-2 rounded-full bg-accent"></span>
        download
      </span>
    </div>
  {/if}
</div>
