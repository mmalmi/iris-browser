import { writable } from 'svelte/store';
import { cacheBookmarkIcon } from '../lib/tauri';
import {
  cloneBookmarks,
  defaultFavoriteApps,
  isRemoteIconUrl,
  matchesPwaIdentity,
  normalizeBookmark,
  normalizeBookmarks,
  type AppBookmark,
} from '../lib/apps';

const STORAGE_KEY = 'iris:apps';

function loadApps(): AppBookmark[] {
  if (typeof localStorage === 'undefined') return cloneBookmarks(defaultFavoriteApps);
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (!stored) return cloneBookmarks(defaultFavoriteApps);
    const parsed = JSON.parse(stored);
    if (!Array.isArray(parsed)) return cloneBookmarks(defaultFavoriteApps);

    const normalized = normalizeBookmarks(parsed as AppBookmark[]);
    if (JSON.stringify(parsed) !== JSON.stringify(normalized)) {
      saveApps(normalized);
    }
    return normalized;
  } catch {
    return cloneBookmarks(defaultFavoriteApps);
  }
}

function saveApps(apps: AppBookmark[]) {
  if (typeof localStorage === 'undefined') return;
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(apps));
  } catch {
    // Ignore storage errors
  }
}

function createAppsStore() {
  const initialApps = loadApps();
  const { subscribe, set, update } = writable<AppBookmark[]>(initialApps);
  let currentApps = initialApps;
  const pendingIconCacheKeys = new Set<string>();

  subscribe((apps) => {
    currentApps = apps;
  });

  function cacheKeyFor(app: AppBookmark): string | null {
    if (!isRemoteIconUrl(app.icon)) return null;
    return [
      app.url,
      app.icon ?? '',
      app.sourceUrl ?? '',
      app.sourceManifestUrl ?? '',
    ].join('\n');
  }

  async function cacheRemoteIconForApp(app: AppBookmark) {
    const remoteIcon = app.icon;
    if (!isRemoteIconUrl(remoteIcon)) return;

    const cacheKey = cacheKeyFor(app);
    if (!cacheKey || pendingIconCacheKeys.has(cacheKey)) return;
    pendingIconCacheKeys.add(cacheKey);

    try {
      const cachedIcon = await cacheBookmarkIcon({
        sourceUrl: app.sourceUrl,
        sourceManifestUrl: app.sourceManifestUrl,
        iconUrl: remoteIcon,
      });
      update((apps) => {
        const existingIndex = apps.findIndex((existing) =>
          existing.url === app.url || matchesPwaIdentity(existing, app),
        );
        if (existingIndex < 0) return apps;

        const existing = apps[existingIndex];
        if (existing.icon !== remoteIcon) return apps;

        const nextApps = [...apps];
        nextApps[existingIndex] = {
          ...existing,
          icon: cachedIcon ?? undefined,
        };
        saveApps(nextApps);
        return nextApps;
      });
    } catch (error) {
      console.warn('[Iris] failed to cache bookmark icon:', error);
      update((apps) => {
        const existingIndex = apps.findIndex((existing) =>
          existing.url === app.url || matchesPwaIdentity(existing, app),
        );
        if (existingIndex < 0) return apps;

        const existing = apps[existingIndex];
        if (existing.icon !== remoteIcon) return apps;

        const nextApps = [...apps];
        nextApps[existingIndex] = {
          ...existing,
          icon: undefined,
        };
        saveApps(nextApps);
        return nextApps;
      });
    } finally {
      pendingIconCacheKeys.delete(cacheKey);
    }
  }

  function cacheRemoteIcons() {
    for (const app of currentApps) {
      void cacheRemoteIconForApp(app);
    }
  }

  return {
    subscribe,

    add(app: AppBookmark) {
      const normalizedApp = normalizeBookmark(app);
      update((apps) => {
        const existingIndex = apps.findIndex((existing) =>
          existing.url === normalizedApp.url || matchesPwaIdentity(existing, normalizedApp),
        );
        if (existingIndex >= 0) {
          const newApps = [...apps];
          newApps[existingIndex] = {
            ...newApps[existingIndex],
            ...normalizedApp,
            addedAt: newApps[existingIndex].addedAt,
          };
          saveApps(newApps);
          return newApps;
        }
        const newApps = [...apps, normalizedApp];
        saveApps(newApps);
        return newApps;
      });

      void cacheRemoteIconForApp(normalizedApp);
    },

    remove(url: string) {
      update((apps) => {
        const newApps = apps.filter((a) => a.url !== url);
        saveApps(newApps);
        return newApps;
      });
    },

    clear() {
      set([]);
      saveApps([]);
    },

    cacheRemoteIcons,
  };
}

export const appsStore = createAppsStore();
