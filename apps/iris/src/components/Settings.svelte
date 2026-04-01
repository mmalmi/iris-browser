<script lang="ts">
  import { onMount } from 'svelte';
  import { minidenticon } from 'minidenticons';
  import BandwidthHistoryChart from './BandwidthHistoryChart.svelte';
  import { animalName } from '../lib/animalName';
  import {
    isAutostartEnabled,
    toggleAutostart,
    getHtreeServerUrl,
    getDaemonNetworkSettings,
    updateDaemonNetworkSettings,
    clearHistory,
    type DaemonBlossomServerSettings,
    type DaemonNetworkSettings,
    type Nip07AccountSummary,
  } from '../lib/tauri';
  import { distributedOwner } from '../lib/apps';
  import {
    advanceMeshBandwidthHistory,
    emptyDaemonMeshStatus,
    formatBandwidth,
    formatBytes,
    parseDaemonMeshStatus,
    type DaemonMeshStatus,
    type MeshBandwidthHistoryPoint,
    type MeshHistoryCursor,
    type MeshPeerInfo,
  } from '../lib/mesh';
  import {
    clearClipboardIfUnchanged,
    sensitiveClipboardClearDelayMs,
    writeClipboardText,
  } from '../lib/sensitiveClipboard';

  interface Props {
    onnavigate: (url: string) => void | Promise<void>;
    selectedTab?: TabId | null;
    onSelectTab?: (tab: TabId | null) => void;
    nip07Accounts?: Nip07AccountSummary[];
    activeNip07AccountPubkey?: string | null;
    exportNip07Secret?: (pubkey: string) => Promise<string>;
  }

  type TabId = 'app' | 'privacy' | 'users' | 'network' | 'about';

  const tabs = [
    {
      id: 'app',
      label: 'App',
      icon: 'i-lucide-settings-2',
      activeRowClass: 'bg-accent/8',
      iconFrameClass: 'bg-accent/12 text-accent ring-1 ring-accent/20',
    },
    {
      id: 'privacy',
      label: 'Privacy',
      icon: 'i-lucide-shield',
      activeRowClass: 'bg-rose-500/8',
      iconFrameClass: 'bg-rose-500/12 text-rose-500 ring-1 ring-rose-500/20',
    },
    {
      id: 'users',
      label: 'Users',
      icon: 'i-lucide-user-round',
      activeRowClass: 'bg-emerald-500/8',
      iconFrameClass: 'bg-emerald-500/12 text-emerald-500 ring-1 ring-emerald-500/20',
    },
    {
      id: 'network',
      label: 'Network',
      icon: 'i-lucide-server',
      activeRowClass: 'bg-sky-500/8',
      iconFrameClass: 'bg-sky-500/12 text-sky-500 ring-1 ring-sky-500/20',
    },
    {
      id: 'about',
      label: 'About',
      icon: 'i-lucide-info',
      activeRowClass: 'bg-amber-500/10',
      iconFrameClass: 'bg-amber-500/12 text-amber-500 ring-1 ring-amber-500/20',
    },
  ] as const satisfies ReadonlyArray<{
    id: TabId;
    label: string;
    icon: string;
    activeRowClass: string;
    iconFrameClass: string;
  }>;

  const DEFAULT_TAB: TabId = 'app';

  const sourceLinks = [
    {
      label: 'Open Iris browser repository',
      description: 'Browse the canonical repository in Iris Git',
      icon: 'i-lucide-git-branch',
      url: `htree://${distributedOwner}/git/#/${distributedOwner}/iris-browser`,
    },
    {
      label: 'Open Iris app source',
      description: 'Jump straight to apps/iris in the standalone repo',
      icon: 'i-lucide-app-window',
      url: `htree://${distributedOwner}/git/#/${distributedOwner}/iris-browser/apps/iris`,
    },
  ] as const;
  const NETWORK_STATUS_POLL_INTERVAL_MS = 2000;
  const CLIPBOARD_CLEAR_DELAY_MS = sensitiveClipboardClearDelayMs();
  const COPY_FEEDBACK_DURATION_MS = 2_500;

  let {
    onnavigate,
    selectedTab = null,
    onSelectTab = undefined,
    nip07Accounts = [],
    activeNip07AccountPubkey = null,
    exportNip07Secret = undefined,
  }: Props = $props();

  let routeTab = $state<TabId | null>(null);
  let autostart = $state(false);
  let daemonUrl = $state('');
  let historyCleared = $state(false);
  let nip07CopyBusyPubkey = $state<string | null>(null);
  let nip07CopySuccessPubkey = $state<string | null>(null);
  let nip07CopyError = $state('');
  let meshStatus = $state<DaemonMeshStatus>(emptyDaemonMeshStatus());
  let meshBandwidthHistory = $state<MeshBandwidthHistoryPoint[]>([]);
  let meshHistoryCursor = $state<MeshHistoryCursor | null>(null);
  let meshUploadBandwidth = $state(0);
  let meshDownloadBandwidth = $state(0);
  let networkStatusLoaded = $state(false);
  let networkStatusError = $state('');
  let daemonNetworkSettings = $state<DaemonNetworkSettings>({
    webrtc: true,
    multicast: false,
    bluetooth: false,
    nostrRelaysEnabled: true,
    blossomEnabled: true,
    maxMulticastPeers: 0,
    maxBluetoothPeers: 0,
    multicastGroup: '239.255.42.98',
    multicastPort: 48555,
    relayUrls: [],
    blossomServers: [],
  });
  let daemonNetworkDraft = $state<DaemonNetworkSettings>({
    webrtc: true,
    multicast: false,
    bluetooth: false,
    nostrRelaysEnabled: true,
    blossomEnabled: true,
    maxMulticastPeers: 0,
    maxBluetoothPeers: 0,
    multicastGroup: '239.255.42.98',
    multicastPort: 48555,
    relayUrls: [],
    blossomServers: [],
  });
  let daemonNetworkLoaded = $state(false);
  let daemonNetworkBusy = $state(false);
  let daemonNetworkError = $state('');
  let daemonNetworkSaved = $state(false);
  let newRelayUrl = $state('');
  let newBlossomUrl = $state('');
  let networkStatusPollInterval: ReturnType<typeof setInterval> | null = null;
  let clipboardClearTimer: ReturnType<typeof setTimeout> | null = null;
  let copyFeedbackTimer: ReturnType<typeof setTimeout> | null = null;

  const buildLabel = (() => {
    const buildTime = import.meta.env.VITE_BUILD_TIME;
    if (!buildTime || buildTime === 'undefined') return 'development';
    try {
      return new Date(buildTime).toLocaleString();
    } catch {
      return buildTime;
    }
  })();

  onMount(() => {
    void (async () => {
      autostart = await isAutostartEnabled();
      await refreshDaemonNetworkSettings();
      try {
        daemonUrl = await getHtreeServerUrl();
        networkStatusError = '';
      } catch {
        daemonUrl = '';
        networkStatusLoaded = true;
        networkStatusError = 'Embedded daemon unavailable';
      }
    })();

    return () => {
      if (clipboardClearTimer) clearTimeout(clipboardClearTimer);
      if (copyFeedbackTimer) clearTimeout(copyFeedbackTimer);
      stopNetworkStatusPolling();
    };
  });

  $effect(() => {
    routeTab = selectedTab;
  });

  let activeTab = $derived(routeTab ?? DEFAULT_TAB);
  let activeTabMeta = $derived(tabs.find((tab) => tab.id === activeTab) ?? tabs[0]);
  let isSettingsRootRoute = $derived(routeTab === null);

  $effect(() => {
    const shouldPollNetworkStatus = activeTab === 'network' && daemonUrl.length > 0;
    stopNetworkStatusPolling();

    if (!shouldPollNetworkStatus) return;

    void refreshNetworkStatus();
    networkStatusPollInterval = setInterval(() => {
      void refreshNetworkStatus();
    }, NETWORK_STATUS_POLL_INTERVAL_MS);

    return () => {
      stopNetworkStatusPolling();
    };
  });

  async function handleAutostartToggle() {
    const newValue = !autostart;
    const ok = await toggleAutostart(newValue);
    if (ok) autostart = newValue;
  }

  async function handleClearHistory() {
    await clearHistory();
    historyCleared = true;
    setTimeout(() => {
      historyCleared = false;
    }, 2000);
  }

  function openSource(url: string) {
    void onnavigate(url);
  }

  function cloneDaemonNetworkSettings(settings: DaemonNetworkSettings): DaemonNetworkSettings {
    return {
      ...settings,
      relayUrls: [...settings.relayUrls],
      blossomServers: settings.blossomServers.map((server) => ({ ...server })),
    };
  }

  function daemonNetworkSettingsEqual(a: DaemonNetworkSettings, b: DaemonNetworkSettings): boolean {
    return JSON.stringify(a) === JSON.stringify(b);
  }

  let hasPendingDaemonNetworkChanges = $derived(
    !daemonNetworkSettingsEqual(daemonNetworkSettings, daemonNetworkDraft),
  );

  let configuredBlossomReadServers = $derived(
    daemonNetworkSettings.blossomServers.filter((server) => server.read).length,
  );
  let activeRelayCount = $derived(
    daemonNetworkSettings.nostrRelaysEnabled ? daemonNetworkSettings.relayUrls.length : 0,
  );
  let activeBlossomReadServerCount = $derived(
    daemonNetworkSettings.blossomEnabled ? configuredBlossomReadServers : 0,
  );
  let connectedMeshPeers = $derived(
    meshStatus.peers.filter((peer) => peer.state === 'connected'),
  );
  let inactiveMeshPeerCount = $derived(meshStatus.peers.length - connectedMeshPeers.length);
  let recentBluetoothEvents = $derived(meshStatus.bluetoothReceivedEvents.slice(0, 6));
  let sortedNip07Accounts = $derived([...nip07Accounts].sort((a, b) => a.addedAt - b.addedAt));

  function selectTab(tab: TabId) {
    routeTab = tab;
    onSelectTab?.(tab);
  }

  function openSettingsIndex() {
    routeTab = null;
    onSelectTab?.(null);
  }

  async function refreshDaemonNetworkSettings() {
    try {
      const settings = await getDaemonNetworkSettings();
      daemonNetworkSettings = cloneDaemonNetworkSettings(settings);
      daemonNetworkDraft = cloneDaemonNetworkSettings(settings);
      daemonNetworkError = '';
    } catch (error) {
      daemonNetworkError = error instanceof Error ? error.message : 'Failed to load daemon network settings';
    } finally {
      daemonNetworkLoaded = true;
    }
  }

  async function refreshNetworkStatus() {
    if (!daemonUrl) {
      networkStatusLoaded = true;
      return;
    }

    try {
      const response = await fetch(`${daemonUrl}/api/status`, {
        cache: 'no-store',
      });
      if (!response.ok) {
        throw new Error(`HTTP ${response.status}`);
      }
      const payload = await response.json();
      const nextStatus = parseDaemonMeshStatus(payload);
      const sample = advanceMeshBandwidthHistory(
        meshHistoryCursor,
        meshBandwidthHistory,
        {
          totalBytesSent: nextStatus.totalBytesSent,
          totalBytesReceived: nextStatus.totalBytesReceived,
        },
        Date.now(),
      );
      meshStatus = nextStatus;
      meshHistoryCursor = sample.nextCursor;
      meshBandwidthHistory = sample.history;
      meshUploadBandwidth = sample.rates.uploadBps;
      meshDownloadBandwidth = sample.rates.downloadBps;
      networkStatusError = '';
    } catch (error) {
      networkStatusError = error instanceof Error ? error.message : 'Failed to load daemon status';
    } finally {
      networkStatusLoaded = true;
    }
  }

  function stopNetworkStatusPolling() {
    if (networkStatusPollInterval) {
      clearInterval(networkStatusPollInterval);
      networkStatusPollInterval = null;
    }
  }

  function stateColor(state: MeshPeerInfo['state']): string {
    return state === 'connected' ? 'bg-success' : 'bg-surface-3';
  }

  function relationshipLabel(pool: MeshPeerInfo['pool']): string | null {
    return pool === 'follows' ? 'contact' : null;
  }

  function transportLabel(transport: string): string {
    switch (transport.toLowerCase()) {
      case 'bluetooth':
        return 'Bluetooth';
      case 'webrtc':
        return 'WebRTC';
      default:
        return transport;
    }
  }

  function signalPathLabel(path: string): string {
    switch (path.toLowerCase()) {
      case 'multicast':
        return 'LAN multicast';
      case 'relay':
        return 'Relay signaling';
      default:
        return path;
    }
  }

  function peerKindLabel(peer: MeshPeerInfo): string {
    if (peer.pool === 'follows') {
      return 'Contact peer';
    }
    if (peer.signalPaths.some((path) => path.toLowerCase() === 'multicast')) {
      return 'LAN peer';
    }
    if (peer.signalPaths.some((path) => path.toLowerCase() === 'relay')) {
      return 'Relay peer';
    }
    return 'Peer';
  }

  function peerLabel(peer: MeshPeerInfo, index: number): string {
    return `${peerKindLabel(peer)} ${index + 1}`;
  }

  function peerIdentitySeed(peer: MeshPeerInfo): string {
    return peer.pubkey || peer.peerId || peer.id;
  }

  function peerIdentityLabel(peer: MeshPeerInfo, index: number): string {
    return peerLabel(peer, index);
  }

  function peerIdentitySubtitle(peer: MeshPeerInfo): string {
    return peerSignalSummary(peer);
  }

  function peerIdenticonUri(peer: MeshPeerInfo): string {
    return `data:image/svg+xml;utf8,${encodeURIComponent(minidenticon(peerIdentitySeed(peer), 48, 48))}`;
  }

  function peerSignalSummary(peer: MeshPeerInfo): string {
    const labels = Array.from(new Set(peer.signalPaths.map((path) => signalPathLabel(path))));
    if (labels.length > 0) return labels.join(' + ');
    return transportLabel(peer.transport);
  }

  async function applyDaemonNetworkSettings(nextSettings: DaemonNetworkSettings) {
    daemonNetworkBusy = true;
    try {
      const applied = await updateDaemonNetworkSettings(nextSettings);
      daemonNetworkSettings = cloneDaemonNetworkSettings(applied);
      daemonNetworkDraft = cloneDaemonNetworkSettings(applied);
      daemonNetworkError = '';
      daemonNetworkSaved = true;
      setTimeout(() => {
        daemonNetworkSaved = false;
      }, 2000);
      await refreshNetworkStatus();
    } catch (error) {
      daemonNetworkError = error instanceof Error ? error.message : 'Failed to apply daemon network settings';
    } finally {
      daemonNetworkBusy = false;
      daemonNetworkLoaded = true;
    }
  }

  async function handleTransportToggle(
    key: keyof Pick<DaemonNetworkSettings, 'webrtc' | 'multicast' | 'bluetooth'>,
  ) {
    if (daemonNetworkBusy) return;

    const nextSettings = cloneDaemonNetworkSettings(daemonNetworkDraft);
    nextSettings[key] = !nextSettings[key];
    daemonNetworkDraft = nextSettings;
    await applyDaemonNetworkSettings(nextSettings);
  }

  function updateDaemonNetworkDraft(patch: Partial<DaemonNetworkSettings>) {
    daemonNetworkDraft = {
      ...daemonNetworkDraft,
      ...patch,
    };
    daemonNetworkSaved = false;
  }

  async function handleApplyDaemonNetworkSettings() {
    if (daemonNetworkBusy) return;
    await applyDaemonNetworkSettings(daemonNetworkDraft);
  }

  function isWebSocketUrl(url: string): boolean {
    try {
      const parsed = new URL(url);
      return parsed.protocol === 'ws:' || parsed.protocol === 'wss:';
    } catch {
      return false;
    }
  }

  function isHttpUrl(url: string): boolean {
    try {
      const parsed = new URL(url);
      return parsed.protocol === 'http:' || parsed.protocol === 'https:';
    } catch {
      return false;
    }
  }

  function addRelay() {
    const url = newRelayUrl.trim();
    if (!url || !isWebSocketUrl(url) || daemonNetworkDraft.relayUrls.includes(url)) return;
    updateDaemonNetworkDraft({
      relayUrls: [...daemonNetworkDraft.relayUrls, url],
    });
    newRelayUrl = '';
  }

  function removeRelay(url: string) {
    updateDaemonNetworkDraft({
      relayUrls: daemonNetworkDraft.relayUrls.filter((relay) => relay !== url),
    });
  }

  function addBlossomServer() {
    const url = newBlossomUrl.trim();
    if (
      !url ||
      !isHttpUrl(url) ||
      daemonNetworkDraft.blossomServers.some((server) => server.url === url)
    ) return;
    updateDaemonNetworkDraft({
      blossomServers: [
        ...daemonNetworkDraft.blossomServers,
        { url, read: true, write: false } satisfies DaemonBlossomServerSettings,
      ],
    });
    newBlossomUrl = '';
  }

  function removeBlossomServer(url: string) {
    updateDaemonNetworkDraft({
      blossomServers: daemonNetworkDraft.blossomServers.filter((server) => server.url !== url),
    });
  }

  function toggleBlossomMode(url: string, key: 'read' | 'write') {
    updateDaemonNetworkDraft({
      blossomServers: daemonNetworkDraft.blossomServers.map((server) =>
        server.url === url ? { ...server, [key]: !server[key] } : server,
      ),
    });
  }

  function updateNumericSetting(
    key: keyof Pick<
      DaemonNetworkSettings,
      'maxMulticastPeers' | 'maxBluetoothPeers' | 'multicastPort'
    >,
    value: string,
  ) {
    const parsed = Number.parseInt(value, 10);
    daemonNetworkDraft = {
      ...daemonNetworkDraft,
      [key]: Number.isFinite(parsed) && parsed >= 0 ? parsed : 0,
    };
    daemonNetworkSaved = false;
  }

  function formatConfigRelayLabel(url: string): string {
    try {
      const parsed = new URL(url);
      return parsed.host || url;
    } catch {
      return url;
    }
  }

  function formatCount(value: number, singular: string, plural: string): string {
    return `${value} ${value === 1 ? singular : plural}`;
  }

  function formatBluetoothEventTime(timestampSeconds: number): string {
    if (!timestampSeconds) return 'Unknown time';
    return new Date(timestampSeconds * 1000).toLocaleString();
  }

  function shortEventId(value: string): string {
    if (value.length <= 18) return value;
    return `${value.slice(0, 8)}…${value.slice(-8)}`;
  }

  function nip07AccountLabel(account: Nip07AccountSummary): string {
    return animalName(account.pubkey);
  }

  function formatNpubLabel(npub: string): string {
    return npub.length > 24 ? `${npub.slice(0, 14)}…${npub.slice(-8)}` : npub;
  }

  function scheduleClipboardClear(secret: string) {
    if (clipboardClearTimer) {
      clearTimeout(clipboardClearTimer);
    }
    clipboardClearTimer = setTimeout(() => {
      void clearClipboardIfUnchanged(secret);
    }, CLIPBOARD_CLEAR_DELAY_MS);
  }

  function resetCopyFeedbackAfterDelay(pubkey: string) {
    if (copyFeedbackTimer) {
      clearTimeout(copyFeedbackTimer);
    }
    copyFeedbackTimer = setTimeout(() => {
      if (nip07CopySuccessPubkey === pubkey) {
        nip07CopySuccessPubkey = null;
      }
    }, COPY_FEEDBACK_DURATION_MS);
  }

  async function handleCopyNip07Secret(pubkey: string) {
    if (nip07CopyBusyPubkey) return;

    nip07CopyBusyPubkey = pubkey;
    nip07CopySuccessPubkey = null;
    nip07CopyError = '';
    try {
      if (!exportNip07Secret) {
        throw new Error('Nostr secret export is unavailable');
      }
      const secret = await exportNip07Secret(pubkey);
      await writeClipboardText(secret);
      scheduleClipboardClear(secret);
      nip07CopySuccessPubkey = pubkey;
      resetCopyFeedbackAfterDelay(pubkey);
    } catch (error) {
      nip07CopyError = error instanceof Error ? error.message : String(error);
    } finally {
      nip07CopyBusyPubkey = null;
    }
  }
</script>

<div class="flex min-h-0 flex-1 flex-col bg-surface-1 lg:flex-row">
  <aside
    class={`min-h-0 shrink-0 overflow-auto border-b border-surface-2 bg-surface-1 lg:w-[22rem] lg:border-b-0 lg:border-r ${isSettingsRootRoute ? 'flex flex-col' : 'hidden lg:flex lg:flex-col'}`}
  >
    <div class="w-full px-4 pb-8 pt-6 lg:px-5 lg:py-6">
      <div class="mb-6">
        <h1 class="text-2xl font-semibold text-text-1">Settings</h1>
      </div>

      <div class="overflow-hidden rounded-2xl bg-surface-2 shadow-sm ring-1 ring-surface-3/80">
        {#each tabs as tab, index (tab.id)}
          <button
            data-testid={tab.id === 'users' ? 'settings-users-tab' : `settings-nav-${tab.id}`}
            onclick={() => selectTab(tab.id)}
            aria-current={activeTab === tab.id ? 'page' : undefined}
            class={`relative flex w-full items-center gap-3 px-4 py-3 text-left transition-colors ${activeTab === tab.id ? tab.activeRowClass : 'hover:bg-surface-3/40'}`}
          >
            <span class={`flex h-9 w-9 shrink-0 items-center justify-center rounded-xl ${tab.iconFrameClass}`}>
              <span class={tab.icon}></span>
            </span>
            <span class="min-w-0 flex-1 text-sm font-medium text-text-1">{tab.label}</span>
            <span class="i-lucide-chevron-right shrink-0 text-base text-text-3"></span>
            {#if index < tabs.length - 1}
              <span class="absolute bottom-0 left-16 right-0 border-b border-surface-3/70"></span>
            {/if}
          </button>
        {/each}
      </div>
    </div>
  </aside>

  <section class={`min-w-0 flex-1 overflow-auto ${isSettingsRootRoute ? 'hidden lg:block' : 'block'}`}>
    <div class="w-full px-4 pb-8 pt-6 lg:px-8 lg:py-8">
      <div class="mb-6 lg:hidden">
        <button
          class="inline-flex items-center gap-2 rounded-full bg-surface-2 px-3 py-2 text-sm font-medium text-text-1 transition-colors hover:bg-surface-3"
          onclick={openSettingsIndex}
        >
          <span class="i-lucide-chevron-left text-base"></span>
          <span>Settings</span>
        </button>
      </div>

      <div class="mb-6">
        <h2 class="text-2xl font-semibold text-text-1">{activeTabMeta.label}</h2>
      </div>

      <div class="w-full space-y-6">
        {#if activeTab === 'app'}
        <div>
          <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">
            App
          </h3>
          <p class="text-xs text-text-3 mb-3">Native shell behavior on this device</p>
          <div class="bg-surface-2 rounded divide-y divide-surface-3">
            <label class="flex items-center justify-between gap-4 p-3">
              <div>
                <div class="text-sm font-medium text-text-1">Launch at startup</div>
                <div class="text-xs text-text-3">Open Iris automatically when you log in</div>
              </div>
              <button
                class="relative h-6 w-11 shrink-0 overflow-hidden rounded-full transition-colors {autostart ? 'bg-accent' : 'bg-surface-3'}"
                onclick={handleAutostartToggle}
                aria-label="Toggle launch at startup"
              >
                <span class="absolute left-0.5 top-0.5 h-5 w-5 rounded-full bg-white transition-transform {autostart ? 'translate-x-5' : ''}"></span>
              </button>
            </label>
          </div>
        </div>
      {:else if activeTab === 'privacy'}
        <div>
          <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">
            Browsing History
          </h3>
          <p class="text-xs text-text-3 mb-3">Shell-local history stored on this device</p>
          <div class="bg-surface-2 rounded p-3 flex items-center justify-between gap-4">
            <div>
              <div class="text-sm font-medium text-text-1">Browsing history</div>
              <div class="text-xs text-text-3">Clear saved addresses and recent visits</div>
            </div>
            {#if historyCleared}
              <span class="text-sm text-success font-medium">Cleared!</span>
            {:else}
              <button
                class="rounded-lg px-3 py-2 text-sm text-text-1 hover:bg-surface-3 transition-colors"
                onclick={handleClearHistory}
              >
                Clear history
              </button>
            {/if}
          </div>
        </div>
      {:else if activeTab === 'users'}
        <div data-testid="settings-users-panel" class="space-y-4">
          <div>
            <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">
              Nostr Users
            </h3>
            <p class="text-xs text-text-3 mb-3">
              Copy an account `nsec` directly to the clipboard without showing it on screen. If the clipboard still contains that exact `nsec` after 30 seconds, Iris clears it.
            </p>
          </div>

          {#if sortedNip07Accounts.length === 0}
            <div class="rounded-2xl bg-surface-2 px-4 py-4 text-sm text-text-2">
              No Nostr users are stored in this shell yet. Add or generate one from the account menu first.
            </div>
          {:else}
            <div class="space-y-3">
              {#each sortedNip07Accounts as account (account.pubkey)}
                {@const isActive = account.pubkey === activeNip07AccountPubkey}
                {@const isCopying = nip07CopyBusyPubkey === account.pubkey}
                {@const isCopied = nip07CopySuccessPubkey === account.pubkey}
                <div class="rounded-2xl bg-surface-2 p-4 flex items-center gap-3">
                  <div class="flex h-12 w-12 shrink-0 items-center justify-center overflow-hidden rounded-full bg-surface-1">
                    <img
                      src={`data:image/svg+xml;utf8,${encodeURIComponent(minidenticon(account.pubkey, 48, 48))}`}
                      alt={nip07AccountLabel(account)}
                      class="h-full w-full rounded-full"
                    />
                  </div>
                  <div class="min-w-0 flex-1">
                    <div class="flex flex-wrap items-center gap-2">
                      <div class="truncate text-sm font-medium text-text-1">
                        {nip07AccountLabel(account)}
                      </div>
                      {#if isActive}
                        <span class="rounded-full bg-success/15 px-2 py-0.5 text-[11px] font-medium text-success">
                          Active
                        </span>
                      {/if}
                    </div>
                    <div class="mt-1 text-xs text-text-3">
                      {formatNpubLabel(account.npub)}
                    </div>
                  </div>
                  <button
                    data-testid={`copy-nsec-button-${account.pubkey}`}
                    class="btn shrink-0 bg-surface-1 text-text-1 hover:bg-surface-3 disabled:opacity-60"
                    onclick={() => void handleCopyNip07Secret(account.pubkey)}
                    disabled={!!nip07CopyBusyPubkey}
                  >
                    {#if isCopying}
                      Copying…
                    {:else if isCopied}
                      Copied
                    {:else}
                      Copy nsec
                    {/if}
                  </button>
                </div>
              {/each}
            </div>
          {/if}

          {#if nip07CopyError}
            <div data-testid="users-copy-error" class="rounded-xl bg-danger/10 px-3 py-2 text-sm text-danger">
              {nip07CopyError}
            </div>
          {/if}
        </div>
      {:else if activeTab === 'network'}
        <div class="space-y-6">
          <div>
            <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">
              Local Service
            </h3>
            <p class="text-xs text-text-3 mb-3">Runs on this device and handles storage, sync, and peer connections.</p>
            <div class="bg-surface-2 rounded p-3 space-y-3">
              <div class="flex items-center justify-between gap-4">
                <span class="text-sm text-text-3">Address</span>
                <span class="text-sm text-text-1 font-mono break-all text-right">
                  {daemonUrl || 'Unavailable'}
                </span>
              </div>
              <div class="flex items-center justify-between gap-4">
                <span class="text-sm text-text-3">Status</span>
                <span class="text-sm text-text-1">
                  {#if networkStatusError}
                    Degraded
                  {:else if networkStatusLoaded}
                    Running
                  {:else}
                    Loading
                  {/if}
                </span>
              </div>
              {#if hasPendingDaemonNetworkChanges || daemonNetworkBusy || daemonNetworkSaved || daemonNetworkError}
              <div class="rounded bg-surface-1/70 p-2.5 space-y-2">
                <div class="flex items-center justify-between gap-4">
                  <div>
                    <div class="text-sm font-medium text-text-1">Network changes</div>
                    <div class="text-xs text-text-3">Save relay, Blossom, and transport changes.</div>
                  </div>
                  <div class="flex items-center gap-3">
                    {#if daemonNetworkBusy}
                      <span class="text-xs text-text-3">Applying…</span>
                    {:else if daemonNetworkSaved && !hasPendingDaemonNetworkChanges}
                      <span class="text-xs text-success">Saved</span>
                    {/if}
                    <button
                      class="rounded-lg px-3 py-2 text-sm text-text-1 transition-colors disabled:opacity-50 disabled:cursor-default {hasPendingDaemonNetworkChanges ? 'bg-surface-2 hover:bg-surface-3' : 'bg-surface-2'}"
                      onclick={() => void handleApplyDaemonNetworkSettings()}
                      disabled={!daemonNetworkLoaded || daemonNetworkBusy || !hasPendingDaemonNetworkChanges}
                    >
                      Apply
                    </button>
                  </div>
                </div>
                {#if daemonNetworkError}
                  <p class="text-xs text-text-3">{daemonNetworkError}</p>
                {/if}
              </div>
              {/if}
            </div>
            {#if networkStatusError}
              <p class="mt-2 text-xs text-text-3">{networkStatusError}</p>
            {/if}
          </div>

          <div class="grid gap-3 lg:grid-cols-2">
            <div class="rounded bg-surface-2 p-3 space-y-3">
              <div>
                <div class="flex items-center justify-between gap-4">
                  <div class="min-w-0">
                    <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">Nostr Relays</h3>
                    <p class="text-xs text-text-3">
                      Syncs Nostr events and follow graph data, and helps Iris find remote peers.
                    </p>
                  </div>
                  <button
                    class="relative h-6 w-11 shrink-0 overflow-hidden rounded-full transition-colors {daemonNetworkDraft.nostrRelaysEnabled ? 'bg-accent' : 'bg-surface-3'}"
                    onclick={() => updateDaemonNetworkDraft({ nostrRelaysEnabled: !daemonNetworkDraft.nostrRelaysEnabled })}
                    aria-label="Toggle Nostr relays"
                    disabled={!daemonNetworkLoaded || daemonNetworkBusy}
                  >
                    <span class="absolute left-0.5 top-0.5 h-5 w-5 rounded-full bg-white transition-transform {daemonNetworkDraft.nostrRelaysEnabled ? 'translate-x-5' : ''}"></span>
                  </button>
                </div>
              </div>
              <div class="text-xs text-text-3">
                {#if daemonNetworkDraft.nostrRelaysEnabled}
                  {formatCount(daemonNetworkDraft.relayUrls.length, 'relay enabled', 'relays enabled')}
                {:else}
                  Relays disabled
                {/if}
              </div>
              <div class={`space-y-2 ${daemonNetworkDraft.nostrRelaysEnabled ? '' : 'opacity-60'}`}>
                {#if daemonNetworkDraft.relayUrls.length === 0}
                  <div class="rounded bg-surface-1/70 px-3 py-2 text-sm text-text-3">
                    No relays configured
                  </div>
                {:else}
                  {#each daemonNetworkDraft.relayUrls as relayUrl (relayUrl)}
                    <div class="flex items-center gap-2 rounded bg-surface-1/70 px-3 py-2">
                      <div class="min-w-0 flex-1">
                        <div class="text-sm text-text-1 truncate">{formatConfigRelayLabel(relayUrl)}</div>
                        <div class="text-xs text-text-3 font-mono truncate">{relayUrl}</div>
                      </div>
                      <button
                        class="rounded p-2 text-text-3 hover:bg-surface-3 hover:text-text-1 transition-colors"
                        onclick={() => removeRelay(relayUrl)}
                        aria-label={`Remove relay ${relayUrl}`}
                      >
                        <span class="i-lucide-x text-sm"></span>
                      </button>
                    </div>
                  {/each}
                {/if}
              </div>
              <div class="flex gap-2">
                <input
                  class="min-w-0 flex-1 rounded-lg bg-surface-1 px-3 py-2 text-sm text-text-1 outline-none ring-0"
                  type="url"
                  placeholder="wss://relay.example"
                  value={newRelayUrl}
                  oninput={(event) => newRelayUrl = event.currentTarget.value}
                  aria-label="Add relay URL"
                  disabled={!daemonNetworkLoaded || daemonNetworkBusy}
                />
                <button
                  class="rounded-lg bg-surface-1 px-3 py-2 text-sm text-text-1 hover:bg-surface-3 transition-colors disabled:opacity-50"
                  onclick={addRelay}
                  disabled={!newRelayUrl.trim() || !daemonNetworkLoaded || daemonNetworkBusy}
                >
                  Add
                </button>
              </div>
            </div>

            <div class="rounded bg-surface-2 p-3 space-y-3">
              <div>
                <div class="flex items-center justify-between gap-4">
                  <div class="min-w-0">
                    <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">Blossom</h3>
                    <p class="text-xs text-text-3">
                      Downloads or uploads files from configured servers when they are not local or on a connected peer.
                    </p>
                  </div>
                  <button
                    class="relative h-6 w-11 shrink-0 overflow-hidden rounded-full transition-colors {daemonNetworkDraft.blossomEnabled ? 'bg-accent' : 'bg-surface-3'}"
                    onclick={() => updateDaemonNetworkDraft({ blossomEnabled: !daemonNetworkDraft.blossomEnabled })}
                    aria-label="Toggle Blossom fallback"
                    disabled={!daemonNetworkLoaded || daemonNetworkBusy}
                  >
                    <span class="absolute left-0.5 top-0.5 h-5 w-5 rounded-full bg-white transition-transform {daemonNetworkDraft.blossomEnabled ? 'translate-x-5' : ''}"></span>
                  </button>
                </div>
              </div>
              <div class="text-xs text-text-3">
                {#if daemonNetworkDraft.blossomEnabled}
                  {formatCount(daemonNetworkDraft.blossomServers.filter((server) => server.read).length, 'read server enabled', 'read servers enabled')}
                {:else}
                  Blossom fallback disabled
                {/if}
              </div>
              <div class="space-y-2">
                {#if daemonNetworkDraft.blossomServers.length === 0}
                  <div class="rounded bg-surface-1/70 px-3 py-2 text-sm text-text-3">
                    No Blossom servers configured
                  </div>
                {:else}
                  {#each daemonNetworkDraft.blossomServers as server (server.url)}
                    <div class="rounded bg-surface-1/70 px-3 py-2 space-y-2">
                      <div class="flex items-start gap-2">
                        <div class="min-w-0 flex-1">
                          <div class="text-sm text-text-1 truncate">{formatConfigRelayLabel(server.url)}</div>
                          <div class="text-xs text-text-3 font-mono truncate">{server.url}</div>
                        </div>
                        <button
                          class="rounded p-2 text-text-3 hover:bg-surface-3 hover:text-text-1 transition-colors"
                          onclick={() => removeBlossomServer(server.url)}
                          aria-label={`Remove Blossom server ${server.url}`}
                        >
                          <span class="i-lucide-x text-sm"></span>
                        </button>
                      </div>
                      <div class="flex flex-wrap gap-2">
                        <button
                          class="rounded px-2 py-1 text-xs transition-colors {server.read ? 'bg-accent/20 text-text-1' : 'bg-surface-2 text-text-3 hover:text-text-1'}"
                          onclick={() => toggleBlossomMode(server.url, 'read')}
                          aria-label={`Toggle Blossom read for ${server.url}`}
                        >
                          Read
                        </button>
                        <button
                          class="rounded px-2 py-1 text-xs transition-colors {server.write ? 'bg-accent/20 text-text-1' : 'bg-surface-2 text-text-3 hover:text-text-1'}"
                          onclick={() => toggleBlossomMode(server.url, 'write')}
                          aria-label={`Toggle Blossom write for ${server.url}`}
                        >
                          Write
                        </button>
                      </div>
                    </div>
                  {/each}
                {/if}
              </div>
              <div class="flex gap-2">
                <input
                  class="min-w-0 flex-1 rounded-lg bg-surface-1 px-3 py-2 text-sm text-text-1 outline-none ring-0"
                  type="url"
                  placeholder="https://cdn.example"
                  value={newBlossomUrl}
                  oninput={(event) => newBlossomUrl = event.currentTarget.value}
                  aria-label="Add Blossom server URL"
                  disabled={!daemonNetworkLoaded || daemonNetworkBusy}
                />
                <button
                  class="rounded-lg bg-surface-1 px-3 py-2 text-sm text-text-1 hover:bg-surface-3 transition-colors disabled:opacity-50"
                  onclick={addBlossomServer}
                  disabled={!newBlossomUrl.trim() || !daemonNetworkLoaded || daemonNetworkBusy}
                >
                  Add
                </button>
              </div>
            </div>
          </div>

          <div>
            <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">
              Peer Router
            </h3>
            <p class="text-xs text-text-3 mb-3">
              Controls direct mesh transport and local-network discovery.
            </p>
            <div class="rounded bg-surface-2 p-3 space-y-3">
              <div class="space-y-2">
                <label class="flex items-center justify-between gap-4 rounded bg-surface-1/70 px-3 py-2">
                  <div class="min-w-0 flex-1">
                    <div class="text-sm font-medium text-text-1">WebRTC</div>
                    <div class="text-xs text-text-3">Direct peer connections, usually negotiated through relays</div>
                  </div>
                  <button
                    class="relative h-6 w-11 shrink-0 overflow-hidden rounded-full transition-colors {daemonNetworkDraft.webrtc ? 'bg-accent' : 'bg-surface-3'}"
                    onclick={() => void handleTransportToggle('webrtc')}
                    aria-label="Toggle WebRTC transport"
                    disabled={!daemonNetworkLoaded || daemonNetworkBusy}
                  >
                    <span class="absolute left-0.5 top-0.5 h-5 w-5 rounded-full bg-white transition-transform {daemonNetworkDraft.webrtc ? 'translate-x-5' : ''}"></span>
                  </button>
                </label>
                <label class="flex items-center justify-between gap-4 rounded bg-surface-1/70 px-3 py-2">
                  <div class="min-w-0 flex-1">
                    <div class="text-sm font-medium text-text-1">LAN multicast</div>
                    <div class="text-xs text-text-3">Discovery and root lookups on the local network</div>
                  </div>
                  <button
                    class="relative h-6 w-11 shrink-0 overflow-hidden rounded-full transition-colors {daemonNetworkDraft.multicast ? 'bg-accent' : 'bg-surface-3'}"
                    onclick={() => void handleTransportToggle('multicast')}
                    aria-label="Toggle LAN multicast transport"
                    disabled={!daemonNetworkLoaded || daemonNetworkBusy}
                  >
                    <span class="absolute left-0.5 top-0.5 h-5 w-5 rounded-full bg-white transition-transform {daemonNetworkDraft.multicast ? 'translate-x-5' : ''}"></span>
                  </button>
                </label>
                <label class="flex items-center justify-between gap-4 rounded bg-surface-1/70 px-3 py-2">
                  <div class="min-w-0 flex-1">
                    <div class="text-sm font-medium text-text-1">Bluetooth</div>
                    <div class="text-xs text-text-3">Nearby Nostr event sync with adjacent devices</div>
                  </div>
                  <button
                    class="relative h-6 w-11 shrink-0 overflow-hidden rounded-full transition-colors {daemonNetworkDraft.bluetooth ? 'bg-accent' : 'bg-surface-3'}"
                    onclick={() => void handleTransportToggle('bluetooth')}
                    aria-label="Toggle Bluetooth transport"
                    disabled={!daemonNetworkLoaded || daemonNetworkBusy}
                  >
                    <span class="absolute left-0.5 top-0.5 h-5 w-5 rounded-full bg-white transition-transform {daemonNetworkDraft.bluetooth ? 'translate-x-5' : ''}"></span>
                  </button>
                </label>
              </div>

              <div class="grid gap-3 sm:grid-cols-2">
                <label class="space-y-1">
                  <span class="text-xs uppercase tracking-wide text-text-3">Multicast Group</span>
                  <input
                    class="w-full rounded-lg bg-surface-1 px-3 py-2 text-sm text-text-1 outline-none ring-0"
                    type="text"
                    value={daemonNetworkDraft.multicastGroup}
                    oninput={(event) => updateDaemonNetworkDraft({ multicastGroup: event.currentTarget.value })}
                    aria-label="Multicast group"
                  />
                </label>
                <label class="space-y-1">
                  <span class="text-xs uppercase tracking-wide text-text-3">Multicast Port</span>
                  <input
                    class="w-full rounded-lg bg-surface-1 px-3 py-2 text-sm text-text-1 outline-none ring-0"
                    type="number"
                    min="0"
                    value={daemonNetworkDraft.multicastPort}
                    oninput={(event) => updateNumericSetting('multicastPort', event.currentTarget.value)}
                    aria-label="Multicast port"
                  />
                </label>
                <label class="space-y-1">
                  <span class="text-xs uppercase tracking-wide text-text-3">Max Multicast Peers</span>
                  <input
                    class="w-full rounded-lg bg-surface-1 px-3 py-2 text-sm text-text-1 outline-none ring-0"
                    type="number"
                    min="0"
                    value={daemonNetworkDraft.maxMulticastPeers}
                    oninput={(event) => updateNumericSetting('maxMulticastPeers', event.currentTarget.value)}
                    aria-label="Maximum multicast peers"
                  />
                </label>
                <label class="space-y-1">
                  <span class="text-xs uppercase tracking-wide text-text-3">Max Bluetooth Peers</span>
                  <input
                    class="w-full rounded-lg bg-surface-1 px-3 py-2 text-sm text-text-1 outline-none ring-0"
                    type="number"
                    min="0"
                    value={daemonNetworkDraft.maxBluetoothPeers}
                    oninput={(event) => updateNumericSetting('maxBluetoothPeers', event.currentTarget.value)}
                    aria-label="Maximum bluetooth peers"
                  />
                </label>
              </div>
            </div>
          </div>

          <div>
            <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">
              Mesh
            </h3>
            <p class="text-xs text-text-3 mb-3">
              Embedded daemon mesh activity on this device
            </p>

            <div class="grid gap-3 sm:grid-cols-2">
              <div class="rounded bg-surface-2 p-3">
                <div class="text-xs uppercase tracking-wide text-text-3">Peers</div>
                <div class="mt-1 text-lg font-semibold text-text-1">{meshStatus.connected} connected</div>
                <div class="mt-2 flex flex-wrap gap-2 text-xs text-text-3">
                  <span class="rounded bg-surface-1 px-2 py-1">{meshStatus.totalPeers} total</span>
                  <span class="rounded bg-surface-1 px-2 py-1">{meshStatus.withDataChannel} ready</span>
                </div>
              </div>

              <div class="rounded bg-surface-2 p-3">
                <div class="text-xs uppercase tracking-wide text-text-3">Transports</div>
                <div class="mt-2 grid gap-2 sm:grid-cols-2">
                  <div
                    role="group"
                    aria-label={`Bluetooth ${formatCount(meshStatus.transportCounts.bluetooth ?? 0, 'peer', 'peers')}`}
                    class="flex items-center justify-between rounded bg-surface-1 px-3 py-2 text-sm"
                  >
                    <span class="text-text-3">Bluetooth</span>
                    <span class="font-medium text-text-1">
                      {formatCount(meshStatus.transportCounts.bluetooth ?? 0, 'peer', 'peers')}
                    </span>
                  </div>
                  <div
                    role="group"
                    aria-label={`WebRTC ${formatCount(meshStatus.transportCounts.webrtc ?? 0, 'peer', 'peers')}`}
                    class="flex items-center justify-between rounded bg-surface-1 px-3 py-2 text-sm"
                  >
                    <span class="text-text-3">WebRTC</span>
                    <span class="font-medium text-text-1">
                      {formatCount(meshStatus.transportCounts.webrtc ?? 0, 'peer', 'peers')}
                    </span>
                  </div>
                </div>
                <div class="mt-2 text-xs text-text-3">
                  {formatCount(activeBlossomReadServerCount, 'blossom read server', 'blossom read servers')} · {formatCount(activeRelayCount, 'relay', 'relays')}
                </div>
              </div>
            </div>

            <div class="mt-3 rounded bg-surface-2 p-3">
              <div class="grid grid-cols-2 gap-3 text-xs">
                <div class="flex items-center justify-between rounded bg-surface-1/70 px-2 py-2">
                  <span class="text-text-3">Upload</span>
                  <span class="font-mono text-success">{formatBandwidth(meshUploadBandwidth)}</span>
                </div>
                <div class="flex items-center justify-between rounded bg-surface-1/70 px-2 py-2">
                  <span class="text-text-3">Download</span>
                  <span class="font-mono text-accent">{formatBandwidth(meshDownloadBandwidth)}</span>
                </div>
                <div class="flex items-center justify-between rounded bg-surface-1/70 px-2 py-2">
                  <span class="text-text-3">Sent</span>
                  <span class="font-mono text-success">{formatBytes(meshStatus.totalBytesSent)}</span>
                </div>
                <div class="flex items-center justify-between rounded bg-surface-1/70 px-2 py-2">
                  <span class="text-text-3">Received</span>
                  <span class="font-mono text-accent">{formatBytes(meshStatus.totalBytesReceived)}</span>
                </div>
              </div>
              <div class="mt-3">
                <BandwidthHistoryChart history={meshBandwidthHistory} />
              </div>
            </div>
          </div>

          <div>
            <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">
              Bluetooth Receipts
            </h3>
            <p class="text-xs text-text-3 mb-3">
              Recently ingested Nostr events that arrived over Bluetooth.
            </p>
            {#if recentBluetoothEvents.length === 0}
              <div class="rounded bg-surface-2 p-3 text-sm text-text-3">
                No Bluetooth-received events recorded yet
              </div>
            {:else}
              <div class="rounded bg-surface-2 divide-y divide-surface-3">
                {#each recentBluetoothEvents as event (event.eventId)}
                  <div class="p-3 text-sm">
                    <div class="flex flex-wrap items-center gap-2">
                      <span class="rounded bg-surface-1 px-2 py-1 text-[10px] uppercase tracking-wide text-text-3">
                        kind {event.kind}
                      </span>
                      {#if event.peerId}
                        <span class="rounded bg-surface-1 px-2 py-1 text-[10px] uppercase tracking-wide text-text-3">
                          {event.peerId}
                        </span>
                      {/if}
                    </div>
                    <div class="mt-2 font-mono text-xs text-text-1 break-all">{shortEventId(event.eventId)}</div>
                    <div class="mt-1 text-xs text-text-3 break-all">{event.pubkey}</div>
                    <div class="mt-2 text-xs text-text-3">
                      Received {formatBluetoothEventTime(event.receivedAt)}
                    </div>
                    {#if event.cidValues.length > 0}
                      <div class="mt-2 flex flex-wrap gap-2">
                        {#each event.cidValues as cid (cid)}
                          <span class="rounded bg-surface-1 px-2 py-1 font-mono text-[11px] text-text-1 break-all">
                            {cid}
                          </span>
                        {/each}
                      </div>
                    {/if}
                  </div>
                {/each}
              </div>
            {/if}
          </div>

          <div>
            <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">
              Active Peers
            </h3>
            <p class="text-xs text-text-3 mb-3">Connected peers only.</p>
            {#if inactiveMeshPeerCount > 0}
              <p class="mb-3 text-xs text-text-3">
                {formatCount(inactiveMeshPeerCount, 'discovered peer not connected yet', 'discovered peers not connected yet')}
              </p>
            {/if}
            {#if connectedMeshPeers.length === 0}
              <div class="rounded bg-surface-2 p-3 text-sm text-text-3">
                No mesh peers connected
              </div>
            {:else}
              <div class="bg-surface-2 rounded divide-y divide-surface-3">
                {#each connectedMeshPeers as peer, index (peer.id)}
                  <div class="p-3">
                    <div class="flex items-center gap-2 text-sm">
                      <span class={`h-2 w-2 shrink-0 rounded-full ${stateColor(peer.state)}`}></span>
                      <div class="min-w-0 flex flex-1 items-center gap-2">
                        <div class="flex h-8 w-8 shrink-0 items-center justify-center overflow-hidden rounded-full bg-surface-1">
                          <img src={peerIdenticonUri(peer)} alt="" width="24" height="24" class="h-6 w-6" />
                        </div>
                        <div class="min-w-0 flex-1">
                          <div class="truncate font-medium text-text-1">
                            {peerIdentityLabel(peer, index)}
                          </div>
                          <div class="truncate text-[11px] text-text-3">
                            {peerIdentitySubtitle(peer)}
                          </div>
                        </div>
                      </div>
                      <div class="flex shrink-0 items-center gap-1">
                        <span class="rounded bg-surface-1 px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-text-3">
                          {transportLabel(peer.transport)}
                        </span>
                        {#if relationshipLabel(peer.pool)}
                          <span class="rounded bg-surface-1 px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-text-3">
                            {relationshipLabel(peer.pool)}
                          </span>
                        {/if}
                      </div>
                    </div>
                    <div class="mt-2 flex flex-wrap gap-x-3 gap-y-1 text-xs text-text-3">
                      <span class="text-success">
                        <span class="i-lucide-arrow-up inline-block align-middle mr-0.5"></span>{formatBytes(peer.bytesSent)}
                      </span>
                      <span class="text-accent">
                        <span class="i-lucide-arrow-down inline-block align-middle mr-0.5"></span>{formatBytes(peer.bytesReceived)}
                      </span>
                    </div>
                  </div>
                {/each}
              </div>
            {/if}
          </div>
        </div>
      {:else if activeTab === 'about'}
        <div class="space-y-6">
          <div>
            <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">
              About
            </h3>
            <p class="text-xs text-text-3 mb-3">Native shell for browsing distributed htree apps</p>
            <div class="bg-surface-2 rounded p-3 space-y-3 text-sm">
              <div class="flex items-center justify-between gap-4">
                <span class="text-text-3">Stack</span>
                <span class="text-text-1">Tauri + Svelte</span>
              </div>
              <div class="flex items-center justify-between gap-4">
                <span class="text-text-3">Build</span>
                <span class="text-text-1 font-mono text-xs text-right">{buildLabel}</span>
              </div>
            </div>
          </div>

          <div>
            <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">
              Source Browser
            </h3>
            <p class="text-xs text-text-3 mb-3">Open the project in Iris Git over htree URLs</p>
            <div class="bg-surface-2 rounded divide-y divide-surface-3">
              {#each sourceLinks as link (link.url)}
                <button
                  class="w-full p-3 text-left hover:bg-surface-3 transition-colors flex items-start gap-3"
                  onclick={() => openSource(link.url)}
                >
                  <span class="{link.icon} mt-0.5 text-text-3 shrink-0"></span>
                  <span class="min-w-0 flex-1">
                    <span class="block text-sm font-medium text-text-1">{link.label}</span>
                    <span class="block text-xs text-text-3 mt-1">{link.description}</span>
                    <span class="block text-xs text-text-3 font-mono mt-2 break-all">{link.url}</span>
                  </span>
                </button>
              {/each}
            </div>
          </div>

          <div>
            <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">
              Actions
            </h3>
            <div class="bg-surface-2 rounded p-3">
              <button
                onclick={() => window.location.reload()}
                class="w-full rounded-lg px-3 py-2 text-sm text-text-1 hover:bg-surface-3 transition-colors flex items-center justify-center gap-2"
              >
                <span class="i-lucide-refresh-cw text-sm"></span>
                <span>Refresh App</span>
              </button>
            </div>
          </div>
        </div>
      {/if}
    </div>
  </div>
  </section>
</div>
