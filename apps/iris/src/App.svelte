<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { LogicalPosition } from '@tauri-apps/api/dpi';
  import { Menu } from '@tauri-apps/api/menu';
  import {
    automationUpdateState,
    automationShutdown,
    createNip07Webview,
    createHtreeWebview,
    deepLinkFrontendReady,
    closeWebview,
    exportNip07AccountSecret,
    generateNip07Account,
    getHtreeServerUrl,
    installSitePwa,
    listNip07Accounts,
    loginNip07Account,
    navigateWebview,
    onAutomationCommand,
    webviewHistory,
    reloadWebview,
    removeNip07Account,
    respondNip07PermissionPrompt,
    setActiveNip07Account,
    setWebviewBounds,
    takeNip07PermissionPrompt,
    onChildWebviewDiagnostic,
    onChildWebviewLocation,
    onChildWebviewPageLoad,
    recordHistoryVisit,
    searchHistory,
    getRecentHistory,
    deleteHistoryEntry,
    type AutomationCommandEvent,
    type WebviewDiagnosticEvent,
    type WebviewLocationEvent,
    type WebviewPageLoadEvent,
    type HistoryEntry,
    type Nip07AccountSummary,
    type Nip07AccountsSummary,
    type Nip07PermissionPrompt,
  } from './lib/tauri';
  import { animalName } from './lib/animalName';
  import { bookmarkSavedName, isBuiltInIrisApp, matchesPwaIdentity } from './lib/apps';
  import { ownerProfileUrl } from './lib/addressIdentity';
  import { appsStore } from './stores/apps';
  import AddressOwnerPill from './components/AddressOwnerPill.svelte';
  import HistoryEntryIcon from './components/HistoryEntryIcon.svelte';
  import AppLauncher from './components/AppLauncher.svelte';
  import LoadingSpinner from './components/LoadingSpinner.svelte';
  import Nip07PermissionBar from './components/Nip07PermissionBar.svelte';
  import Settings from './components/Settings.svelte';
  import { minidenticon } from 'minidenticons';
  import {
    clearClipboardIfUnchanged,
    sensitiveClipboardClearDelayMs,
  } from './lib/sensitiveClipboard';

  type View = 'launcher' | 'settings' | 'webview';
  type SettingsTabId = 'app' | 'privacy' | 'users' | 'network' | 'about';
  type NavigateOptions = {
    pushHistory?: boolean;
    preferPlainLoopbackHost?: boolean;
  };
  type ResolvedTreeRoot = {
    hash?: string | null;
    cid?: string | null;
  };
  type DetectedPwa = {
    sourceAppId?: string;
    sourceUrl: string;
    manifestUrl: string;
    name?: string;
    iconUrl?: string;
  };

  const CHILD_LABEL = 'content';
  const TOOLBAR_BASE_HEIGHT = 48;
  const COMPACT_TOOLBAR_BREAKPOINT = 720;
  const DESKTOP_TRAFFIC_LIGHTS_PADDING = 88;
  const MOBILE_CHILD_WEBVIEWS_UNSUPPORTED = 'Mobile child webviews are not supported yet';
  const BLANK_SUGGESTED_TREE_RECOVERY_DELAY_MS = 1500;
  const BUILT_IN_TREE_ROOT_REFRESH_TIMEOUT_MS = 5000;
  const HTREE_LOAD_STALL_RECOVERY_DELAY_MS = 8000;
  const NIP07_PERMISSION_POLL_INTERVAL_MS = 350;
  const VISUAL_VIEWPORT_KEYBOARD_THRESHOLD_PX = 96;
  const MACOS_FUNCTION_KEY_GLYPHS = /[\uF700-\uF8FF]/g;
  const MACOS_FUNCTION_KEY_GLYPHS_SINGLE = /[\uF700-\uF8FF]/;
  const LEGACY_MACOS_ARROW_KEY_CODES = new Set([63232, 63233, 63234, 63235]);
  const RECOVERABLE_TREE_BODY_TEXTS = new Set(['Not found', 'Resolution timeout']);
  const ACCOUNT_SECRET_CLIPBOARD_CLEAR_DELAY_MS = sensitiveClipboardClearDelayMs();
  const PRIVATE_USE_ARROW_KEYS = {
    '\uF700': 'ArrowUp',
    '\uF701': 'ArrowDown',
    '\uF702': 'ArrowLeft',
    '\uF703': 'ArrowRight',
  } as const;
  const g = globalThis as typeof globalThis & {
    __irisChildReady?: boolean;
    __IRIS_BROWSER_TAURI_MOCK__?: boolean;
  };

  function isSettingsTabId(value: string): value is SettingsTabId {
    return value === 'app' || value === 'privacy' || value === 'users' || value === 'network' || value === 'about';
  }

  function parseSettingsRouteFromHash(hash: string): SettingsTabId | null | undefined {
    const path = (hash.startsWith('#') ? hash.slice(1) : hash).split('?')[0] ?? '';
    if (!path) return undefined;
    if (path === '/settings' || path === 'settings') return null;

    const parts = path.split('/').filter(Boolean);
    if (parts[0] !== 'settings') return undefined;
    return parts[1] && isSettingsTabId(parts[1]) ? parts[1] : undefined;
  }

  function updateShellHash(path: string | null) {
    if (typeof window === 'undefined') return;
    const url = new URL(window.location.href);
    const nextHash = path ? `#${path}` : '';
    if (url.hash === nextHash) return;
    url.hash = path ?? '';
    window.history.replaceState(window.history.state, '', url);
  }

  let addressValue = $state('');
  let currentUrl = $state('');              // full URL for editing
  let isAddressFocused = $state(false);
  let addressInputEl: HTMLInputElement | null = $state(null);
  let currentView: View = $state('launcher');
  let settingsTab = $state<SettingsTabId | null>(null);

  // Autocomplete dropdown
  let showDropdown = $state(false);
  let dropdownItems: HistoryEntry[] = $state([]);
  let selectedIndex = $state(-1);
  let debounceTimer: ReturnType<typeof setTimeout> | null = null;
  let blurTimer: ReturnType<typeof setTimeout> | null = null;
  let blankSuggestedTreeRecoveryTimer: ReturnType<typeof setTimeout> | null = null;
  let childLoadStallRecoveryTimer: ReturnType<typeof setTimeout> | null = null;
  let boundsRaf: number | null = null;
  let automationSyncRaf: number | null = null;
  let addressBarEl: HTMLDivElement | null = $state(null);
  let dropdownEl: HTMLDivElement | null = $state(null);
  let safeAreaTopInsetEl: HTMLDivElement | null = $state(null);
  let toolbarHeight = $state(TOOLBAR_BASE_HEIGHT);
  let isCompactToolbar = $state(
    typeof window !== 'undefined' && window.innerWidth < COMPACT_TOOLBAR_BREAKPOINT
  );
  let keyboardInsetBottom = $state(0);
  let showMobileMenu = $state(false);
  let mobileMenuEl: HTMLDivElement | null = $state(null);
  let accountMenuEl: HTMLDivElement | null = $state(null);
  let accountButtonEl: HTMLButtonElement | null = $state(null);
  let showAccountMenu = $state(false);
  let savedAccounts: Nip07AccountSummary[] = $state([]);
  let activeAccountPubkey: string | null = $state(null);
  let accountSecretDraft = $state('');
  let showAddAccountSecret = $state(false);
  let pendingAccountRemovalPubkey: string | null = $state(null);
  let accountError = $state('');
  let accountBusy = $state(false);
  let permissionPromptQueue: Nip07PermissionPrompt[] = $state([]);
  let permissionPromptBusy = $state(false);
  let permissionPromptError = $state('');
  let permissionPromptPollTimer: ReturnType<typeof setInterval> | null = null;
  let nativeHistoryMenuBusy = $state(false);
  let nativeHistoryMenuRequested = $state(false);
  let nativeHistoryMenuFallback = $state(false);
  let nativeAccountMenuFallback = $state(false);
  let accountSecretClipboardClearTimer: ReturnType<typeof setTimeout> | null = null;

  // Shell-level navigation history
  let historyStack: string[] = $state([]);  // URLs visited
  let historyIndex = $state(-1);            // -1 = launcher

  // Intra-webview navigation tracking
  let webviewNavDepth = $state(0);          // user navigations within current webview
  let webviewFwdAvail = $state(0);          // forward steps available within webview
  let ignoreLocationEvents = 0;             // skip location events we caused
  const treeRootRecoveryAttempts = new Map<string, number>();
  let childPageLoadState = $state('idle');
  let childPageLoadUrl = $state('');
  let childDocumentTitle = $state('');
  let childBodyText = $state('');
  let childMediaSummary = $state('');
  let childViewportWidth = $state(0);
  let childViewportHeight = $state(0);
  let childLastError = $state('');
  let detectedPwa: DetectedPwa | null = $state(null);
  let isInstallingPwa = $state(false);
  let childWebviewReady = $state(!!g.__irisChildReady);
  let childUsesPlainLoopbackTransport = $state(false);
  const plainLoopbackFallbackScopes = new Set<string>();

  let canGoBack = $derived(
    (currentView === 'webview' && webviewNavDepth > 0) ||
    historyIndex >= 0 ||
    currentView !== 'launcher'
  );
  let canGoForward = $derived(
    (currentView === 'webview' && webviewFwdAvail > 0) ||
    historyIndex < historyStack.length - 1
  );
  let isChildLoading = $derived(
    currentView === 'webview' &&
    !!currentUrl &&
    childPageLoadState !== 'finished' &&
    !childLastError
  );
  let currentPwaBookmark = $derived(
    detectedPwa
      ? $appsStore.find((app) => matchesPwaIdentity(app, detectedPwa)) ?? null
      : null,
  );
  let canInstallCurrentPwa = $derived(
    currentView === 'webview' &&
    !!currentUrl &&
    currentUrl.startsWith('https://') &&
    !!detectedPwa?.manifestUrl,
  );
  let currentPermissionPrompt = $derived(permissionPromptQueue[0] ?? null);
  let showShellHistoryDropdown = $derived(
    showDropdown &&
    dropdownItems.length > 0 &&
    (!canTryNativeHistoryMenu() || nativeHistoryMenuFallback)
  );
  let showShellAccountMenu = $derived(
    showAccountMenu &&
    (!canTryNativeAccountMenu() || nativeAccountMenuFallback)
  );
  let currentAccount = $derived.by(() => {
    if (savedAccounts.length === 0) return null;
    return savedAccounts.find((account) => account.pubkey === activeAccountPubkey) ?? savedAccounts[0] ?? null;
  });
  let sortedAccounts = $derived([...savedAccounts].sort((a, b) => a.addedAt - b.addedAt));
  let currentAccountName = $derived(currentAccount ? animalName(currentAccount.pubkey) : 'Nostr user');
  let accountAvatarUrl = $derived.by(() => {
    if (!currentAccount?.pubkey) return '';
    return `data:image/svg+xml;utf8,${encodeURIComponent(minidenticon(currentAccount.pubkey, 40, 40))}`;
  });
  let accountMenuStyle = $derived.by(() => {
    if (isCompactToolbar) {
      return `right: 12px; bottom: calc(env(safe-area-inset-bottom, 0px) + ${toolbarHeight}px + ${keyboardInsetBottom}px + 8px);`;
    }
    return `right: 12px; top: calc(env(safe-area-inset-top, 0px) + ${toolbarHeight}px + 8px);`;
  });

  function urlToDisplay(url: string): string {
    try {
      return url.replace(/^(https?|htree):\/\//, '').replace(/\/$/, '');
    } catch {
      return url;
    }
  }

  function browserIsolationScope(url: string): string {
    const htree = parseHtreeUrl(url);
    if (htree?.nhash) {
      return `htree://${htree.nhash}`;
    }
    if (htree?.treename) {
      return `htree://${htree.host}/${encodeURIComponent(htree.treename)}/`;
    }

    try {
      return new URL(url).origin;
    } catch {
      return url;
    }
  }

  function shouldRecreateBrowserForUrl(nextUrl: string, previousUrl: string): boolean {
    if (!previousUrl) return true;
    return browserIsolationScope(nextUrl) !== browserIsolationScope(previousUrl);
  }

  function displayToUrl(value: string): string {
    const trimmed = value.trim();
    if (!trimmed) return '';
    if (trimmed.startsWith('http://') || trimmed.startsWith('https://')) return trimmed;
    if (trimmed.startsWith('htree://')) return trimmed;
    if (trimmed === 'self' || trimmed.startsWith('self/')) return `htree://${trimmed}`;
    if (trimmed.startsWith('nhash1') || trimmed.startsWith('npub1')) return `htree://${trimmed}`;
    if (trimmed.includes('.') && !trimmed.includes(' ')) return `https://${trimmed}`;
    return `https://${trimmed}`;
  }

  function sanitizeAddressText(value: string): string {
    return value.replace(MACOS_FUNCTION_KEY_GLYPHS, '');
  }

  function normalizedAddressKey(event: KeyboardEvent): string {
    const privateUseKey = PRIVATE_USE_ARROW_KEYS[event.key as keyof typeof PRIVATE_USE_ARROW_KEYS];
    if (privateUseKey) return privateUseKey;
    switch (event.keyCode || event.which) {
      case 37: return 'ArrowLeft';
      case 38: return 'ArrowUp';
      case 39: return 'ArrowRight';
      case 40: return 'ArrowDown';
      case 63232: return 'ArrowUp';
      case 63233: return 'ArrowDown';
      case 63234: return 'ArrowLeft';
      case 63235: return 'ArrowRight';
      default: return event.key;
    }
  }

  function isLegacyMacosArrowKeyCode(event: KeyboardEvent): boolean {
    return LEGACY_MACOS_ARROW_KEY_CODES.has(event.keyCode || event.which);
  }

  function isMacosFunctionArrowEvent(event: KeyboardEvent): boolean {
    return MACOS_FUNCTION_KEY_GLYPHS_SINGLE.test(event.key) || isLegacyMacosArrowKeyCode(event);
  }

  function isEscapeKey(event: KeyboardEvent): boolean {
    return event.key === 'Escape'
      || event.key === 'Esc'
      || event.code === 'Escape'
      || event.keyCode === 27
      || event.which === 27;
  }

  function moveAddressCaret(direction: -1 | 1) {
    const input = addressInputEl;
    if (!input) return;
    const start = input.selectionStart ?? 0;
    const end = input.selectionEnd ?? start;
    const hasSelection = start !== end;
    const boundary = direction < 0 ? Math.min(start, end) : Math.max(start, end);
    const next = hasSelection
      ? boundary
      : Math.max(0, Math.min(input.value.length, boundary + direction));
    input.setSelectionRange(next, next);
  }

  function sanitizeAddressFieldValue() {
    const input = addressInputEl;
    const rawValue = input?.value ?? addressValue;
    const sanitizedValue = sanitizeAddressText(rawValue);

    if (rawValue === sanitizedValue) {
      if (addressValue !== rawValue) {
        addressValue = rawValue;
      }
      return sanitizedValue;
    }

    const selectionStart = input?.selectionStart ?? rawValue.length;
    const selectionEnd = input?.selectionEnd ?? rawValue.length;
    const removedBeforeStart = (rawValue.slice(0, selectionStart).match(MACOS_FUNCTION_KEY_GLYPHS) ?? []).length;
    const removedBeforeEnd = (rawValue.slice(0, selectionEnd).match(MACOS_FUNCTION_KEY_GLYPHS) ?? []).length;

    addressValue = sanitizedValue;
    if (input) {
      input.value = sanitizedValue;
    }

    requestAnimationFrame(() => {
      if (!addressInputEl) return;
      const nextStart = Math.max(0, selectionStart - removedBeforeStart);
      const nextEnd = Math.max(0, selectionEnd - removedBeforeEnd);
      addressInputEl.setSelectionRange(nextStart, nextEnd);
    });

    return sanitizedValue;
  }

  function setChildWebviewReady(ready: boolean) {
    g.__irisChildReady = ready;
    childWebviewReady = ready;
  }

  function formatWebviewError(error: unknown): string {
    if (error instanceof Error && error.message) {
      return error.message;
    }
    if (typeof error === 'string') {
      return error;
    }
    if (error && typeof error === 'object' && 'message' in error) {
      const message = (error as { message?: unknown }).message;
      if (typeof message === 'string' && message) {
        return message;
      }
    }
    return 'Failed to open page.';
  }

  async function refreshTreeRoot(npub: string, treename: string): Promise<ResolvedTreeRoot | null> {
    try {
      const serverUrl = await getHtreeServerUrl();
      const refreshUrl =
        `${serverUrl}/api/resolve/${encodeURIComponent(npub)}/${encodeURIComponent(treename)}?refresh=1`;
      const controller = new AbortController();
      const timeout = setTimeout(() => controller.abort(), BUILT_IN_TREE_ROOT_REFRESH_TIMEOUT_MS);
      try {
        const response = await fetch(refreshUrl, {
          cache: 'no-store',
          signal: controller.signal,
        });
        const payload = await response.json().catch(() => null) as ResolvedTreeRoot & { error?: string } | null;
        if (response.ok && !payload?.error) {
          return payload;
        }
      } catch (error) {
        console.warn('[Iris] failed to refresh tree root:', npub, treename, formatWebviewError(error));
      } finally {
        clearTimeout(timeout);
      }
    } catch (error) {
      console.warn('[Iris] failed to refresh tree root:', npub, treename, formatWebviewError(error));
    }

    return null;
  }

  function isUnsupportedChildWebviewError(message: string): boolean {
    return message.includes(MOBILE_CHILD_WEBVIEWS_UNSUPPORTED);
  }

  function shouldRetryNavigateAfterCreateFailure(message: string): boolean {
    return !isUnsupportedChildWebviewError(message) && !message.includes('missing required key origin');
  }

  function setChildWebviewError(error: unknown) {
    childLastError = formatWebviewError(error);
    childPageLoadState = 'failed';
    setChildWebviewReady(false);
    scheduleAutomationStateSync();
  }

  function webviewErrorHeadline(error: string): string {
    return isUnsupportedChildWebviewError(error)
      ? 'Embedded browsing is not available on this device yet'
      : 'Could not open this page';
  }

  function webviewErrorDetail(error: string): string {
    return isUnsupportedChildWebviewError(error)
      ? 'Iris uses child webviews for in-app pages, and the current mobile runtime does not provide them yet.'
      : error;
  }

  function isFatalChildDiagnosticError(error: string, source?: string | null): boolean {
    const trimmed = error.trim();
    if (!trimmed) return false;

    const lower = trimmed.toLowerCase();
    if (
      lower.includes('notification.is_permission_granted not allowed') ||
      lower.includes("can't find variable: rtcpeerconnection") ||
      trimmed.includes('console:warn') ||
      trimmed.includes('worker:init:') ||
      trimmed.includes('worker:ready') ||
      trimmed.includes('media:setup:') ||
      trimmed.includes('prefix:')
    ) {
      return false;
    }

    if (source === 'resource-error') {
      return lower.startsWith('script failed to load') || lower.startsWith('link failed to load');
    }

    return trimmed.includes('console:error') ||
      trimmed.includes('window:error') ||
      trimmed.includes('window:unhandledrejection') ||
      lower.includes('failed to load') ||
      lower.includes('invalid session token') ||
      lower.includes('protocol bridge request failed') ||
      lower.includes('could not open');
  }

  function syncToolbarMode() {
    const nextIsCompactToolbar = window.innerWidth < COMPACT_TOOLBAR_BREAKPOINT;
    if (isCompactToolbar !== nextIsCompactToolbar) {
      isCompactToolbar = nextIsCompactToolbar;
      showMobileMenu = false;
    }
    syncKeyboardInsetBottom();
  }

  function computeKeyboardInsetBottom(): number {
    if (!isCompactToolbar || typeof window === 'undefined') {
      return 0;
    }

    const viewport = window.visualViewport;
    if (!viewport) {
      return 0;
    }

    const overlap = window.innerHeight - (viewport.height + viewport.offsetTop);
    if (overlap < VISUAL_VIEWPORT_KEYBOARD_THRESHOLD_PX) {
      return 0;
    }

    return Math.max(0, Math.round(overlap));
  }

  function syncKeyboardInsetBottom() {
    const nextKeyboardInsetBottom = computeKeyboardInsetBottom();
    if (keyboardInsetBottom === nextKeyboardInsetBottom) {
      return;
    }
    keyboardInsetBottom = nextKeyboardInsetBottom;
    scheduleWebviewBoundsUpdate();
  }

  function handleAddressBeforeInput(event: InputEvent) {
    if (!event.data) return;
    if (!MACOS_FUNCTION_KEY_GLYPHS_SINGLE.test(event.data)) return;
    event.preventDefault();
  }

  function handleAddressKeyPress(event: KeyboardEvent) {
    if (!isMacosFunctionArrowEvent(event)) return;
    event.preventDefault();
    event.stopPropagation();
  }

  function handleAddressInput() {
    const sanitizedValue = sanitizeAddressFieldValue();
    if (!isAddressFocused) isAddressFocused = true;
    showDropdown = true;
    nativeHistoryMenuRequested = false;
    nativeHistoryMenuFallback = false;
    debouncedSearch(sanitizedValue);
  }

  function handleAddressKeyUp(event: KeyboardEvent) {
    if (!isMacosFunctionArrowEvent(event)) return;
    sanitizeAddressFieldValue();
  }

  function handleAddressChromeClick(event: MouseEvent) {
    const target = event.target;
    if (!(target instanceof Element)) return;
    if (target.closest('button')) return;
    if (isAddressFocused) return;
    addressInputEl?.focus();
  }

  function handleAddressChromeKeyDown(event: KeyboardEvent) {
    if (isAddressFocused) return;
    const target = event.target;
    if (!(target instanceof Element)) return;
    if (target.closest('button') || target.closest('input')) return;
    if (event.key !== 'Enter' && event.key !== ' ') return;
    event.preventDefault();
    addressInputEl?.focus();
  }

  function handleAddressKeyDown(event: KeyboardEvent) {
    const key = normalizedAddressKey(event);
    const isMacosFunctionArrow = isMacosFunctionArrowEvent(event);

    if (key === 'Enter') {
      handleAddressSubmit();
      return;
    }

    if (isEscapeKey(event)) {
      event.preventDefault();
      event.stopPropagation();
      dismissDropdown();
      return;
    }

    if (key === 'ArrowDown' && showDropdown && dropdownItems.length > 0 && canTryNativeHistoryMenu()) {
      nativeHistoryMenuRequested = false;
      void tryShowNativeHistoryMenu();
    }

    if (key === 'ArrowDown' && showDropdown && dropdownItems.length > 0) {
      event.preventDefault();
      selectedIndex = selectedIndex < 0 ? 0 : (selectedIndex + 1) % dropdownItems.length;
      return;
    }

    if (key === 'ArrowUp' && showDropdown && dropdownItems.length > 0) {
      event.preventDefault();
      selectedIndex = selectedIndex <= 0 ? dropdownItems.length - 1 : selectedIndex - 1;
      return;
    }

    if (!isMacosFunctionArrow) return;

    if (key === 'ArrowLeft') {
      event.preventDefault();
      event.stopPropagation();
      moveAddressCaret(-1);
      return;
    }

    if (key === 'ArrowRight') {
      event.preventDefault();
      event.stopPropagation();
      moveAddressCaret(1);
      return;
    }

    if (key === 'ArrowUp' || key === 'ArrowDown') {
      event.preventDefault();
      event.stopPropagation();
    }
  }

  function handleLocationChange(event: WebviewLocationEvent) {
    if (event.label !== CHILD_LABEL) return;
    const previousUrl = currentUrl;
    const nextUrl = normalizeChildReportedUrl(event.url, previousUrl);
    const requiresRecreation = (
      currentView === 'webview' &&
      !!previousUrl &&
      shouldRecreateBrowserForUrl(nextUrl, previousUrl)
    );

    // Native navigation callbacks can arrive before the runtime confirms a
    // top-level load. Ignore cross-scope signals here and let page-load events
    // confirm them so subframes do not hijack the shell URL.
    if (event.source === 'navigation' && requiresRecreation) {
      return;
    }

    currentUrl = nextUrl;
    if (!isAddressFocused) {
      addressValue = urlToDisplay(nextUrl);
    }
    if (ignoreLocationEvents > 0) {
      ignoreLocationEvents--;
      return;
    }
    if (nextUrl === previousUrl) {
      return;
    }
    if (requiresRecreation) {
      currentUrl = previousUrl;
      void navigate(nextUrl, { pushHistory: false });
      return;
    }
    if (isRecordableUrl(nextUrl)) {
      recordHistoryVisit(buildHistoryEntry(nextUrl))
        .catch((e) => console.warn('[Iris] record history failed:', e));
    }
    // User navigated within webview (clicked a link, etc.)
    if (currentView === 'webview') {
      webviewNavDepth++;
      webviewFwdAvail = 0;
    }
  }

  function decodeUrlComponent(value: string): string {
    try {
      return decodeURIComponent(value);
    } catch {
      return value;
    }
  }

  function decodePath(rawPath: string): string {
    const segments = rawPath
      .split('/')
      .filter(Boolean)
      .map(decodeUrlComponent);
    return segments.length > 0 ? `/${segments.join('/')}` : '/';
  }

  /** Parse htree://{self|npub}/treename/path, legacy htree://npub.treename/path, or htree://nhash/path. */
  function parseHtreeUrl(url: string): {
    host: string;
    nhash?: string;
    npub?: string;
    treename?: string;
    path: string;
    query?: string;
    fragment?: string;
  } | null {
    if (!url.startsWith('htree://')) return null;
    const rest = url.slice('htree://'.length);
    const fragmentIndex = rest.indexOf('#');
    const fragment = fragmentIndex === -1 ? undefined : rest.slice(fragmentIndex + 1);
    const withoutFragment = fragmentIndex === -1 ? rest : rest.slice(0, fragmentIndex);
    const separatorMatch = withoutFragment.match(/[/?]/);
    const separatorIndex = separatorMatch?.index ?? -1;
    const host = separatorIndex === -1 ? withoutFragment : withoutFragment.slice(0, separatorIndex);
    const pathAndQuery = separatorIndex === -1 ? '' : withoutFragment.slice(separatorIndex);
    const queryIndex = pathAndQuery.indexOf('?');
    const rawPath = queryIndex === -1 ? pathAndQuery : pathAndQuery.slice(0, queryIndex);
    const query = queryIndex === -1 ? undefined : pathAndQuery.slice(queryIndex + 1);

    if (host.startsWith('npub1')) {
      const dotIndex = host.indexOf('.');
      if (dotIndex !== -1) {
        const npub = host.slice(0, dotIndex);
        const treename = decodeUrlComponent(host.slice(dotIndex + 1));
        return { host, npub, treename, path: decodePath(rawPath), query, fragment };
      }

      const pathSegments = rawPath.split('/').filter(Boolean);
      const treename = pathSegments[0] ? decodeUrlComponent(pathSegments[0]) : '';
      const path = pathSegments.length > 1 ? `/${pathSegments.slice(1).map(decodeUrlComponent).join('/')}` : '/';
      return { host, npub: host, treename, path, query, fragment };
    } else if (host === 'self') {
      const pathSegments = rawPath.split('/').filter(Boolean);
      const treename = pathSegments[0] ? decodeUrlComponent(pathSegments[0]) : '';
      const path = pathSegments.length > 1 ? `/${pathSegments.slice(1).map(decodeUrlComponent).join('/')}` : '/';
      return { host, treename, path, query, fragment };
    } else if (host.startsWith('nhash1')) {
      return { host, nhash: host, path: decodePath(rawPath), query, fragment };
    }
    return null;
  }

  function htreePathLabel(htree: {
    treename?: string;
    path: string;
    query?: string;
    fragment?: string;
  }): string {
    let label = htree.treename ? `/${htree.treename}` : '';
    if (htree.path && htree.path !== '/') {
      label += htree.path.startsWith('/') ? htree.path : `/${htree.path}`;
    }
    if (!label) {
      label = '/';
    }
    if (htree.query) {
      label += `?${htree.query}`;
    }
    if (htree.fragment) {
      label += `#${htree.fragment}`;
    }
    return label;
  }

  function normalizeChildReportedUrl(nextUrl: string, previousUrl: string): string {
    if (!previousUrl) return nextUrl;
    const next = parseHtreeUrl(nextUrl);
    const previous = parseHtreeUrl(previousUrl);
    if (!next || !previous) return nextUrl;
    if (next.host !== previous.host) return nextUrl;
    if (next.nhash !== previous.nhash) return nextUrl;
    if (next.npub !== previous.npub) return nextUrl;
    if (next.treename !== previous.treename) return nextUrl;
    if ((next.query ?? '') !== (previous.query ?? '')) return nextUrl;
    if ((next.fragment ?? '') !== (previous.fragment ?? '')) return nextUrl;
    if (next.path === '/' && previous.path === '/index.html') {
      return previousUrl;
    }
    return nextUrl;
  }

  function preferredBlurredNhashTitle(url: string, nhash: string): string | null {
    const title = childDocumentTitle.trim();
    if (!title) return null;
    const displayUrl = urlToDisplay(url);
    if (title === url || title === displayUrl || title === nhash) {
      return null;
    }
    return title;
  }

  let blurredOwnerSummary = $derived.by(() => {
    if (isAddressFocused) return null;
    const htree = parseHtreeUrl(currentUrl);
    if (!htree?.npub) return null;
    return {
      host: htree.npub,
      treeName: htree.treename ?? '',
    };
  });

  let blurredNhashTitle = $derived.by(() => {
    if (isAddressFocused) return '';
    const htree = parseHtreeUrl(currentUrl);
    if (!htree?.nhash) return '';
    return preferredBlurredNhashTitle(currentUrl, htree.nhash) ?? '';
  });

  function historyOwnerSummary(entry: HistoryEntry) {
    const htree = parseHtreeUrl(entry.path);
    if (!htree?.npub) return null;
    return {
      host: htree.npub,
      displayLabel: bookmarkSavedName(entry.path, entry.label),
    };
  }

  function historyWebLabel(entry: HistoryEntry): string {
    return bookmarkSavedName(entry.path, entry.label);
  }

  function historyMenuLabel(entry: HistoryEntry): string {
    const owner = historyOwnerSummary(entry);
    if (owner) {
      return `${owner.displayLabel} - ${owner.host}`;
    }

    const baseLabel = historyWebLabel(entry);
    try {
      const host = new URL(entry.path).host;
      if (!host || baseLabel === host) {
        return baseLabel;
      }
      return `${baseLabel} - ${host}`;
    } catch {
      return baseLabel;
    }
  }

  function isRecordableUrl(url: string): boolean {
    return url.startsWith('http://') || url.startsWith('https://') || url.startsWith('htree://');
  }

  function buildHistoryEntry(url: string, preferredLabel?: string) {
    const htree = parseHtreeUrl(url);
    return {
      path: url,
      label: bookmarkSavedName(url, preferredLabel),
      entry_type: htree ? 'tree' : 'web',
      npub: htree?.npub ?? null,
      tree_name: htree?.treename ?? null,
    };
  }

  function clearBlankSuggestedTreeRecoveryTimer() {
    if (blankSuggestedTreeRecoveryTimer) {
      clearTimeout(blankSuggestedTreeRecoveryTimer);
      blankSuggestedTreeRecoveryTimer = null;
    }
  }

  function clearChildLoadStallRecoveryTimer() {
    if (childLoadStallRecoveryTimer) {
      clearTimeout(childLoadStallRecoveryTimer);
      childLoadStallRecoveryTimer = null;
    }
  }

  function shouldRefreshBuiltInAppTreeRoot(url: string): boolean {
    const htree = parseHtreeUrl(url);
    return isBuiltInIrisApp(htree?.npub, htree?.treename);
  }

  function hasChildDiagnosticsSnapshot(): boolean {
    return !!childDocumentTitle.trim() ||
      !!childBodyText.trim() ||
      !!childMediaSummary.trim() ||
      !!childLastError.trim();
  }

  function shouldUsePlainLoopbackTransport(url: string, preferPlainLoopbackHost: boolean): boolean {
    return preferPlainLoopbackHost || plainLoopbackFallbackScopes.has(browserIsolationScope(url));
  }

  function scheduleHtreeLoadStallRecovery(url: string) {
    clearChildLoadStallRecoveryTimer();
    if (!parseHtreeUrl(url)) return;
    if (plainLoopbackFallbackScopes.has(browserIsolationScope(url))) return;

    const scheduledUrl = url;
    childLoadStallRecoveryTimer = setTimeout(() => {
      childLoadStallRecoveryTimer = null;
      if (
        currentView !== 'webview' ||
        currentUrl !== scheduledUrl ||
        childPageLoadState !== 'started' ||
        hasChildDiagnosticsSnapshot()
      ) {
        return;
      }
      void recoverHtreeWebview(scheduledUrl, {
        reason: 'stalled-start',
        preferPlainLoopbackHost: true,
      });
    }, HTREE_LOAD_STALL_RECOVERY_DELAY_MS);
  }

  function detectPwaFromDiagnostic(event: WebviewDiagnosticEvent): DetectedPwa | null {
    const sourceAppId = event.manifestAppId?.trim();
    const manifestUrl = event.manifestUrl?.trim();
    const sourceUrl = event.url?.trim() || currentUrl;
    if (!manifestUrl || !sourceUrl.startsWith('https://')) {
      return null;
    }
    const name = event.manifestName?.trim() || event.title?.trim() || childDocumentTitle.trim();
    const iconUrl = event.manifestIconUrl?.trim();
    return {
      sourceAppId: sourceAppId || undefined,
      sourceUrl,
      manifestUrl,
      name: name || undefined,
      iconUrl: iconUrl || undefined,
    };
  }

  function resetChildDiagnostics(loadState: string = 'idle', loadUrl: string = '') {
    clearBlankSuggestedTreeRecoveryTimer();
    clearChildLoadStallRecoveryTimer();
    childPageLoadState = loadState;
    childPageLoadUrl = loadUrl;
    childDocumentTitle = '';
    childBodyText = '';
    childMediaSummary = '';
    childViewportWidth = 0;
    childViewportHeight = 0;
    childLastError = '';
    detectedPwa = null;
    isInstallingPwa = false;
  }

  async function destroyChildWebview() {
    // Always try to close, regardless of tracked state
    try {
      await closeWebview(CHILD_LABEL);
    } catch {
      // Webview might not exist, that's fine
    }
    setChildWebviewReady(false);
    childUsesPlainLoopbackTransport = false;
    resetChildDiagnostics();
    scheduleAutomationStateSync();
  }

  function browserViewportInsets() {
    const mobileMenuHeight = showMobileMenu ? (mobileMenuEl?.offsetHeight ?? 0) : 0;
    const safeAreaTop = safeAreaTopInsetEl?.offsetHeight ?? 0;

    if (isCompactToolbar) {
      return {
        top: safeAreaTop,
        bottom: toolbarHeight + keyboardInsetBottom + (mobileMenuHeight > 0 ? mobileMenuHeight + 8 : 0),
      };
    }

    return {
      top: toolbarHeight,
      bottom: 0,
    };
  }

  /** Open a URL in the child webview. */
  async function navigate(url: string, options: NavigateOptions = {}) {
    showAccountMenu = false;
    updateShellHash(null);
    settingsTab = null;
    const {
      pushHistory = true,
      preferPlainLoopbackHost = false,
    } = options;
    const htree = parseHtreeUrl(url);
    const usePlainLoopbackTransport = htree
      ? shouldUsePlainLoopbackTransport(url, preferPlainLoopbackHost)
      : false;

    // Destroy existing child webview when switching origins or entering webview
    if (g.__irisChildReady) {
      if (
        currentView !== 'webview' ||
        shouldRecreateBrowserForUrl(url, currentUrl) ||
        (htree && usePlainLoopbackTransport)
      ) {
        await destroyChildWebview();
      }
    }

    ignoreLocationEvents++;
    webviewNavDepth = 0;
    webviewFwdAvail = 0;

    currentView = 'webview';
    currentUrl = url;
    resetChildDiagnostics('started', url);
    await tick();

    const x = 0;
    const { top, bottom } = browserViewportInsets();
    const y = top;
    const width = window.innerWidth;
    const height = Math.max(0, window.innerHeight - top - bottom);
    let builtInTreeRoot: ResolvedTreeRoot | null = null;

    if (htree?.npub && htree.treename && isBuiltInIrisApp(htree.npub, htree.treename)) {
      // Built-in apps are released independently of the shell. Refresh the
      // mutable root before navigation, but keep the previous cached root if
      // relays are flaky so the app can still load.
      builtInTreeRoot = await refreshTreeRoot(htree.npub, htree.treename);
    }

    if (!g.__irisChildReady) {
      try {
        if (htree) {
          await createHtreeWebview(
            CHILD_LABEL,
            {
              ...htree,
              cacheBust: builtInTreeRoot?.cid ?? builtInTreeRoot?.hash ?? undefined,
            },
            x,
            y,
            width,
            height,
            usePlainLoopbackTransport,
          );
          childUsesPlainLoopbackTransport = usePlainLoopbackTransport;
        } else {
          await createNip07Webview(CHILD_LABEL, url, x, y, width, height);
          childUsesPlainLoopbackTransport = false;
        }
        setChildWebviewReady(true);
        scheduleWebviewBoundsUpdate();
        scheduleAutomationStateSync();
      } catch (e) {
        const createError = formatWebviewError(e);
        if (!shouldRetryNavigateAfterCreateFailure(createError)) {
          console.warn('[Iris] create webview failed:', createError);
          setChildWebviewError(createError);
          return;
        }
        console.warn('[Iris] create webview failed, trying navigate:', createError);
        try {
          await navigateWebview(CHILD_LABEL, url);
          childUsesPlainLoopbackTransport = false;
          setChildWebviewReady(true);
          scheduleWebviewBoundsUpdate();
          scheduleAutomationStateSync();
        } catch (e2) {
          console.error('[Iris] navigate also failed:', e2);
          setChildWebviewError(e2);
          return;
        }
      }
    } else {
      await navigateWebview(CHILD_LABEL, url);
      childUsesPlainLoopbackTransport = false;
      scheduleWebviewBoundsUpdate();
    }

    scheduleHtreeLoadStallRecovery(url);

    if (pushHistory) {
      // Truncate any forward history, then push
      historyStack = [...historyStack.slice(0, historyIndex + 1), url];
      historyIndex = historyStack.length - 1;

      // Record visit for autocomplete
      const entry = buildHistoryEntry(url);
      recordHistoryVisit(entry)
        .catch((e) => console.warn('[Iris] record history failed:', e));
    }

    if (!isAddressFocused) {
      addressValue = urlToDisplay(url);
    }
  }

  async function goHome() {
    showMobileMenu = false;
    showAccountMenu = false;
    updateShellHash(null);
    await destroyChildWebview();
    currentView = 'launcher';
    settingsTab = null;
    currentUrl = '';
    addressValue = '';
    webviewNavDepth = 0;
    webviewFwdAvail = 0;
  }

  async function goSettings(tab: SettingsTabId | null = null, syncHash = true) {
    showMobileMenu = false;
    showAccountMenu = false;
    if (syncHash) {
      updateShellHash(tab ? `/settings/${tab}` : '/settings');
    }
    if (currentView === 'webview' || g.__irisChildReady) {
      await destroyChildWebview();
    }
    settingsTab = tab;
    currentView = 'settings';
    currentUrl = '';
    addressValue = '';
    webviewNavDepth = 0;
    webviewFwdAvail = 0;
  }

  let isFavorited = $derived(currentUrl ? $appsStore.some(a => a.url === currentUrl) : false);

  function applyAccountsSummary(summary: Nip07AccountsSummary) {
    savedAccounts = summary.accounts ?? [];
    activeAccountPubkey = summary.activePubkey ?? null;
  }

  function nextAccountPubkeyAfterMutation(summary: Nip07AccountsSummary): string | null {
    return summary.activePubkey
      ?? summary.accounts.find((account) => account.pubkey === activeAccountPubkey)?.pubkey
      ?? summary.accounts[0]?.pubkey
      ?? null;
  }

  async function refreshChildWebviewForAccountChange(
    previousPubkey: string | null,
    nextPubkey: string | null,
  ) {
    if (previousPubkey === nextPubkey) return;
    if (currentView !== 'webview' || !currentUrl) return;
    if (!childWebviewReady) {
      await navigate(currentUrl, { pushHistory: false });
      return;
    }
    await reloadWebview(CHILD_LABEL);
  }

  function accountDisplayName(account: Pick<Nip07AccountSummary, 'pubkey'> | null | undefined): string {
    return account?.pubkey ? animalName(account.pubkey) : 'Nostr user';
  }

  async function loadNip07Accounts() {
    try {
      applyAccountsSummary(await listNip07Accounts());
    } catch (error) {
      console.warn('[Iris] failed to load NIP-07 accounts:', error);
    }
  }

  function canTryNativeAccountMenu(): boolean {
    return currentView === 'webview' && typeof window !== 'undefined' && !g.__IRIS_BROWSER_TAURI_MOCK__;
  }

  function canTryNativeHistoryMenu(): boolean {
    return (
      currentView === 'webview' &&
      typeof window !== 'undefined' &&
      !!addressBarEl &&
      !g.__IRIS_BROWSER_TAURI_MOCK__
    );
  }

  function popupLogicalPositionForElement(
    element: HTMLElement,
    options: { offsetX?: number; offsetY?: number } = {},
  ): LogicalPosition {
    const { offsetX = 0, offsetY = 0 } = options;
    const rect = element.getBoundingClientRect();
    return new LogicalPosition(
      Math.round(rect.left + offsetX),
      Math.round(rect.bottom + offsetY),
    );
  }

  async function addAccountSecret(secret: string) {
    const trimmed = secret.trim();
    if (!trimmed || accountBusy) return;

    const previousPubkey = currentAccount?.pubkey ?? null;
    accountBusy = true;
    accountError = '';
    try {
      await loginNip07Account(trimmed);
      await loadNip07Accounts();
      accountSecretDraft = '';
      showAddAccountSecret = false;
      showAccountMenu = false;
      await clearClipboardIfUnchanged(trimmed);
      await refreshChildWebviewForAccountChange(previousPubkey, activeAccountPubkey);
    } catch (error) {
      accountError = formatWebviewError(error);
      throw error;
    } finally {
      accountBusy = false;
    }
  }

  async function promptForAccountSecretFromNativeMenu() {
    const secret = window.prompt('Paste nsec or hex secret', '');
    if (!secret?.trim()) return;
    await addAccountSecret(secret);
  }

  async function confirmRemoveCurrentAccountFromNativeMenu() {
    if (!activeAccountPubkey) return;
    if (!window.confirm(`Remove ${currentAccountName}?`)) return;
    await confirmRemoveAccount(activeAccountPubkey);
  }

  async function tryShowNativeAccountMenu(): Promise<boolean> {
    if (!canTryNativeAccountMenu() || !accountButtonEl) return false;

    try {
      const items: Array<Record<string, unknown>> = [
        {
          id: 'account-current',
          text: currentAccount ? currentAccountName : 'No Nostr user selected',
          enabled: false,
        },
      ];

      if (sortedAccounts.length > 0) {
        items.push({ item: 'Separator' });
        for (const account of sortedAccounts) {
          const isActive = account.pubkey === activeAccountPubkey;
          items.push({
            id: `account-${account.pubkey}`,
            text: isActive ? `Active: ${accountDisplayName(account)}` : accountDisplayName(account),
            enabled: !accountBusy && !isActive,
            action: () => {
              if (!isActive) {
                void switchToAccount(account);
              }
            },
          });
        }
      }

      items.push({ item: 'Separator' });
      items.push({
        id: 'account-generate',
        text: 'Generate New User',
        enabled: !accountBusy,
        action: () => {
          void createAccount();
        },
      });
      items.push({
        id: 'account-add-existing',
        text: 'Add Existing Secret…',
        enabled: !accountBusy,
        action: () => {
          void promptForAccountSecretFromNativeMenu();
        },
      });
      items.push({
        id: 'account-manage-users',
        text: 'Manage Users…',
        enabled: true,
        action: () => {
          void goSettings('users');
        },
      });

      if (currentAccount) {
        items.push({
          id: 'account-remove-active',
          text: 'Remove Active User…',
          enabled: !accountBusy,
          action: () => {
            void confirmRemoveCurrentAccountFromNativeMenu();
          },
        });
      }

      const menu = await Menu.new({ items });
      try {
        await menu.popup(popupLogicalPositionForElement(accountButtonEl, {
          offsetX: Math.max(0, accountButtonEl.offsetWidth - 8),
          offsetY: 6,
        }));
      } finally {
        await menu.close().catch(() => {});
      }
      nativeAccountMenuFallback = false;
      return true;
    } catch (error) {
      nativeAccountMenuFallback = true;
      console.warn('[Iris] native account menu unavailable:', error);
      return false;
    }
  }

  async function tryShowNativeHistoryMenu(): Promise<boolean> {
    if (!canTryNativeHistoryMenu() || !addressBarEl || nativeHistoryMenuBusy || dropdownItems.length === 0) {
      return false;
    }

    nativeHistoryMenuBusy = true;
    try {
      const menu = await Menu.new({
        items: [
          {
            id: 'history-header',
            text: addressValue.trim() ? 'Suggestions' : 'Recent History',
            enabled: false,
          },
          { item: 'Separator' },
          ...dropdownItems.map((entry) => ({
            id: `history-${entry.path}`,
            text: historyMenuLabel(entry),
            action: () => {
              handleDropdownSelect(entry);
            },
          })),
        ],
      });

      try {
        await menu.popup(popupLogicalPositionForElement(addressBarEl, { offsetY: 6 }));
      } finally {
        await menu.close().catch(() => {});
      }

      if (showDropdown) {
        closeDropdown();
      }
      nativeHistoryMenuFallback = false;
      return true;
    } catch (error) {
      nativeHistoryMenuFallback = true;
      console.warn('[Iris] native history menu unavailable:', error);
      return false;
    } finally {
      nativeHistoryMenuBusy = false;
    }
  }

  async function toggleAccountMenu() {
    showMobileMenu = false;
    if (showDropdown || isAddressFocused) {
      dismissDropdown();
    }
    accountError = '';
    pendingAccountRemovalPubkey = null;
    nativeAccountMenuFallback = false;
    if (await tryShowNativeAccountMenu()) {
      showAccountMenu = false;
      return;
    }
    showAccountMenu = !showAccountMenu;
  }

  async function saveAccountSecret() {
    try {
      await addAccountSecret(accountSecretDraft);
    } catch {
      // addAccountSecret already stored the user-facing error
    }
  }

  function scheduleAccountSecretClipboardClear(secret: string) {
    if (accountSecretClipboardClearTimer) {
      clearTimeout(accountSecretClipboardClearTimer);
    }
    accountSecretClipboardClearTimer = setTimeout(() => {
      void clearClipboardIfUnchanged(secret);
    }, ACCOUNT_SECRET_CLIPBOARD_CLEAR_DELAY_MS);
  }

  function handleAccountSecretPaste(event: ClipboardEvent) {
    const pasted = event.clipboardData?.getData('text/plain')?.trim();
    if (!pasted) return;
    void clearClipboardIfUnchanged(pasted);
    scheduleAccountSecretClipboardClear(pasted);
  }

  async function createAccount() {
    if (accountBusy) return;

    const previousPubkey = currentAccount?.pubkey ?? null;
    accountBusy = true;
    accountError = '';
    try {
      await generateNip07Account();
      await loadNip07Accounts();
      accountSecretDraft = '';
      showAddAccountSecret = false;
      await refreshChildWebviewForAccountChange(previousPubkey, activeAccountPubkey);
    } catch (error) {
      accountError = formatWebviewError(error);
    } finally {
      accountBusy = false;
    }
  }

  async function switchToAccount(account: Nip07AccountSummary) {
    if (accountBusy || account.pubkey === activeAccountPubkey) return;

    const previousPubkey = currentAccount?.pubkey ?? null;
    accountBusy = true;
    accountError = '';
    try {
      await setActiveNip07Account(account.pubkey);
      await loadNip07Accounts();
      pendingAccountRemovalPubkey = null;
      showAccountMenu = false;
      await refreshChildWebviewForAccountChange(previousPubkey, account.pubkey);
    } catch (error) {
      accountError = formatWebviewError(error);
    } finally {
      accountBusy = false;
    }
  }

  function startRemoveAccount(pubkey: string) {
    pendingAccountRemovalPubkey = pubkey;
  }

  function cancelRemoveAccount() {
    pendingAccountRemovalPubkey = null;
  }

  async function confirmRemoveAccount(pubkey: string) {
    if (accountBusy) return;

    const previousPubkey = currentAccount?.pubkey ?? null;
    accountBusy = true;
    accountError = '';
    try {
      const nextSummary = await removeNip07Account(pubkey);
      applyAccountsSummary(nextSummary);
      pendingAccountRemovalPubkey = null;
      if (savedAccounts.length === 0) {
        showAccountMenu = false;
      }
      await refreshChildWebviewForAccountChange(
        previousPubkey,
        nextAccountPubkeyAfterMutation(nextSummary),
      );
    } catch (error) {
      accountError = formatWebviewError(error);
    } finally {
      accountBusy = false;
    }
  }

  function permissionMethodLabel(method: string): string {
    switch (method) {
      case 'getPublicKey':
        return 'read your public key';
      case 'signEvent':
        return 'sign Nostr events';
      case 'nip04.encrypt':
      case 'nip44.encrypt':
        return 'encrypt messages';
      case 'nip04.decrypt':
      case 'nip44.decrypt':
        return 'decrypt messages';
      default:
        return method;
    }
  }

  function permissionOriginLabel(origin: string): string {
    try {
      const parsed = new URL(origin);
      return parsed.host || origin;
    } catch {
      return origin.replace(/^htree:\/\//, '');
    }
  }

  async function pollNip07PermissionQueue() {
    try {
      const prompt = await takeNip07PermissionPrompt();
      if (!prompt) return;
      if (permissionPromptQueue.some((existing) => existing.requestId === prompt.requestId)) return;
      showMobileMenu = false;
      showAccountMenu = false;
      permissionPromptError = '';
      permissionPromptQueue = [...permissionPromptQueue, prompt];
    } catch {
      // Browser tests and non-native contexts may not support prompt polling.
    }
  }

  async function respondToPermissionPrompt(
    decision: 'deny' | 'allowSession' | 'allowAlways' | 'blockSite',
  ) {
    const prompt = currentPermissionPrompt;
    if (!prompt || permissionPromptBusy) return;

    permissionPromptBusy = true;
    permissionPromptError = '';
    try {
      await respondNip07PermissionPrompt(prompt.requestId, decision);
      permissionPromptQueue = permissionPromptQueue.filter(
        (existing) => existing.requestId !== prompt.requestId,
      );
    } catch (error) {
      permissionPromptError = formatWebviewError(error);
    } finally {
      permissionPromptBusy = false;
      await pollNip07PermissionQueue();
    }
  }

  function toggleFavorite() {
    if (!currentUrl) return;
    if (isFavorited) {
      appsStore.remove(currentUrl);
    } else {
      appsStore.add({
        url: currentUrl,
        name: bookmarkSavedName(currentUrl, childDocumentTitle),
        addedAt: Date.now(),
      });
    }
  }

  async function installCurrentPwa() {
    showMobileMenu = false;
    showAccountMenu = false;
    if (!canInstallCurrentPwa || !detectedPwa || isInstallingPwa) return;
    isInstallingPwa = true;
    try {
      const installed = await installSitePwa(detectedPwa.sourceUrl);
      appsStore.add({
        url: installed.launchUrl,
        name: installed.name || detectedPwa.name || bookmarkSavedName(detectedPwa.sourceUrl, childDocumentTitle),
        icon: installed.iconUrl ?? detectedPwa.iconUrl,
        sourceAppId: installed.sourceAppId ?? detectedPwa.sourceAppId,
        sourceUrl: installed.sourceUrl,
        sourceManifestUrl: installed.sourceManifestUrl,
        addedAt: Date.now(),
      });
    } catch (error) {
      console.warn('[Iris] failed to install PWA:', error);
    } finally {
      isInstallingPwa = false;
    }
  }

  async function refresh() {
    showMobileMenu = false;
    showAccountMenu = false;
    if (currentView === 'webview' && currentUrl && !childWebviewReady) {
      await navigate(currentUrl, { pushHistory: false });
      return;
    }
    if (currentView === 'webview' && childWebviewReady) {
      await reloadWebview(CHILD_LABEL);
    }
  }

  async function openAddressOwnerProfile(host: string) {
    showMobileMenu = false;
    showAccountMenu = false;
    closeDropdown();
    isAddressFocused = false;
    addressInputEl?.blur();
    await navigate(ownerProfileUrl(host));
  }

  async function fetchDropdownItems(query: string) {
    try {
      if (!query.trim()) {
        const recent = await getRecentHistory(8);
        dropdownItems = recent;
      } else {
        const results = await searchHistory(query, 8);
        dropdownItems = results.map(r => r.entry);
      }
    } catch (e) {
      console.error('[Iris] history fetch failed:', e);
      dropdownItems = [];
    }
    selectedIndex = -1;
    if (nativeHistoryMenuRequested && showDropdown && dropdownItems.length > 0) {
      nativeHistoryMenuRequested = false;
      void tryShowNativeHistoryMenu();
    }
    scheduleWebviewBoundsUpdate();
  }

  function debouncedSearch(query: string) {
    if (debounceTimer) clearTimeout(debounceTimer);
    debounceTimer = setTimeout(() => fetchDropdownItems(query), 150);
  }

  function closeDropdown() {
    showDropdown = false;
    dropdownItems = [];
    selectedIndex = -1;
    nativeHistoryMenuRequested = false;
    nativeHistoryMenuFallback = false;
    if (debounceTimer) clearTimeout(debounceTimer);
    scheduleWebviewBoundsUpdate();
  }

  function handleDropdownSelect(entry: HistoryEntry) {
    closeDropdown();
    isAddressFocused = false;
    addressValue = urlToDisplay(entry.path);
    currentUrl = entry.path;
    navigate(entry.path);
    addressInputEl?.blur();
  }

  function handlePageLoadEvent(event: WebviewPageLoadEvent) {
    if (event.label !== CHILD_LABEL) return;
    const previousUrl = currentUrl;
    const nextUrl = normalizeChildReportedUrl(event.url, previousUrl);
    const requiresRecreation = (
      event.event === 'started' &&
      currentView === 'webview' &&
      !!previousUrl &&
      nextUrl !== previousUrl &&
      shouldRecreateBrowserForUrl(nextUrl, previousUrl)
    );

    if (requiresRecreation) {
      void navigate(nextUrl, { pushHistory: false });
      return;
    }

    if (currentView === 'webview' && nextUrl && nextUrl !== currentUrl) {
      currentUrl = nextUrl;
      if (!isAddressFocused) {
        addressValue = urlToDisplay(nextUrl);
      }
    }

    childPageLoadState = event.event;
    childPageLoadUrl = nextUrl;
    if (event.event === 'started') {
      detectedPwa = null;
      isInstallingPwa = false;
      clearBlankSuggestedTreeRecoveryTimer();
      return;
    }
    clearChildLoadStallRecoveryTimer();
    if (event.event === 'finished' && currentUrl && parseHtreeUrl(currentUrl)) {
      const scheduledUrl = currentUrl;
      clearBlankSuggestedTreeRecoveryTimer();
      blankSuggestedTreeRecoveryTimer = setTimeout(() => {
        blankSuggestedTreeRecoveryTimer = null;
        if (
          currentView !== 'webview' ||
          currentUrl !== scheduledUrl ||
          childPageLoadState !== 'finished' ||
          hasChildDiagnosticsSnapshot()
        ) {
          return;
        }
        void recoverHtreeWebview(scheduledUrl, {
          reason: 'blank',
          preferPlainLoopbackHost: true,
        });
      }, BLANK_SUGGESTED_TREE_RECOVERY_DELAY_MS);
    }
  }

  async function recoverHtreeWebview(url: string, options: {
    reason: string;
    preferPlainLoopbackHost?: boolean;
  }) {
    const htree = parseHtreeUrl(url);
    if (!htree) return;
    const {
      reason,
      preferPlainLoopbackHost = false,
    } = options;

    const attemptKey = `${url}|${reason}`;
    const attempts = treeRootRecoveryAttempts.get(attemptKey) ?? 0;
    if (attempts >= 1) return;
    treeRootRecoveryAttempts.set(attemptKey, attempts + 1);

    try {
      clearBlankSuggestedTreeRecoveryTimer();
      clearChildLoadStallRecoveryTimer();
      if (preferPlainLoopbackHost) {
        plainLoopbackFallbackScopes.add(browserIsolationScope(url));
      }
      await destroyChildWebview();
      await navigate(url, {
        pushHistory: false,
        preferPlainLoopbackHost,
      });
    } catch (error) {
      console.warn('[Iris] failed to recover htree webview:', error);
    }
  }

  async function maybeRecoverSuggestedTreeRoot(url: string, bodyText: string) {
    if (!RECOVERABLE_TREE_BODY_TEXTS.has(bodyText.trim())) return;
    if (!shouldRefreshBuiltInAppTreeRoot(url)) return;
    await recoverHtreeWebview(url, {
      reason: bodyText.trim(),
      preferPlainLoopbackHost: true,
    });
  }

  function handleDiagnosticEvent(event: WebviewDiagnosticEvent) {
    if (event.label !== CHILD_LABEL) return;
    if (event.title) childDocumentTitle = event.title;
    const detected = detectPwaFromDiagnostic(event);
    if (detected) detectedPwa = detected;
    if (event.title && currentUrl && isRecordableUrl(currentUrl)) {
      recordHistoryVisit(buildHistoryEntry(currentUrl, event.title))
        .catch((error) => console.warn('[Iris] record history failed:', error));
    }
    if (event.bodyText) childBodyText = event.bodyText;
    if (event.mediaSummary) childMediaSummary = event.mediaSummary;
    if (typeof event.viewportWidth === 'number') childViewportWidth = event.viewportWidth;
    if (typeof event.viewportHeight === 'number') childViewportHeight = event.viewportHeight;
    if (event.error && isFatalChildDiagnosticError(event.error, event.source)) {
      childLastError = event.error;
    }
    if (event.bodyText && currentUrl) {
      void maybeRecoverSuggestedTreeRoot(currentUrl, event.bodyText);
    }
  }

  async function handleDeleteHistoryItem(event: MouseEvent, path: string) {
    event.stopPropagation();
    await deleteHistoryEntry(path);
    dropdownItems = dropdownItems.filter(item => item.path !== path);
  }

  function handleAddressFocus() {
    showMobileMenu = false;
    showAccountMenu = false;
    nativeAccountMenuFallback = false;
    // Cancel any pending blur-close so it doesn't kill the new dropdown
    if (blurTimer) { clearTimeout(blurTimer); blurTimer = null; }
    isAddressFocused = true;
    if (currentUrl) {
      addressValue = currentUrl;
    }
    showDropdown = true;
    nativeHistoryMenuRequested = canTryNativeHistoryMenu();
    nativeHistoryMenuFallback = false;
    fetchDropdownItems(addressValue);
    scheduleWebviewBoundsUpdate();
    // Select all text for easy replacement
    requestAnimationFrame(() => addressInputEl?.select());
  }

  function handleAddressBlur() {
    isAddressFocused = false;
    if (currentUrl) {
      addressValue = urlToDisplay(currentUrl);
    }
    // Delay to allow mousedown on dropdown items to fire first
    blurTimer = setTimeout(() => { blurTimer = null; closeDropdown(); }, 150);
  }

  function dismissDropdown() {
    if (blurTimer) {
      clearTimeout(blurTimer);
      blurTimer = null;
    }
    isAddressFocused = false;
    closeDropdown();
    addressInputEl?.blur();
  }

  function scheduleWebviewBoundsUpdate() {
    if (boundsRaf !== null) cancelAnimationFrame(boundsRaf);
    boundsRaf = requestAnimationFrame(async () => {
      boundsRaf = null;
      if (currentView !== 'webview' || !childWebviewReady) return;
      const { top, bottom } = browserViewportInsets();
      const height = Math.max(0, window.innerHeight - top - bottom);
      try {
        await setWebviewBounds(CHILD_LABEL, 0, top, window.innerWidth, height);
      } catch {
        // If the webview is gone or not ready, ignore.
      }
    });
  }

  function scheduleAutomationStateSync() {
    if (automationSyncRaf !== null) cancelAnimationFrame(automationSyncRaf);
    automationSyncRaf = requestAnimationFrame(() => {
      automationSyncRaf = null;
      const { top, bottom } = browserViewportInsets();
      const childBoundsHeight = currentView === 'webview'
        ? Math.max(0, window.innerHeight - top - bottom)
        : 0;
      automationUpdateState({
        shellReady: true,
        currentView: currentView,
        currentUrl: currentUrl,
        addressValue: addressValue,
        canGoBack: canGoBack,
        canGoForward: canGoForward,
        showDropdown: showDropdown,
        childWebviewReady: childWebviewReady,
        childPageLoadState: childPageLoadState,
        childPageLoadUrl: childPageLoadUrl,
        childDocumentTitle: childDocumentTitle,
        childBodyText: childBodyText,
        childMediaSummary: childMediaSummary,
        childLastError: childLastError,
        historyIndex: historyIndex,
        historyLength: historyStack.length,
        windowInnerHeight: Math.round(window.innerHeight),
        windowOuterHeight: Math.round(window.outerHeight),
        toolbarHeight: Math.round(toolbarHeight),
        childBoundsTop: Math.round(top),
        childBoundsHeight: Math.round(childBoundsHeight),
        childViewportWidth: Math.round(childViewportWidth),
        childViewportHeight: Math.round(childViewportHeight),
        pendingNip07PromptRequestId: currentPermissionPrompt?.requestId ?? '',
        pendingNip07PromptOrigin: currentPermissionPrompt?.origin ?? '',
        pendingNip07PromptMethod: currentPermissionPrompt?.method ?? '',
      }).catch(() => {
        // Browser dev mode and tests without native commands can ignore this.
      });
    });
  }

  async function handleAutomationCommand(command: AutomationCommandEvent) {
    switch (command.action) {
      case 'open_url': {
        const rawUrl = command.url?.trim();
        if (!rawUrl) return;
        const url = displayToUrl(rawUrl);
        currentUrl = url;
        addressValue = isAddressFocused ? url : urlToDisplay(url);
        await navigate(url);
        return;
      }
      case 'back':
        await goBack();
        return;
      case 'forward':
        await goForward();
        return;
      case 'reload':
        await refresh();
        return;
      case 'home':
        await goHome();
        return;
      case 'settings':
        await goSettings();
        return;
      case 'respond_nip07_prompt': {
        const requestId = command.requestId?.trim();
        const decision = command.decision ?? null;
        if (!requestId || !decision) return;
        await respondNip07PermissionPrompt(requestId, decision);
        permissionPromptQueue = permissionPromptQueue.filter(
          (existing) => existing.requestId !== requestId,
        );
        permissionPromptError = '';
        await pollNip07PermissionQueue();
        return;
      }
      case 'shutdown':
        await automationShutdown();
        return;
      default:
        console.warn('[Iris] unknown automation action:', command.action);
    }
  }

  function handleGlobalKeyDown(event: KeyboardEvent) {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'l') {
      event.preventDefault();
      addressInputEl?.focus();
      return;
    }
    if (isEscapeKey(event) && showAccountMenu) {
      event.preventDefault();
      showAccountMenu = false;
      return;
    }
    if (isEscapeKey(event) && showMobileMenu) {
      event.preventDefault();
      showMobileMenu = false;
      return;
    }
    if (!isEscapeKey(event) || !showDropdown) return;
    event.preventDefault();
    dismissDropdown();
  }

  function handleGlobalPointerDown(event: PointerEvent) {
    const target = event.target;
    if (showAccountMenu) {
      if (!(target instanceof Element)) {
        showAccountMenu = false;
      } else if (
        !target.closest('[data-testid="account-menu"]') &&
        !target.closest('[data-testid="account-button"]')
      ) {
        showAccountMenu = false;
      }
    }

    if (!showMobileMenu) return;
    if (!(target instanceof Element)) {
      showMobileMenu = false;
      return;
    }
    if (target.closest('[data-testid="mobile-more-menu"]')) return;
    if (target.closest('button[title="More"]')) return;
    showMobileMenu = false;
  }

  async function goBack() {
    if (currentView === 'webview' && webviewNavDepth > 0) {
      // Navigate back within the webview
      ignoreLocationEvents++;
      await webviewHistory(CHILD_LABEL, 'back');
      webviewNavDepth--;
      webviewFwdAvail++;
    } else if (historyIndex > 0) {
      historyIndex--;
      await navigate(historyStack[historyIndex], { pushHistory: false });
    } else {
      // At first page or no history — go to launcher
      historyIndex = -1;
      goHome();
    }
  }

  async function goForward() {
    if (currentView === 'webview' && webviewFwdAvail > 0) {
      // Navigate forward within the webview
      ignoreLocationEvents++;
      await webviewHistory(CHILD_LABEL, 'forward');
      webviewNavDepth++;
      webviewFwdAvail--;
    } else if (historyIndex < historyStack.length - 1) {
      historyIndex++;
      await navigate(historyStack[historyIndex], { pushHistory: false });
    }
  }

  function handleAddressSubmit() {
    if (showDropdown && selectedIndex >= 0 && selectedIndex < dropdownItems.length) {
      handleDropdownSelect(dropdownItems[selectedIndex]);
      return;
    }
    closeDropdown();
    const url = displayToUrl(addressValue);
    isAddressFocused = false;
    if (url) {
      currentUrl = url;
      addressValue = urlToDisplay(url);
      navigate(url);
    }
    addressInputEl?.blur();
  }

  $effect(() => {
    currentView;
    currentUrl;
    addressValue;
    canGoBack;
    canGoForward;
    showDropdown;
    childPageLoadState;
    childPageLoadUrl;
    childDocumentTitle;
    childBodyText;
    childMediaSummary;
    childViewportWidth;
    childViewportHeight;
    childLastError;
    historyIndex;
    historyStack.length;
    scheduleAutomationStateSync();
  });

  $effect(() => {
    toolbarHeight;
    scheduleWebviewBoundsUpdate();
  });

  onMount(async () => {
    const visualViewport = window.visualViewport;
    const handleShellHashChange = () => {
      const routeTab = parseSettingsRouteFromHash(window.location.hash);
      if (routeTab === undefined) {
        if (currentView === 'settings') {
          void goHome();
        }
        return;
      }
      void goSettings(routeTab, false);
    };
    const unlistenLocation = await onChildWebviewLocation(handleLocationChange);
    const unlistenPageLoad = await onChildWebviewPageLoad(handlePageLoadEvent);
    const unlistenDiagnostic = await onChildWebviewDiagnostic(handleDiagnosticEvent);
    const unlistenAutomation = await onAutomationCommand((command) => {
      handleAutomationCommand(command).catch((error) => {
        console.warn('[Iris] automation command failed:', error);
      });
    });
    const initialSettingsRoute = parseSettingsRouteFromHash(window.location.hash);
    if (initialSettingsRoute !== undefined) {
      await goSettings(initialSettingsRoute, false);
    }
    try {
      const pendingDeepLinks = await deepLinkFrontendReady();
      for (const url of pendingDeepLinks) {
        await handleAutomationCommand({ action: 'open_url', url });
      }
    } catch (error) {
      console.warn('[Iris] deep-link initialization failed:', error);
    }
    appsStore.cacheRemoteIcons();
    await loadNip07Accounts();
    await pollNip07PermissionQueue();
    permissionPromptPollTimer = setInterval(() => {
      void pollNip07PermissionQueue();
    }, NIP07_PERMISSION_POLL_INTERVAL_MS);
    syncToolbarMode();
    syncKeyboardInsetBottom();
    scheduleAutomationStateSync();
    window.addEventListener('keydown', handleGlobalKeyDown);
    window.addEventListener('hashchange', handleShellHashChange);
    window.addEventListener('pointerdown', handleGlobalPointerDown);
    window.addEventListener('resize', syncToolbarMode);
    window.addEventListener('resize', scheduleWebviewBoundsUpdate);
    visualViewport?.addEventListener('resize', syncKeyboardInsetBottom);
    visualViewport?.addEventListener('scroll', syncKeyboardInsetBottom);
    return () => {
      window.removeEventListener('keydown', handleGlobalKeyDown);
      window.removeEventListener('hashchange', handleShellHashChange);
      window.removeEventListener('pointerdown', handleGlobalPointerDown);
      window.removeEventListener('resize', syncToolbarMode);
      window.removeEventListener('resize', scheduleWebviewBoundsUpdate);
      visualViewport?.removeEventListener('resize', syncKeyboardInsetBottom);
      visualViewport?.removeEventListener('scroll', syncKeyboardInsetBottom);
      if (automationSyncRaf !== null) cancelAnimationFrame(automationSyncRaf);
      if (permissionPromptPollTimer) clearInterval(permissionPromptPollTimer);
      if (accountSecretClipboardClearTimer) clearTimeout(accountSecretClipboardClearTimer);
      unlistenLocation();
      unlistenPageLoad();
      unlistenDiagnostic();
      unlistenAutomation();
    };
  });
</script>

<div class="h-[100dvh] max-h-[100dvh] flex flex-col overscroll-none overflow-hidden bg-surface-0">
  <div
    bind:this={safeAreaTopInsetEl}
    aria-hidden="true"
    class="pointer-events-none fixed left-0 top-0 h-0 w-0 overflow-hidden opacity-0"
    style="padding-top: env(safe-area-inset-top, 0px);"
  ></div>
  <!-- Browser chrome -->
  {#if isCompactToolbar}
    <div
      bind:offsetHeight={toolbarHeight}
      data-testid="toolbar"
      data-tauri-drag-region
      class="order-2 relative shrink-0 border-t border-surface-2 bg-surface-1 px-3 pt-2"
      style={`padding-bottom: calc(env(safe-area-inset-bottom, 0px) + 12px + ${keyboardInsetBottom}px);`}
    >
      {#if showMobileMenu && !isAddressFocused}
        <div
          bind:this={mobileMenuEl}
          data-testid="mobile-more-menu"
          data-tauri-drag-region="false"
          class="absolute bottom-full right-3 mb-2 w-52 overflow-hidden rounded-2xl bg-surface-1 b-1 b-solid b-surface-3 shadow-lg"
        >
          <button
            data-tauri-drag-region="false"
            class="w-full flex items-center justify-between px-4 py-3 text-left text-sm text-text-1 hover:bg-surface-2 transition-colors"
            onclick={async () => {
              showMobileMenu = false;
              await goHome();
            }}
          >
            <span>Home</span>
            <span class="i-lucide-home text-base text-text-3"></span>
          </button>
          <button
            data-tauri-drag-region="false"
            class="w-full flex items-center justify-between px-4 py-3 text-left text-sm text-text-1 hover:bg-surface-2 transition-colors disabled:opacity-40"
            onclick={async () => {
              showMobileMenu = false;
              await goForward();
            }}
            disabled={!canGoForward}
          >
            <span>Forward</span>
            <span class="i-lucide-chevron-right text-base text-text-3"></span>
          </button>
          {#if currentUrl}
            <button
              data-tauri-drag-region="false"
              class="w-full flex items-center justify-between px-4 py-3 text-left text-sm text-text-1 hover:bg-surface-2 transition-colors"
              onclick={async () => {
                showMobileMenu = false;
                await refresh();
              }}
            >
              <span>Refresh</span>
              <span class="i-lucide-refresh-cw text-base text-text-3"></span>
            </button>
          {/if}
          <button
            data-tauri-drag-region="false"
            class="w-full flex items-center justify-between px-4 py-3 text-left text-sm text-text-1 hover:bg-surface-2 transition-colors"
            onclick={() => {
              showMobileMenu = false;
              void goSettings();
            }}
          >
            <span>Settings</span>
            <span class="i-lucide-settings text-base text-text-3"></span>
          </button>
        </div>
      {/if}

      <div data-tauri-drag-region class="flex items-center gap-2">
        {#if !isAddressFocused}
          <button
            data-tauri-drag-region="false"
            class="btn-circle btn-ghost shrink-0"
            class:opacity-40={!canGoBack}
            onclick={goBack}
            disabled={!canGoBack}
            title="Back"
          >
            <span class="i-lucide-chevron-left text-lg"></span>
          </button>
        {/if}

        <div data-tauri-drag-region class="flex-1 min-w-0 relative">
          <div
            bind:this={addressBarEl}
            data-testid="address-bar"
            data-tauri-drag-region="false"
            class="w-full min-w-0 flex items-center gap-2 rounded-full bg-surface-0 b-1 b-solid b-surface-3 px-4 py-2 transition-all {isAddressFocused ? 'b-accent' : ''}"
            role="button"
            tabindex={isAddressFocused ? -1 : 0}
            aria-label="Focus address bar"
            onclick={handleAddressChromeClick}
            onkeydown={handleAddressChromeKeyDown}
          >
            {#if currentUrl && !isAddressFocused}
              <button
                data-tauri-drag-region="false"
                class="shrink-0 text-text-3 hover:text-text-1"
                onclick={refresh}
                title={isChildLoading ? 'Loading' : 'Refresh'}
              >
                {#if isChildLoading}
                  <LoadingSpinner class="h-4 w-4" testId="address-loading-spinner" />
                {:else}
                  <span class="i-lucide-refresh-cw text-sm"></span>
                {/if}
              </button>
            {/if}
            <span data-tauri-drag-region="false" class="i-lucide-search text-sm text-muted shrink-0"></span>
            <div data-tauri-drag-region="false" class="relative min-w-0 flex-1">
              {#if blurredOwnerSummary}
                <div class="absolute inset-0 overflow-hidden">
                  <div class="flex h-full min-w-0 max-w-full items-center gap-1.5 pr-2">
                    <AddressOwnerPill
                      host={blurredOwnerSummary.host}
                      openProfile={() => void openAddressOwnerProfile(blurredOwnerSummary.host)}
                      maxWidthClass="max-w-full"
                      allowShrink={true}
                      size="xs"
                      testId="address-owner-pill"
                    />
                    {#if blurredOwnerSummary.treeName}
                      <span data-testid="address-path" class="shrink-0 text-xs text-text-2">
                        {blurredOwnerSummary.treeName}
                      </span>
                    {/if}
                  </div>
                </div>
              {:else if blurredNhashTitle}
                <div class="absolute inset-0 overflow-hidden">
                  <div class="flex h-full w-full min-w-0 max-w-full items-center justify-center px-2">
                    <span
                      data-testid="address-title-text"
                      class="block min-w-0 max-w-full truncate text-center text-sm text-text-1"
                      title={blurredNhashTitle}
                    >
                      {blurredNhashTitle}
                    </span>
                  </div>
                </div>
              {/if}
              <input
                type="text"
                data-tauri-drag-region="false"
                autocorrect="off"
                autocapitalize="none"
                autocomplete="off"
                bind:this={addressInputEl}
                bind:value={addressValue}
                onfocus={handleAddressFocus}
                onblur={handleAddressBlur}
                onbeforeinput={handleAddressBeforeInput}
                onkeypress={handleAddressKeyPress}
                oninput={handleAddressInput}
                onkeydown={handleAddressKeyDown}
                onkeyup={handleAddressKeyUp}
                placeholder="Search or enter address"
                spellcheck={false}
                class={`w-full bg-transparent border-none outline-none text-sm text-text-1 placeholder:text-muted min-w-0 text-left ${(blurredOwnerSummary || blurredNhashTitle) ? 'pointer-events-none opacity-0' : ''}`}
              />
            </div>
            {#if !isAddressFocused}
              {#if canInstallCurrentPwa}
                <button
                  data-testid="install-pwa-button"
                  data-tauri-drag-region="false"
                  class="shrink-0 text-text-3 hover:text-text-1 disabled:opacity-30"
                  onclick={installCurrentPwa}
                  disabled={isInstallingPwa}
                  title={currentPwaBookmark ? 'Update in Iris home screen' : 'Add to Iris home screen'}
                >
                  {#if isInstallingPwa}
                    <LoadingSpinner class="h-4 w-4" />
                  {:else if currentPwaBookmark}
                    <span class="i-lucide-refresh-cw"></span>
                  {:else}
                    <span class="i-lucide-download"></span>
                  {/if}
                </button>
              {/if}
              <button
                data-tauri-drag-region="false"
                class="shrink-0 text-text-3 hover:text-text-1 disabled:opacity-30"
                onclick={toggleFavorite}
                disabled={!currentUrl}
                title={isFavorited ? 'Unfavourite' : 'Favourite'}
              >
                {#if isFavorited}
                  <span class="i-lucide-star text-yellow-500 fill-yellow-500"></span>
                {:else}
                  <span class="i-lucide-star"></span>
                {/if}
              </button>
            {/if}
          </div>

          {#if showShellHistoryDropdown}
            <div
              bind:this={dropdownEl}
              class="absolute bottom-full left-0 right-0 mb-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-lg overflow-hidden z-50 max-h-80 overflow-y-auto"
              role="listbox"
            >
              {#each dropdownItems as item, i}
                {@const ownerSummary = historyOwnerSummary(item)}
                <div
                  class="w-full flex items-center gap-2 px-3 py-2 text-left hover:bg-surface-2 transition-colors cursor-pointer {i === selectedIndex ? 'bg-surface-2' : ''}"
                  onmousedown={() => handleDropdownSelect(item)}
                  role="option"
                  aria-selected={i === selectedIndex}
                  tabindex="-1"
                >
                  <HistoryEntryIcon entry={item} />
                  <div class="flex-1 min-w-0 text-sm text-text-1">
                    {#if ownerSummary}
                      <div class="flex items-center gap-1.5 min-w-0">
                        <AddressOwnerPill
                          host={ownerSummary.host}
                          interactive={false}
                          showBackground={false}
                          maxWidthClass="max-w-40"
                          size="xs"
                        />
                        <span class="min-w-0 truncate">{ownerSummary.displayLabel}</span>
                      </div>
                    {:else}
                      <div class="truncate">{historyWebLabel(item)}</div>
                    {/if}
                  </div>
                  <button
                    class="shrink-0 text-text-3 hover:text-danger p-1"
                    onmousedown={(e) => handleDeleteHistoryItem(e, item.path)}
                    title="Delete"
                  >
                    <span class="i-lucide-x text-sm"></span>
                  </button>
                </div>
              {/each}
            </div>
          {/if}
        </div>

        {#if !isAddressFocused}
          <button
            bind:this={accountButtonEl}
            data-testid="account-button"
            data-account-state={currentAccount ? 'signed-in' : 'signed-out'}
            data-tauri-drag-region="false"
            class="btn-circle btn-ghost shrink-0 overflow-hidden"
            onclick={toggleAccountMenu}
            title={currentAccount ? `Switch Nostr user (${currentAccountName})` : 'Sign in to Nostr'}
          >
            {#if currentAccount && accountAvatarUrl}
              <img
                src={accountAvatarUrl}
                alt="Current Nostr account"
                class="h-7 w-7 rounded-full"
              />
            {:else}
              <span class="i-lucide-user-round text-lg"></span>
            {/if}
          </button>
          <button
            data-tauri-drag-region="false"
            class="btn-circle btn-ghost shrink-0"
            onclick={() => { showMobileMenu = !showMobileMenu; }}
            title="More"
          >
            <span class="i-lucide-ellipsis text-lg"></span>
          </button>
        {/if}
      </div>

      {#if currentPermissionPrompt}
        <Nip07PermissionBar
          prompt={currentPermissionPrompt}
          busy={permissionPromptBusy}
          error={permissionPromptError}
          compact={true}
          permissionMethodLabel={permissionMethodLabel}
          permissionOriginLabel={permissionOriginLabel}
          respond={(decision) => void respondToPermissionPrompt(decision)}
        />
      {/if}
    </div>
  {:else}
    <div
      bind:offsetHeight={toolbarHeight}
      data-testid="toolbar"
      data-tauri-drag-region
      class="shrink-0 border-b border-surface-2 bg-surface-1 px-3 py-2"
      style={`padding-left: ${DESKTOP_TRAFFIC_LIGHTS_PADDING}px;`}
    >
      <div class="flex h-8 items-center gap-2">
      <div data-tauri-drag-region class="flex items-center gap-1 shrink-0">
        <button
          data-tauri-drag-region="false"
          class="btn-circle btn-ghost"
          class:opacity-40={!canGoBack}
          onclick={goBack}
          disabled={!canGoBack}
          title="Back"
        >
          <span class="i-lucide-chevron-left text-lg"></span>
        </button>
        <button
          data-tauri-drag-region="false"
          class="btn-circle btn-ghost"
          class:opacity-40={!canGoForward}
          onclick={goForward}
          disabled={!canGoForward}
          title="Forward"
        >
          <span class="i-lucide-chevron-right text-lg"></span>
        </button>
        <button data-tauri-drag-region="false" class="btn-circle btn-ghost" onclick={goHome} title="Home">
          <span class="i-lucide-home text-lg"></span>
        </button>
      </div>

      <div data-tauri-drag-region class="flex flex-1 min-w-0 relative justify-center">
        <div
          bind:this={addressBarEl}
          data-testid="address-bar"
          data-tauri-drag-region="false"
          class="w-full min-w-0 max-w-lg flex items-center gap-2 px-3 py-1 rounded-full bg-surface-0 b-1 b-solid b-surface-3 transition-colors {isAddressFocused ? 'b-accent' : ''}"
          role="button"
          tabindex={isAddressFocused ? -1 : 0}
          aria-label="Focus address bar"
          onclick={handleAddressChromeClick}
          onkeydown={handleAddressChromeKeyDown}
        >
          {#if currentUrl}
            <button
              data-tauri-drag-region="false"
              class="shrink-0 text-text-3 hover:text-text-1"
              onclick={refresh}
              title={isChildLoading ? 'Loading' : 'Refresh'}
            >
              {#if isChildLoading}
                <LoadingSpinner class="h-4 w-4" testId="address-loading-spinner" />
              {:else}
                <span class="i-lucide-refresh-cw text-sm"></span>
              {/if}
            </button>
          {/if}
          <span data-tauri-drag-region="false" class="i-lucide-search text-sm text-muted shrink-0"></span>
          <div data-tauri-drag-region="false" class="relative min-w-0 flex-1">
            {#if blurredOwnerSummary}
              <div class="absolute inset-0 overflow-hidden">
                <div class="flex h-full min-w-0 max-w-full items-center gap-1.5 pr-2">
                  <AddressOwnerPill
                    host={blurredOwnerSummary.host}
                    openProfile={() => void openAddressOwnerProfile(blurredOwnerSummary.host)}
                    maxWidthClass="max-w-full"
                    allowShrink={true}
                    size="xs"
                    testId="address-owner-pill"
                  />
                  {#if blurredOwnerSummary.treeName}
                    <span data-testid="address-path" class="shrink-0 text-xs text-text-2">
                      {blurredOwnerSummary.treeName}
                    </span>
                  {/if}
                </div>
              </div>
            {:else if blurredNhashTitle}
              <div class="absolute inset-0 overflow-hidden">
                <div class="flex h-full w-full min-w-0 max-w-full items-center justify-center px-2">
                  <span
                    data-testid="address-title-text"
                    class="block min-w-0 max-w-full truncate text-center text-sm text-text-1"
                    title={blurredNhashTitle}
                  >
                    {blurredNhashTitle}
                  </span>
                </div>
              </div>
            {/if}
            <input
              type="text"
              data-tauri-drag-region="false"
              autocorrect="off"
              autocapitalize="none"
              autocomplete="off"
              bind:this={addressInputEl}
              bind:value={addressValue}
              onfocus={handleAddressFocus}
              onblur={handleAddressBlur}
              onbeforeinput={handleAddressBeforeInput}
              onkeypress={handleAddressKeyPress}
              oninput={handleAddressInput}
              onkeydown={handleAddressKeyDown}
              onkeyup={handleAddressKeyUp}
              placeholder="Search or enter address"
              spellcheck={false}
              class={`w-full bg-transparent border-none outline-none text-sm text-text-1 placeholder:text-muted min-w-0 text-center ${(blurredOwnerSummary || blurredNhashTitle) ? 'pointer-events-none opacity-0 text-left' : ''}`}
            />
          </div>
          {#if canInstallCurrentPwa}
            <button
              data-testid="install-pwa-button"
              data-tauri-drag-region="false"
              class="shrink-0 text-text-3 hover:text-text-1 disabled:opacity-30"
              onclick={installCurrentPwa}
              disabled={isInstallingPwa}
              title={currentPwaBookmark ? 'Update in Iris home screen' : 'Add to Iris home screen'}
            >
              {#if isInstallingPwa}
                <LoadingSpinner class="h-4 w-4" />
              {:else if currentPwaBookmark}
                <span class="i-lucide-refresh-cw"></span>
              {:else}
                <span class="i-lucide-download"></span>
              {/if}
            </button>
          {/if}
          <button
            data-tauri-drag-region="false"
            class="shrink-0 text-text-3 hover:text-text-1 disabled:opacity-30"
            onclick={toggleFavorite}
            disabled={!currentUrl}
            title={isFavorited ? 'Unfavourite' : 'Favourite'}
          >
            {#if isFavorited}
              <span class="i-lucide-star text-yellow-500 fill-yellow-500"></span>
            {:else}
              <span class="i-lucide-star"></span>
            {/if}
          </button>
        </div>

        {#if showShellHistoryDropdown}
          <div
            bind:this={dropdownEl}
            class="absolute top-full left-1/2 -translate-x-1/2 mt-1 w-full max-w-lg bg-surface-1 b-1 b-solid b-surface-3 rounded-lg overflow-hidden z-50 max-h-80 overflow-y-auto"
            role="listbox"
          >
            {#each dropdownItems as item, i}
              {@const ownerSummary = historyOwnerSummary(item)}
              <div
                class="w-full flex items-center gap-2 px-3 py-2 text-left hover:bg-surface-2 transition-colors cursor-pointer {i === selectedIndex ? 'bg-surface-2' : ''}"
                onmousedown={() => handleDropdownSelect(item)}
                role="option"
                aria-selected={i === selectedIndex}
                tabindex="-1"
              >
                <HistoryEntryIcon entry={item} />
                <div class="flex-1 min-w-0 text-sm text-text-1">
                  {#if ownerSummary}
                    <div class="flex items-center gap-1.5 min-w-0">
                      <AddressOwnerPill
                        host={ownerSummary.host}
                        interactive={false}
                        showBackground={false}
                        maxWidthClass="max-w-48"
                        size="xs"
                      />
                      <span class="min-w-0 truncate">{ownerSummary.displayLabel}</span>
                    </div>
                  {:else}
                    <div class="truncate">{historyWebLabel(item)}</div>
                  {/if}
                </div>
                <button
                  class="shrink-0 text-text-3 hover:text-danger p-1"
                  onmousedown={(e) => handleDeleteHistoryItem(e, item.path)}
                  title="Delete"
                >
                  <span class="i-lucide-x text-sm"></span>
                </button>
              </div>
            {/each}
          </div>
        {/if}
      </div>

      <button
        data-tauri-drag-region="false"
        class="btn-circle btn-ghost shrink-0"
        onclick={() => void goSettings()}
        title="Settings"
      >
        <span class="i-lucide-settings text-lg"></span>
      </button>
      <button
        bind:this={accountButtonEl}
        data-testid="account-button"
        data-account-state={currentAccount ? 'signed-in' : 'signed-out'}
        data-tauri-drag-region="false"
        class="btn-circle btn-ghost shrink-0 overflow-hidden"
        onclick={toggleAccountMenu}
        title={currentAccount ? `Switch Nostr user (${currentAccountName})` : 'Sign in to Nostr'}
      >
        {#if currentAccount && accountAvatarUrl}
          <img
            src={accountAvatarUrl}
            alt="Current Nostr account"
            class="h-7 w-7 rounded-full"
          />
        {:else}
          <span class="i-lucide-user-round text-lg"></span>
        {/if}
      </button>
      </div>

      {#if currentPermissionPrompt}
        <Nip07PermissionBar
          prompt={currentPermissionPrompt}
          busy={permissionPromptBusy}
          error={permissionPromptError}
          permissionMethodLabel={permissionMethodLabel}
          permissionOriginLabel={permissionOriginLabel}
          respond={(decision) => void respondToPermissionPrompt(decision)}
        />
      {/if}
    </div>
  {/if}

  {#if showShellAccountMenu}
    <div
      bind:this={accountMenuEl}
      data-testid="account-menu"
      data-tauri-drag-region="false"
      class="fixed z-60 w-[min(22rem,calc(100vw-1.5rem))] overflow-hidden rounded-2xl b-1 b-solid b-surface-3 bg-surface-1 shadow-lg"
      style={accountMenuStyle}
    >
      <div class="space-y-4 p-4">
        <div class="flex items-start gap-3">
          <div class="flex h-11 w-11 shrink-0 items-center justify-center overflow-hidden rounded-full bg-surface-2">
            {#if currentAccount && accountAvatarUrl}
              <img
                src={accountAvatarUrl}
                alt="Current Nostr account"
                class="h-full w-full rounded-full"
              />
            {:else}
              <span class="i-lucide-user-round text-lg text-text-3"></span>
            {/if}
          </div>
          <div class="min-w-0 flex-1">
            <div data-testid="account-current-name" class="truncate text-sm font-medium text-text-1">
              {currentAccount ? currentAccountName : 'No Nostr user selected'}
            </div>
          </div>
        </div>

        {#if sortedAccounts.length > 0}
          <div class="space-y-2">
            {#each sortedAccounts as account (account.pubkey)}
              {@const isActive = account.pubkey === activeAccountPubkey}
              {@const isConfirmingRemoval = pendingAccountRemovalPubkey === account.pubkey}
              {@const isSwitchable = !isActive && !isConfirmingRemoval && !accountBusy}
              <div
                data-testid="account-item"
                class="flex items-center gap-3 rounded-2xl px-3 py-3 transition-colors {isActive ? 'bg-surface-0' : 'bg-surface-1 hover:bg-surface-2'} {isSwitchable ? 'cursor-pointer' : ''}"
                role="button"
                aria-disabled={!isSwitchable}
                tabindex={isSwitchable ? 0 : -1}
                onclick={() => {
                  if (isSwitchable) {
                    void switchToAccount(account);
                  }
                }}
                onkeydown={(event) => {
                  if (event.key === 'Enter' && isSwitchable) {
                    event.preventDefault();
                    void switchToAccount(account);
                  }
                }}
              >
                <div class="flex h-10 w-10 shrink-0 items-center justify-center overflow-hidden rounded-full bg-surface-2">
                  <img
                    src={`data:image/svg+xml;utf8,${encodeURIComponent(minidenticon(account.pubkey, 40, 40))}`}
                    alt={accountDisplayName(account)}
                    class="h-full w-full rounded-full"
                  />
                </div>
                <div class="min-w-0 flex-1">
                  <div
                    data-testid={isActive ? 'active-account-name' : undefined}
                    class="truncate text-sm font-medium text-text-1"
                  >
                    {accountDisplayName(account)}
                  </div>
                  <div class="text-xs text-text-3">
                    {#if isActive}
                      Active in websites you open here
                    {:else}
                      Switch to this user
                    {/if}
                  </div>
                </div>
                <div class="shrink-0 flex items-center gap-2">
                  {#if isActive}
                    <span
                      data-testid="active-account-indicator"
                      class="i-lucide-check-circle text-base text-success"
                    ></span>
                  {/if}
                  {#if isConfirmingRemoval}
                    <button
                      data-testid="confirm-remove-account-button"
                      class="btn h-8 px-3 text-xs bg-danger text-white hover:opacity-90"
                      onclick={(event) => {
                        event.stopPropagation();
                        void confirmRemoveAccount(account.pubkey);
                      }}
                      disabled={accountBusy}
                    >
                      Remove
                    </button>
                    <button
                      class="btn btn-ghost h-8 px-3 text-xs"
                      onclick={(event) => {
                        event.stopPropagation();
                        cancelRemoveAccount();
                      }}
                      disabled={accountBusy}
                    >
                      Cancel
                    </button>
                  {:else}
                    <button
                      class="btn-circle btn-ghost h-8 w-8 text-text-3 hover:text-danger"
                      title={`Remove ${accountDisplayName(account)}`}
                      onclick={(event) => {
                        event.stopPropagation();
                        startRemoveAccount(account.pubkey);
                      }}
                      disabled={accountBusy}
                    >
                      <span class="i-lucide-trash-2 text-sm"></span>
                    </button>
                  {/if}
                </div>
              </div>
            {/each}
          </div>
        {/if}

        <div class="grid grid-cols-2 gap-2">
          <button
            data-testid="generate-account-button"
            class="btn bg-accent text-white hover:opacity-90 disabled:opacity-50"
            onclick={createAccount}
            disabled={accountBusy}
          >
            Generate New
          </button>
          <button
            data-testid="toggle-add-account-button"
            class="btn btn-ghost"
            onclick={() => {
              showAddAccountSecret = !showAddAccountSecret;
              accountError = '';
              pendingAccountRemovalPubkey = null;
              if (!showAddAccountSecret) {
                accountSecretDraft = '';
              }
            }}
            disabled={accountBusy}
          >
            {showAddAccountSecret ? 'Cancel' : 'Add Existing'}
          </button>
        </div>

        {#if showAddAccountSecret}
          <div class="space-y-3 rounded-2xl bg-surface-0 px-3 py-3">
            <div class="space-y-2">
              <label
                for="account-secret"
                class="block text-xs font-medium uppercase tracking-wide text-text-3"
              >
                Secret key
              </label>
              <input
                id="account-secret"
                data-testid="account-nsec-input"
                type="password"
                bind:value={accountSecretDraft}
                placeholder="Paste nsec or hex secret"
                autocomplete="off"
                autocorrect="off"
                autocapitalize="none"
                spellcheck={false}
                class="w-full rounded-xl bg-surface-1 px-3 py-2 text-sm text-text-1 outline-none ring-0 b-1 b-solid b-surface-3 focus:b-accent"
                onpaste={handleAccountSecretPaste}
                onkeydown={(event) => {
                  if (event.key === 'Enter') {
                    event.preventDefault();
                    void saveAccountSecret();
                  }
                }}
              />
            </div>
            <button
              data-testid="account-save-button"
              class="btn w-full bg-accent text-white hover:opacity-90 disabled:opacity-50"
              onclick={saveAccountSecret}
              disabled={accountBusy || !accountSecretDraft.trim()}
            >
              Add User
            </button>
          </div>
        {/if}

        <button
          data-testid="manage-users-button"
          class="btn btn-ghost w-full justify-between"
          onclick={() => void goSettings('users')}
        >
          <span>Manage Users</span>
          <span class="i-lucide-chevron-right text-sm text-text-3"></span>
        </button>

        {#if accountError}
          <div class="rounded-xl bg-danger/10 px-3 py-2 text-sm text-danger">
            {accountError}
          </div>
        {/if}
      </div>
    </div>
  {/if}

  <!-- Content area -->
  <main class="min-h-0 flex-1 flex flex-col {isCompactToolbar ? 'order-1' : ''}">
    {#if currentView === 'launcher'}
      <AppLauncher
        onnavigate={(url) => navigate(url)}
      />
    {:else if currentView === 'settings'}
      <Settings
        onnavigate={(url) => navigate(url)}
        selectedTab={settingsTab}
        onSelectTab={(tab) => void goSettings(tab, true)}
        nip07Accounts={savedAccounts}
        activeNip07AccountPubkey={activeAccountPubkey}
        exportNip07Secret={exportNip07AccountSecret}
      />
    {:else if !childWebviewReady || childLastError}
      <section class="flex flex-1 items-center justify-center p-6">
        {#if childLastError}
          <div
            data-testid="webview-error"
            class="w-full max-w-md rounded-3xl border border-surface-3 bg-surface-1 px-5 py-6 text-center shadow-lg"
          >
            <div class="mx-auto mb-4 flex h-12 w-12 items-center justify-center rounded-full bg-surface-2 text-text-1">
              <span class="i-lucide-triangle-alert text-xl text-warning"></span>
            </div>
            <h2 class="text-lg font-semibold text-text-1">{webviewErrorHeadline(childLastError)}</h2>
            <p class="mt-2 text-sm text-text-2">{webviewErrorDetail(childLastError)}</p>
            {#if currentUrl}
              <p class="mt-3 break-all text-xs text-text-3">{currentUrl}</p>
            {/if}
            {#if webviewErrorDetail(childLastError) !== childLastError}
              <p class="mt-3 break-all text-xs text-text-3">{childLastError}</p>
            {/if}
          </div>
        {:else}
          <LoadingSpinner testId="webview-loading-spinner" class="h-8 w-8 text-text-2" />
        {/if}
      </section>
    {/if}
    <!-- When currentView === 'webview', child webview overlays this area -->
  </main>
</div>
