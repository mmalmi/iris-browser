import { test as base, type Page } from '@playwright/test';
import { finalizeEvent, generateSecretKey, getPublicKey, nip04, nip19, nip44 } from 'nostr-tools';
import {
  attachRenderLoopGuardToPage,
  formatRenderLoopFailures,
  isRenderLoopMessage,
} from './renderLoopGuard';

type Fixtures = {
  tauriPage: Page;
  renderLoopErrors: Set<string>;
  renderLoopGuard: void;
};

function normalizeSecretKey(secret: string): Uint8Array {
  const trimmed = secret.trim();
  if (!trimmed) {
    throw new Error('Missing Nostr secret key');
  }

  if (trimmed.startsWith('nsec1')) {
    const decoded = nip19.decode(trimmed);
    if (decoded.type !== 'nsec' || !(decoded.data instanceof Uint8Array)) {
      throw new Error('Invalid Nostr secret key');
    }
    return decoded.data;
  }

  if (!/^[0-9a-fA-F]{64}$/.test(trimmed)) {
    throw new Error('Invalid Nostr secret key');
  }

  return Uint8Array.from(Buffer.from(trimmed, 'hex'));
}

function accountSummaryFromSecret(secretKey: Uint8Array, addedAt = Date.now()) {
  const pubkey = getPublicKey(secretKey);
  return {
    pubkey,
    npub: nip19.npubEncode(pubkey),
    addedAt,
  };
}

/**
 * Mock Tauri IPC so the shell UI can render in a regular browser.
 *
 * We intercept `window.__TAURI_INTERNALS__.invoke` and `window.__TAURI_INTERNALS__.transformCallback`
 * before the app boots so that calls like createNip07Webview / closeWebview don't throw.
 */
async function mockTauriIPC(page: Page) {
  const savedAccounts: Array<{
    secretKey: Uint8Array;
    summary: ReturnType<typeof accountSummaryFromSecret>;
  }> = [];
  let activeAccountPubkey: string | null = null;
  const grantedPermissions = new Map<string, Map<string, 'allowSession' | 'allowAlways'>>();
  const blockedOrigins = new Set<string>();
  const pendingPermissionPrompts: Array<{ requestId: string; origin: string; method: string }> = [];
  const permissionPromptWaiters = new Map<
    string,
    (decision: 'deny' | 'allowSession' | 'allowAlways' | 'blockSite') => void
  >();

  function activeAccount() {
    if (!activeAccountPubkey) return null;
    return savedAccounts.find((account) => account.summary.pubkey === activeAccountPubkey) ?? null;
  }

  function grantedPermissionFor(origin: string, method: string) {
    return grantedPermissions.get(origin)?.get(method) ?? null;
  }

  async function requestPermission(origin: string, method: string): Promise<boolean> {
    if (origin === 'tauri://localhost') {
      return true;
    }
    if (blockedOrigins.has(origin)) {
      return false;
    }
    const existing = grantedPermissionFor(origin, method);
    if (existing === 'allowSession' || existing === 'allowAlways') {
      return true;
    }

    const requestId = `${Date.now()}-${Math.random().toString(16).slice(2)}`;
    pendingPermissionPrompts.push({ requestId, origin, method });
    const decision = await new Promise<'deny' | 'allowSession' | 'allowAlways' | 'blockSite'>((resolve) => {
      permissionPromptWaiters.set(requestId, resolve);
    });

    if (decision === 'allowSession' || decision === 'allowAlways') {
      const originPermissions = grantedPermissions.get(origin) ?? new Map();
      originPermissions.set(method, decision);
      grantedPermissions.set(origin, originPermissions);
      return true;
    }

    if (decision === 'blockSite') {
      blockedOrigins.add(origin);
      grantedPermissions.delete(origin);
    }

    return false;
  }

  await page.exposeFunction('__irisNip07GetAccount', async () => {
    return activeAccount()?.summary ?? null;
  });

  await page.exposeFunction('__irisNip07ListAccounts', async () => {
    return {
      accounts: savedAccounts.map((account) => account.summary),
      activePubkey: activeAccountPubkey,
    };
  });

  await page.exposeFunction('__irisNip07Login', async (secret: string) => {
    const secretKey = normalizeSecretKey(secret);
    const summary = accountSummaryFromSecret(secretKey);
    const existing = savedAccounts.find((account) => account.summary.pubkey === summary.pubkey);
    if (existing) {
      existing.secretKey = secretKey;
      activeAccountPubkey = existing.summary.pubkey;
      return existing.summary;
    }
    savedAccounts.push({ secretKey, summary });
    activeAccountPubkey = summary.pubkey;
    return summary;
  });

  await page.exposeFunction('__irisNip07Generate', async () => {
    const secretKey = generateSecretKey();
    const summary = accountSummaryFromSecret(secretKey);
    savedAccounts.push({ secretKey, summary });
    activeAccountPubkey = summary.pubkey;
    return summary;
  });

  await page.exposeFunction('__irisNip07Logout', async () => {
    savedAccounts.length = 0;
    activeAccountPubkey = null;
    return null;
  });

  await page.exposeFunction('__irisNip07SetActive', async (pubkey: string) => {
    const account = savedAccounts.find((entry) => entry.summary.pubkey === pubkey);
    if (!account) {
      throw new Error('Nostr account not found');
    }
    activeAccountPubkey = account.summary.pubkey;
    return account.summary;
  });

  await page.exposeFunction('__irisNip07Remove', async (pubkey: string) => {
    const index = savedAccounts.findIndex((entry) => entry.summary.pubkey === pubkey);
    if (index < 0) {
      throw new Error('Nostr account not found');
    }
    savedAccounts.splice(index, 1);
    if (activeAccountPubkey === pubkey) {
      activeAccountPubkey = savedAccounts[0]?.summary.pubkey ?? null;
    }
    return {
      accounts: savedAccounts.map((account) => account.summary),
      activePubkey: activeAccountPubkey,
    };
  });

  await page.exposeFunction('__irisNip07Export', async (pubkey: string) => {
    const account = savedAccounts.find((entry) => entry.summary.pubkey === pubkey);
    if (!account) {
      throw new Error('Nostr account not found');
    }
    return nip19.nsecEncode(account.secretKey);
  });

  function requireActiveSecretKey() {
    const account = activeAccount();
    if (!account) {
      return null;
    }
    return account.secretKey;
  }

  await page.exposeFunction('__irisTakeNip07PermissionPrompt', async () => {
    return pendingPermissionPrompts.shift() ?? null;
  });

  await page.exposeFunction(
    '__irisRespondNip07PermissionPrompt',
    async (
      requestId: string,
      decision: 'deny' | 'allowSession' | 'allowAlways' | 'blockSite',
    ) => {
      const resolve = permissionPromptWaiters.get(requestId);
      if (!resolve) {
        throw new Error('Permission prompt was no longer pending');
      }
      permissionPromptWaiters.delete(requestId);
      resolve(decision);
      return null;
    },
  );

  await page.exposeFunction('__irisNip07HandleRequest', async (request: {
    method?: string;
    params?: any;
    origin?: string;
  }) => {
    const method = request?.method ?? '';
    const params = request?.params ?? {};
    const origin = request?.origin ?? 'tauri://localhost';

    if (method === 'getPublicKey') {
      const activeSecretKey = requireActiveSecretKey();
      if (!activeSecretKey) {
        return { result: null, error: 'No Nostr account signed in' };
      }
      if (!(await requestPermission(origin, method))) {
        return { result: null, error: 'Permission denied' };
      }
      return { result: getPublicKey(activeSecretKey), error: null };
    }

    if (method === 'signEvent') {
      const activeSecretKey = requireActiveSecretKey();
      if (!activeSecretKey) {
        return { result: null, error: 'No Nostr account signed in' };
      }
      if (!(await requestPermission(origin, method))) {
        return { result: null, error: 'Permission denied' };
      }
      const event = params?.event;
      if (!event || typeof event !== 'object') {
        return { result: null, error: 'Missing event parameter' };
      }
      const signed = finalizeEvent({
        created_at: event.created_at,
        kind: event.kind,
        tags: event.tags,
        content: event.content,
      }, activeSecretKey);
      return { result: signed, error: null };
    }

    if (method === 'getRelays') {
      return { result: {}, error: null };
    }

    if (method === 'nip04.encrypt') {
      const activeSecretKey = requireActiveSecretKey();
      if (!activeSecretKey) {
        return { result: null, error: 'No Nostr account signed in' };
      }
      if (!(await requestPermission(origin, method))) {
        return { result: null, error: 'Permission denied' };
      }
      const pubkey = typeof params?.pubkey === 'string' ? params.pubkey : '';
      const plaintext = typeof params?.plaintext === 'string' ? params.plaintext : '';
      if (!pubkey) {
        return { result: null, error: 'Missing pubkey parameter' };
      }
      if (!plaintext) {
        return { result: null, error: 'Missing plaintext parameter' };
      }
      try {
        return { result: await nip04.encrypt(activeSecretKey, pubkey, plaintext), error: null };
      } catch (error) {
        return { result: null, error: error instanceof Error ? error.message : String(error) };
      }
    }

    if (method === 'nip04.decrypt') {
      const activeSecretKey = requireActiveSecretKey();
      if (!activeSecretKey) {
        return { result: null, error: 'No Nostr account signed in' };
      }
      if (!(await requestPermission(origin, method))) {
        return { result: null, error: 'Permission denied' };
      }
      const pubkey = typeof params?.pubkey === 'string' ? params.pubkey : '';
      const ciphertext = typeof params?.ciphertext === 'string' ? params.ciphertext : '';
      if (!pubkey) {
        return { result: null, error: 'Missing pubkey parameter' };
      }
      if (!ciphertext) {
        return { result: null, error: 'Missing ciphertext parameter' };
      }
      try {
        return { result: await nip04.decrypt(activeSecretKey, pubkey, ciphertext), error: null };
      } catch (error) {
        return { result: null, error: error instanceof Error ? error.message : String(error) };
      }
    }

    if (method === 'nip44.encrypt') {
      const activeSecretKey = requireActiveSecretKey();
      if (!activeSecretKey) {
        return { result: null, error: 'No Nostr account signed in' };
      }
      if (!(await requestPermission(origin, method))) {
        return { result: null, error: 'Permission denied' };
      }
      const pubkey = typeof params?.pubkey === 'string' ? params.pubkey : '';
      const plaintext = typeof params?.plaintext === 'string' ? params.plaintext : '';
      if (!pubkey) {
        return { result: null, error: 'Missing pubkey parameter' };
      }
      if (!plaintext) {
        return { result: null, error: 'Missing plaintext parameter' };
      }
      try {
        return {
          result: await nip44.encrypt(plaintext, nip44.getConversationKey(activeSecretKey, pubkey)),
          error: null,
        };
      } catch (error) {
        return { result: null, error: error instanceof Error ? error.message : String(error) };
      }
    }

    if (method === 'nip44.decrypt') {
      const activeSecretKey = requireActiveSecretKey();
      if (!activeSecretKey) {
        return { result: null, error: 'No Nostr account signed in' };
      }
      if (!(await requestPermission(origin, method))) {
        return { result: null, error: 'Permission denied' };
      }
      const pubkey = typeof params?.pubkey === 'string' ? params.pubkey : '';
      const ciphertext = typeof params?.ciphertext === 'string' ? params.ciphertext : '';
      if (!pubkey) {
        return { result: null, error: 'Missing pubkey parameter' };
      }
      if (!ciphertext) {
        return { result: null, error: 'Missing ciphertext parameter' };
      }
      try {
        return {
          result: await nip44.decrypt(ciphertext, nip44.getConversationKey(activeSecretKey, pubkey)),
          error: null,
        };
      } catch (error) {
        return { result: null, error: error instanceof Error ? error.message : String(error) };
      }
    }

    return { result: null, error: `Unknown method: ${method}` };
  });

  await page.addInitScript(() => {
    (window as any).__IRIS_BROWSER_TAURI_MOCK__ = true;
    (window as any).__irisClipboardText = '';
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: {
        writeText: async (text: string) => {
          (window as any).__irisClipboardText = text;
        },
        readText: async () => (window as any).__irisClipboardText ?? '',
      },
    });

    // Track invocations for assertions
    (window as any).__tauriInvocations = [] as Array<{ cmd: string; args: any }>;
    (window as any).__automationState = {
      enabled: false,
      port: null,
      shellReady: false,
      currentView: 'launcher',
      currentUrl: '',
      addressValue: '',
      canGoBack: false,
      canGoForward: false,
      showDropdown: false,
      childWebviewReady: false,
      childPageLoadState: 'idle',
      childPageLoadUrl: '',
      childDocumentTitle: '',
      childBodyText: '',
      childMediaSummary: '',
      childLastError: '',
      historyIndex: -1,
      historyLength: 0,
      windowInnerHeight: 0,
      windowOuterHeight: 0,
      toolbarHeight: 0,
      childBoundsTop: 0,
      childBoundsHeight: 0,
      childViewportWidth: 0,
      childViewportHeight: 0,
      pendingNip07PromptRequestId: '',
      pendingNip07PromptOrigin: '',
      pendingNip07PromptMethod: '',
    };
    (window as any).__irisAddressOwnerProfiles = {
      npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm: {
        display_name: 'Sirius Business Ltd',
        picture: 'https://blossom.primal.net/e0c717e8060b46a3f6c30017e4c8efef709a81fcd061f3231699b3d09f01434e.jpg',
      },
    };
    (window as any).__daemonNetworkSettings = {
      webrtc: true,
      multicast: false,
      bluetooth: false,
      nostrRelaysEnabled: true,
      blossomEnabled: true,
      maxMulticastPeers: 0,
      maxBluetoothPeers: 0,
      multicastGroup: '239.255.42.98',
      multicastPort: 48555,
      relayUrls: ['wss://relay.damus.io', 'ws://127.0.0.1:21417/ws'],
      blossomServers: [
        { url: 'https://upload.iris.to', read: false, write: true },
        { url: 'https://cdn.iris.to', read: true, write: false },
      ],
    };
    (window as any).__tauriInvokeErrors = {} as Record<string, string>;
    (window as any).__tauriInvokeResults = {} as Record<string, unknown>;
    (window as any).__pendingDeepLinks = (window as any).__pendingDeepLinks ?? [];
    const callbackStore = new Map<number, (...args: any[]) => void>();
    const eventListeners = new Map<string, Array<{ eventId: number; handlerId: number }>>();
    let nextCallbackId = 1;
    let nextEventId = 1;

    // Mutable in-memory history store — record_history_visit adds entries,
    // get_recent_history / search_history read from it.
    const historyStore: Array<{
      path: string; label: string; entry_type: string;
      npub?: string; tree_name?: string;
      visit_count: number; last_visited: number; first_visited: number;
    }> = [];
    (window as any).__historyStore = historyStore;

    function unregisterListener(event: string, eventId: number) {
      const listeners = eventListeners.get(event);
      if (!listeners) return;
      eventListeners.set(event, listeners.filter((listener) => listener.eventId !== eventId));
    }

    (window as any).__emitTauriEvent = (event: string, payload: any) => {
      const listeners = eventListeners.get(event) ?? [];
      for (const listener of listeners) {
        const callback = callbackStore.get(listener.handlerId);
        callback?.({
          event,
          id: listener.eventId,
          payload,
        });
      }
    };

    const ipc = {
      metadata: {
        currentWindow: { label: 'main' },
        currentWebview: { label: 'main' },
      },
      invoke(cmd: string, args: any) {
        (window as any).__tauriInvocations.push({ cmd, args });

        const forcedError = (window as any).__tauriInvokeErrors?.[cmd];
        if (forcedError) {
          return Promise.reject(forcedError);
        }

        if (Object.prototype.hasOwnProperty.call((window as any).__tauriInvokeResults ?? {}, cmd)) {
          return Promise.resolve((window as any).__tauriInvokeResults[cmd]);
        }

        // Return sensible defaults per command
        switch (cmd) {
          case 'plugin:event|listen': {
            const event = args?.event ?? '';
            const handlerId = args?.handler;
            const eventId = nextEventId++;
            const listeners = eventListeners.get(event) ?? [];
            listeners.push({ eventId, handlerId });
            eventListeners.set(event, listeners);
            return Promise.resolve(eventId);
          }
          case 'plugin:event|unlisten':
            unregisterListener(args?.event ?? '', args?.eventId);
            return Promise.resolve();
          case 'create_nip07_webview':
          case 'create_htree_webview':
          case 'clear_tree_root_cache':
          case 'close_webview':
          case 'navigate_webview':
          case 'webview_history':
          case 'reload_webview':
          case 'set_webview_bounds':
          case 'set_mobile_shell_overlay':
            return Promise.resolve();
          case 'automation_update_state':
            (window as any).__automationState = {
              ...(window as any).__automationState,
              ...(args?.snapshot ?? {}),
            };
            return Promise.resolve();
          case 'automation_get_state':
            return Promise.resolve((window as any).__automationState);
          case 'automation_shutdown':
            return Promise.resolve();
          case 'get_daemon_transport_settings':
            return Promise.resolve((window as any).__daemonNetworkSettings);
          case 'update_daemon_transport_settings':
            (window as any).__daemonNetworkSettings = {
              ...((window as any).__daemonNetworkSettings ?? {}),
              ...(args?.settings ?? {}),
            };
            return Promise.resolve((window as any).__daemonNetworkSettings);
          case 'get_daemon_network_settings':
            return Promise.resolve((window as any).__daemonNetworkSettings);
          case 'update_daemon_network_settings':
            (window as any).__daemonNetworkSettings = {
              ...((window as any).__daemonNetworkSettings ?? {}),
              ...(args?.settings ?? {}),
            };
            return Promise.resolve((window as any).__daemonNetworkSettings);
          case 'deep_link_frontend_ready': {
            const pending = Array.isArray((window as any).__pendingDeepLinks)
              ? [...(window as any).__pendingDeepLinks]
              : [];
            (window as any).__pendingDeepLinks = [];
            return Promise.resolve(pending);
          }
          case 'record_history_visit': {
            const now = Date.now();
            const existing = historyStore.find(e => e.path === args?.path);
            if (existing) {
              existing.visit_count++;
              existing.last_visited = now;
              existing.label = args?.label ?? existing.label;
            } else {
              historyStore.push({
                path: args?.path ?? '',
                label: args?.label ?? '',
                entry_type: args?.entry_type ?? 'web',
                npub: args?.npub,
                tree_name: args?.tree_name,
                visit_count: 1,
                last_visited: now,
                first_visited: now,
              });
            }
            return Promise.resolve();
          }
          case 'get_htree_server_url':
            return Promise.resolve('http://127.0.0.1:21417');
          case 'webview_current_url':
            return Promise.resolve('about:blank');
          case 'get_recent_history': {
            const sorted = [...historyStore].sort((a, b) => b.last_visited - a.last_visited);
            return Promise.resolve(sorted.slice(0, args?.limit ?? 20));
          }
          case 'search_history': {
            const query = (args?.query ?? '').toLowerCase();
            const limit = args?.limit ?? 10;
            const matches = historyStore
              .filter(e => e.label.toLowerCase().includes(query) || e.path.toLowerCase().includes(query))
              .slice(0, limit)
              .map(entry => ({ entry, score: 5.0 }));
            return Promise.resolve(matches);
          }
          case 'delete_history_entry': {
            const idx = historyStore.findIndex(e => e.path === args?.path);
            if (idx >= 0) { historyStore.splice(idx, 1); return Promise.resolve(true); }
            return Promise.resolve(false);
          }
          case 'clear_history':
            historyStore.length = 0;
            return Promise.resolve();
          case 'get_nip07_account':
            return (window as any).__irisNip07GetAccount();
          case 'list_nip07_accounts':
            return (window as any).__irisNip07ListAccounts();
          case 'login_nip07_account':
            return (window as any).__irisNip07Login(args?.secret ?? '');
          case 'generate_nip07_account':
            return (window as any).__irisNip07Generate();
          case 'logout_nip07_account':
            return (window as any).__irisNip07Logout();
          case 'set_active_nip07_account':
            return (window as any).__irisNip07SetActive(args?.pubkey ?? '');
          case 'remove_nip07_account':
            return (window as any).__irisNip07Remove(args?.pubkey ?? '');
          case 'export_nip07_account_secret':
            return (window as any).__irisNip07Export(args?.pubkey ?? '');
          case 'take_nip07_permission_prompt':
            return (window as any).__irisTakeNip07PermissionPrompt();
          case 'respond_nip07_permission_prompt':
            return (window as any).__irisRespondNip07PermissionPrompt(
              args?.requestId ?? '',
              args?.decision ?? 'deny',
            );
          case 'nip07_request':
            return (window as any).__irisNip07HandleRequest({
              method: args?.method,
              params: args?.params ?? {},
              origin: args?.origin ?? 'tauri://localhost',
            });
          default:
            return Promise.resolve(null);
        }
      },
      transformCallback(callback: Function, once: boolean) {
        const id = nextCallbackId++;
        callbackStore.set(id, (...cbArgs: any[]) => {
          callback(...cbArgs);
          if (once) {
            callbackStore.delete(id);
          }
        });
        return id;
      },
      unregisterCallback(id: number) {
        callbackStore.delete(id);
      },
      convertFileSrc(path: string) {
        return path;
      },
    };

    Object.defineProperty(window, '__TAURI_INTERNALS__', {
      value: ipc,
      writable: false,
      configurable: true,
    });

    Object.defineProperty(window, '__TAURI_EVENT_PLUGIN_INTERNALS__', {
      value: {
        unregisterListener,
      },
      writable: false,
      configurable: true,
    });

    if (!(window as any).nostr) {
      const callNip07 = async (method: string, params: Record<string, unknown> = {}) => {
        const result = await ipc.invoke('nip07_request', {
          method,
          params,
          origin: 'tauri://localhost',
        });
        if (result?.error) {
          throw new Error(result.error);
        }
        return result?.result;
      };

      (window as any).nostr = {
        async getPublicKey() {
          return callNip07('getPublicKey');
        },
        async signEvent(event: {
          created_at: number;
          kind: number;
          tags: string[][];
          content: string;
        }) {
          return callNip07('signEvent', { event });
        },
        async getRelays() {
          return callNip07('getRelays');
        },
        nip04: {
          async encrypt(pubkey: string, plaintext: string) {
            return callNip07('nip04.encrypt', { pubkey, plaintext });
          },
          async decrypt(pubkey: string, ciphertext: string) {
            return callNip07('nip04.decrypt', { pubkey, ciphertext });
          },
        },
        nip44: {
          async encrypt(pubkey: string, plaintext: string) {
            return callNip07('nip44.encrypt', { pubkey, plaintext });
          },
          async decrypt(pubkey: string, ciphertext: string) {
            return callNip07('nip44.decrypt', { pubkey, ciphertext });
          },
        },
      };
    }
  });
}

export const test = base.extend<Fixtures>({
  renderLoopErrors: async ({}, use) => {
    await use(new Set<string>());
  },
  renderLoopGuard: [async ({ renderLoopErrors }, use) => {
    const before = new Set(renderLoopErrors);
    await use();
    const failures = new Set(
      Array.from(renderLoopErrors).filter((message) => !before.has(message)),
    );
    if (failures.size > 0) {
      throw new Error(formatRenderLoopFailures(failures));
    }
  }, { auto: true }],
  tauriPage: async ({ page, renderLoopErrors }, use) => {
    await mockTauriIPC(page);
    attachRenderLoopGuardToPage(page, renderLoopErrors);
    await use(page);
  },
});

export { expect } from '@playwright/test';

export function setupPageErrorHandler(page: Page) {
  page.on('pageerror', (err: Error) => {
    const msg = err.message;
    if (isRenderLoopMessage(msg)) return;
    if (!msg.includes('rate-limited') && !msg.includes('pow:') && !msg.includes('bits needed')) {
      console.log('Page error:', msg);
    }
  });
}

export async function disableOthersPool(_page: Page) {
  // Iris shell has no WebRTC pools; keep as no-op for shared test conventions.
}

export async function gotoHome(page: Page) {
  await page.goto('/');
  await disableOthersPool(page);
}

/** Get the list of Tauri IPC invocations recorded during the test. */
export async function getTauriInvocations(page: Page): Promise<Array<{ cmd: string; args: any }>> {
  return page.evaluate(() => (window as any).__tauriInvocations ?? []);
}

/** Get invocations for a specific command. */
export async function getInvocationsFor(page: Page, cmd: string): Promise<Array<{ cmd: string; args: any }>> {
  const all = await getTauriInvocations(page);
  return all.filter((i) => i.cmd === cmd);
}

export async function emitTauriEvent(page: Page, event: string, payload: unknown): Promise<void> {
  await page.evaluate(([name, data]) => {
    (window as any).__emitTauriEvent?.(name, data);
  }, [event, payload]);
}

export async function failTauriCommand(page: Page, cmd: string, message: string): Promise<void> {
  await page.evaluate(([name, error]) => {
    (window as any).__tauriInvokeErrors = {
      ...((window as any).__tauriInvokeErrors ?? {}),
      [name]: error,
    };
  }, [cmd, message]);
}

export async function setTauriCommandResult(page: Page, cmd: string, result: unknown): Promise<void> {
  await page.evaluate(([name, value]) => {
    (window as any).__tauriInvokeResults = {
      ...((window as any).__tauriInvokeResults ?? {}),
      [name]: value,
    };
  }, [cmd, result]);
}

export async function getAutomationState(page: Page): Promise<any> {
  return page.evaluate(() => (window as any).__automationState);
}
