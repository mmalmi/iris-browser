import { writable } from 'svelte/store';

const STORAGE_KEY = 'iris:dismissed-suggestions';

function loadDismissedSuggestions(): string[] {
  if (typeof localStorage === 'undefined') return [];
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (!stored) return [];

    const parsed = JSON.parse(stored);
    if (!Array.isArray(parsed)) return [];

    return parsed.filter((value): value is string => typeof value === 'string');
  } catch {
    return [];
  }
}

function saveDismissedSuggestions(urls: string[]) {
  if (typeof localStorage === 'undefined') return;
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(urls));
  } catch {
    // Ignore storage errors
  }
}

function createDismissedSuggestionsStore() {
  const { subscribe, set, update } = writable<string[]>(loadDismissedSuggestions());

  return {
    subscribe,

    dismiss(url: string) {
      update((urls) => {
        if (urls.includes(url)) return urls;
        const nextUrls = [...urls, url];
        saveDismissedSuggestions(nextUrls);
        return nextUrls;
      });
    },

    reset() {
      set([]);
      saveDismissedSuggestions([]);
    },
  };
}

export const dismissedSuggestionsStore = createDismissedSuggestionsStore();
