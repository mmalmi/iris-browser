/**
 * Tauri invoke wrappers for the iris shell.
 *
 * These wrap the Rust commands exposed in src-tauri/src/ for
 * webview management, history, autostart, and daemon URL.
 */

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

// ── Daemon URL ──

export async function getHtreeServerUrl(): Promise<string> {
  return invoke<string>('get_htree_server_url');
}

export async function deepLinkFrontendReady(): Promise<string[]> {
  return invoke<string[]>('deep_link_frontend_ready');
}

export interface DaemonTransportSettings {
  webrtc: boolean;
  multicast: boolean;
  bluetooth: boolean;
  maxMulticastPeers: number;
  maxBluetoothPeers: number;
}

export interface DaemonBlossomServerSettings {
  url: string;
  read: boolean;
  write: boolean;
}

export interface DaemonNetworkSettings extends DaemonTransportSettings {
  nostrRelaysEnabled: boolean;
  blossomEnabled: boolean;
  multicastGroup: string;
  multicastPort: number;
  relayUrls: string[];
  blossomServers: DaemonBlossomServerSettings[];
}

export async function getDaemonTransportSettings(): Promise<DaemonTransportSettings> {
  return invoke<DaemonTransportSettings>('get_daemon_transport_settings');
}

export async function updateDaemonTransportSettings(
  settings: DaemonTransportSettings,
): Promise<DaemonTransportSettings> {
  return invoke<DaemonTransportSettings>('update_daemon_transport_settings', { settings });
}

export async function getDaemonNetworkSettings(): Promise<DaemonNetworkSettings> {
  return invoke<DaemonNetworkSettings>('get_daemon_network_settings');
}

export async function updateDaemonNetworkSettings(
  settings: DaemonNetworkSettings,
): Promise<DaemonNetworkSettings> {
  return invoke<DaemonNetworkSettings>('update_daemon_network_settings', { settings });
}

// ── Child webview management ──

export async function createNip07Webview(
  label: string,
  url: string,
  x: number,
  y: number,
  width: number,
  height: number,
): Promise<void> {
  return invoke<void>('create_nip07_webview', {
    label,
    url,
    x,
    y,
    width,
    height,
    scale: currentDeviceScale(),
  });
}

export async function createHtreeWebview(
  label: string,
  opts: {
    host?: string;
    nhash?: string;
    npub?: string;
    treename?: string;
    path: string;
    query?: string;
    fragment?: string;
    cacheBust?: string;
  },
  x: number,
  y: number,
  width: number,
  height: number,
  preferPlainLoopbackHost?: boolean,
): Promise<void> {
  return invoke<void>('create_htree_webview', {
    label,
    host: opts.host ?? null,
    nhash: opts.nhash ?? null,
    npub: opts.npub ?? null,
    treename: opts.treename ?? null,
    path: opts.path,
    query: opts.query ?? null,
    fragment: opts.fragment ?? null,
    cacheBust: opts.cacheBust ?? null,
    x,
    y,
    width,
    height,
    scale: currentDeviceScale(),
    preferPlainLoopbackHost: preferPlainLoopbackHost ?? null,
  });
}

export async function cacheTreeRoot(
  npub: string,
  treeName: string,
  hash: string,
  key?: string | null,
  visibility?: string | null,
  nhash?: string | null,
): Promise<void> {
  return invoke<void>('cache_tree_root', {
    npub,
    treeName,
    hash,
    key: key ?? null,
    visibility: visibility ?? null,
    nhash: nhash ?? null,
  });
}

export async function clearTreeRootCache(
  npub: string,
  treeName: string,
  key?: string | null,
  visibility?: string | null,
): Promise<void> {
  return invoke<void>('clear_tree_root_cache', {
    npub,
    treeName,
    key: key ?? null,
    visibility: visibility ?? null,
  });
}

export async function closeWebview(label: string): Promise<void> {
  return invoke<void>('close_webview', { label });
}

export async function navigateWebview(label: string, url: string): Promise<void> {
  return invoke<void>('navigate_webview', { label, url });
}

export async function webviewHistory(label: string, direction: 'back' | 'forward'): Promise<void> {
  return invoke<void>('webview_history', { label, direction });
}

export async function reloadWebview(label: string): Promise<void> {
  return invoke<void>('reload_webview', { label });
}

export async function setWebviewBounds(
  label: string,
  x: number,
  y: number,
  width: number,
  height: number,
): Promise<void> {
  return invoke<void>('set_webview_bounds', {
    label,
    x,
    y,
    width,
    height,
    scale: currentDeviceScale(),
  });
}

export async function setMobileShellOverlay(
  enabled: boolean,
  x: number,
  y: number,
  width: number,
  height: number,
): Promise<void> {
  return invoke<void>('set_mobile_shell_overlay', {
    enabled,
    x,
    y,
    width,
    height,
    scale: currentDeviceScale(),
  });
}

export async function webviewCurrentUrl(label: string): Promise<string> {
  return invoke<string>('webview_current_url', { label });
}

export interface InstalledSitePwa {
  name: string;
  launchUrl: string;
  iconUrl?: string | null;
  sourceAppId?: string | null;
  sourceUrl: string;
  sourceManifestUrl: string;
}

export async function installSitePwa(url: string): Promise<InstalledSitePwa> {
  return invoke<InstalledSitePwa>('install_site_pwa', { url });
}

export async function cacheBookmarkIcon(args: {
  sourceUrl?: string | null;
  sourceManifestUrl?: string | null;
  iconUrl?: string | null;
}): Promise<string | null> {
  return invoke<string | null>('cache_bookmark_icon', {
    sourceUrl: args.sourceUrl ?? null,
    sourceManifestUrl: args.sourceManifestUrl ?? null,
    iconUrl: args.iconUrl ?? null,
  });
}

export interface Nip07AccountSummary {
  pubkey: string;
  npub: string;
  addedAt: number;
}

export interface Nip07AccountsSummary {
  accounts: Nip07AccountSummary[];
  activePubkey: string | null;
}

export interface Nip07PermissionPrompt {
  requestId: string;
  origin: string;
  method: string;
}

export async function getNip07Account(): Promise<Nip07AccountSummary | null> {
  return invoke<Nip07AccountSummary | null>('get_nip07_account');
}

export async function listNip07Accounts(): Promise<Nip07AccountsSummary> {
  return invoke<Nip07AccountsSummary>('list_nip07_accounts');
}

export async function loginNip07Account(secret: string): Promise<Nip07AccountSummary> {
  return invoke<Nip07AccountSummary>('login_nip07_account', { secret });
}

export async function generateNip07Account(): Promise<Nip07AccountSummary> {
  return invoke<Nip07AccountSummary>('generate_nip07_account');
}

export async function logoutNip07Account(): Promise<void> {
  return invoke<void>('logout_nip07_account');
}

export async function setActiveNip07Account(pubkey: string): Promise<Nip07AccountSummary> {
  return invoke<Nip07AccountSummary>('set_active_nip07_account', { pubkey });
}

export async function removeNip07Account(pubkey: string): Promise<Nip07AccountsSummary> {
  return invoke<Nip07AccountsSummary>('remove_nip07_account', { pubkey });
}

export async function exportNip07AccountSecret(pubkey: string): Promise<string> {
  return invoke<string>('export_nip07_account_secret', { pubkey });
}

export async function takeNip07PermissionPrompt(): Promise<Nip07PermissionPrompt | null> {
  return invoke<Nip07PermissionPrompt | null>('take_nip07_permission_prompt');
}

export async function respondNip07PermissionPrompt(
  requestId: string,
  decision: 'deny' | 'allowSession' | 'allowAlways' | 'blockSite',
): Promise<void> {
  return invoke<void>('respond_nip07_permission_prompt', {
    requestId,
    decision,
  });
}

export async function showNativeNip07PermissionDialog(
  origin: string,
  method: string,
): Promise<'deny' | 'allowSession' | 'allowAlways' | 'blockSite' | null> {
  return invoke<'deny' | 'allowSession' | 'allowAlways' | 'blockSite' | null>(
    'show_native_nip07_permission_dialog',
    {
      origin,
      method,
    },
  );
}

// ── History ──

export interface HistoryEntry {
  path: string;
  label: string;
  entry_type: string;
  npub?: string;
  tree_name?: string;
  visit_count: number;
  last_visited: number;
  first_visited: number;
}

const FALLBACK_HISTORY_STORAGE_KEY = 'iris.addressBarHistory.v1';

function currentDeviceScale(): number {
  if (typeof window === 'undefined') return 1;
  const scale = window.devicePixelRatio;
  return Number.isFinite(scale) && scale > 0 ? scale : 1;
}

function historyStorage(): Storage | null {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    return null;
  }
}

function readFallbackHistory(): HistoryEntry[] {
  const storage = historyStorage();
  if (!storage) return [];
  try {
    const raw = storage.getItem(FALLBACK_HISTORY_STORAGE_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter((entry): entry is HistoryEntry => {
      return !!entry &&
        typeof entry.path === 'string' &&
        typeof entry.label === 'string' &&
        typeof entry.entry_type === 'string' &&
        typeof entry.visit_count === 'number' &&
        typeof entry.last_visited === 'number' &&
        typeof entry.first_visited === 'number';
    });
  } catch {
    return [];
  }
}

function writeFallbackHistory(entries: HistoryEntry[]) {
  const storage = historyStorage();
  if (!storage) return;
  storage.setItem(FALLBACK_HISTORY_STORAGE_KEY, JSON.stringify(entries));
}

function upsertFallbackHistory(entry: {
  path: string;
  label: string;
  entry_type: string;
  npub?: string | null;
  tree_name?: string | null;
}) {
  const now = Date.now();
  const entries = readFallbackHistory();
  const existing = entries.find((item) => item.path === entry.path);
  if (existing) {
    existing.label = entry.label;
    existing.entry_type = entry.entry_type;
    existing.npub = entry.npub ?? undefined;
    existing.tree_name = entry.tree_name ?? undefined;
    existing.visit_count += 1;
    existing.last_visited = now;
  } else {
    entries.push({
      path: entry.path,
      label: entry.label,
      entry_type: entry.entry_type,
      npub: entry.npub ?? undefined,
      tree_name: entry.tree_name ?? undefined,
      visit_count: 1,
      last_visited: now,
      first_visited: now,
    });
  }
  entries.sort((a, b) => b.last_visited - a.last_visited);
  writeFallbackHistory(entries.slice(0, 1000));
}

function deleteFallbackHistoryEntry(path: string): boolean {
  const entries = readFallbackHistory();
  const nextEntries = entries.filter((entry) => entry.path !== path);
  if (nextEntries.length === entries.length) return false;
  writeFallbackHistory(nextEntries);
  return true;
}

function clearFallbackHistory() {
  const storage = historyStorage();
  storage?.removeItem(FALLBACK_HISTORY_STORAGE_KEY);
}

function getFallbackRecentHistory(limit: number): HistoryEntry[] {
  return readFallbackHistory()
    .sort((a, b) => b.last_visited - a.last_visited)
    .slice(0, limit);
}

function scoreFallbackHistory(query: string, entry: HistoryEntry): number {
  const normalizedQuery = query.toLowerCase();
  const label = entry.label.toLowerCase();
  const path = entry.path.toLowerCase();
  if (path === normalizedQuery) return 10;
  if (label === normalizedQuery) return 9;
  if (label.startsWith(normalizedQuery)) return 8;
  if (path.startsWith(normalizedQuery)) return 7;
  if (label.includes(normalizedQuery)) return 6;
  if (path.includes(normalizedQuery)) return 5;
  return 0;
}

export async function recordHistoryVisit(entry: {
  path: string;
  label: string;
  entry_type: string;
  npub?: string;
  tree_name?: string;
}): Promise<void> {
  upsertFallbackHistory(entry);
  try {
    await invoke<void>('record_history_visit', entry);
  } catch {
    // Keep fallback history even if native storage is unavailable.
  }
}

export interface HistorySearchResult {
  entry: HistoryEntry;
  score: number;
}

export async function searchHistory(query: string, limit?: number): Promise<HistorySearchResult[]> {
  const cappedLimit = limit ?? 10;
  let nativeResults: HistorySearchResult[] = [];
  try {
    nativeResults = await invoke<HistorySearchResult[]>('search_history', { query, limit: cappedLimit });
  } catch {
    nativeResults = [];
  }
  if (nativeResults.length > 0) {
    return nativeResults;
  }
  return readFallbackHistory()
    .map((entry) => ({ entry, score: scoreFallbackHistory(query, entry) }))
    .filter((result) => result.score > 0)
    .sort((a, b) => b.score - a.score || b.entry.last_visited - a.entry.last_visited)
    .slice(0, cappedLimit);
}

export async function getRecentHistory(limit?: number): Promise<HistoryEntry[]> {
  const cappedLimit = limit ?? 20;
  let nativeEntries: HistoryEntry[] = [];
  try {
    nativeEntries = await invoke<HistoryEntry[]>('get_recent_history', { limit: cappedLimit });
  } catch {
    nativeEntries = [];
  }
  if (nativeEntries.length > 0) {
    return nativeEntries;
  }
  return getFallbackRecentHistory(cappedLimit);
}

export async function deleteHistoryEntry(path: string): Promise<boolean> {
  const fallbackDeleted = deleteFallbackHistoryEntry(path);
  try {
    return await invoke<boolean>('delete_history_entry', { path });
  } catch {
    return fallbackDeleted;
  }
}

export async function clearHistory(): Promise<void> {
  clearFallbackHistory();
  try {
    await invoke<void>('clear_history');
  } catch {
    // Keep fallback history cleared even if native storage is unavailable.
  }
}

// ── Automation ──

export type AutomationAction =
  | 'open_url'
  | 'back'
  | 'forward'
  | 'reload'
  | 'home'
  | 'settings'
  | 'respond_nip07_prompt'
  | 'shutdown';

export interface AutomationCommandEvent {
  action: AutomationAction;
  url?: string | null;
  requestId?: string | null;
  decision?: 'deny' | 'allowSession' | 'allowAlways' | 'blockSite' | null;
}

export interface AutomationUiState {
  shellReady: boolean;
  currentView: string;
  currentUrl: string;
  addressValue: string;
  canGoBack: boolean;
  canGoForward: boolean;
  showDropdown: boolean;
  childWebviewReady: boolean;
  childPageLoadState: string;
  childPageLoadUrl: string;
  childDocumentTitle: string;
  childBodyText: string;
  childMediaSummary: string;
  childLastError: string;
  historyIndex: number;
  historyLength: number;
  windowInnerHeight: number;
  windowOuterHeight: number;
  toolbarHeight: number;
  childBoundsTop: number;
  childBoundsHeight: number;
  childViewportWidth: number;
  childViewportHeight: number;
  pendingNip07PromptRequestId: string;
  pendingNip07PromptOrigin: string;
  pendingNip07PromptMethod: string;
}

export interface AutomationState extends AutomationUiState {
  enabled: boolean;
  port: number | null;
}

export async function automationUpdateState(snapshot: AutomationUiState): Promise<void> {
  return invoke<void>('automation_update_state', { snapshot });
}

export async function automationGetState(): Promise<AutomationState> {
  return invoke<AutomationState>('automation_get_state');
}

export async function automationShutdown(): Promise<void> {
  return invoke<void>('automation_shutdown');
}

// ── Autostart ──

export async function isAutostartEnabled(): Promise<boolean> {
  try {
    const { isEnabled } = await import('@tauri-apps/plugin-autostart');
    return await isEnabled();
  } catch {
    return false;
  }
}

export async function toggleAutostart(enabled: boolean): Promise<boolean> {
  try {
    if (enabled) {
      const { enable } = await import('@tauri-apps/plugin-autostart');
      await enable();
    } else {
      const { disable } = await import('@tauri-apps/plugin-autostart');
      await disable();
    }
    return true;
  } catch {
    return false;
  }
}

// ── Events ──

export interface WebviewLocationEvent {
  label: string;
  url: string;
  source?: string;
}

export interface WebviewPageLoadEvent {
  label: string;
  url: string;
  event: string;
}

export interface WebviewDiagnosticEvent {
  label: string;
  url?: string | null;
  source?: string | null;
  title?: string | null;
  readyState?: string | null;
  bodyText?: string | null;
  mediaSummary?: string | null;
  viewportWidth?: number | null;
  viewportHeight?: number | null;
  manifestAppId?: string | null;
  manifestUrl?: string | null;
  manifestName?: string | null;
  manifestIconUrl?: string | null;
  error?: string | null;
}

export function onChildWebviewLocation(
  callback: (event: WebviewLocationEvent) => void,
): Promise<UnlistenFn> {
  return listen<WebviewLocationEvent>('child-webview-location', (event) => {
    callback(event.payload);
  });
}

export function onChildWebviewPageLoad(
  callback: (event: WebviewPageLoadEvent) => void,
): Promise<UnlistenFn> {
  return listen<WebviewPageLoadEvent>('child-webview-page-load', (event) => {
    callback(event.payload);
  });
}

export function onChildWebviewDiagnostic(
  callback: (event: WebviewDiagnosticEvent) => void,
): Promise<UnlistenFn> {
  return listen<WebviewDiagnosticEvent>('child-webview-diagnostic', (event) => {
    callback(event.payload);
  });
}

export function onAutomationCommand(
  callback: (event: AutomationCommandEvent) => void,
): Promise<UnlistenFn> {
  return listen<AutomationCommandEvent>('automation-command', (event) => {
    callback(event.payload);
  });
}
