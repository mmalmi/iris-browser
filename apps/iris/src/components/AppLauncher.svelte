<script lang="ts">
  import { bookmarkDisplayName, isRemoteIconUrl, suggestedApps, type AppBookmark } from '../lib/apps';
  import { appsStore } from '../stores/apps';
  import { dismissedSuggestionsStore } from '../stores/dismissedSuggestions';

  interface Props {
    onnavigate: (url: string) => void;
  }

  let { onnavigate }: Props = $props();
  const suggestions: readonly AppBookmark[] = suggestedApps;

  let favorites = $derived($appsStore);
  let dismissedSuggestions = $derived($dismissedSuggestionsStore);
  let visibleSuggestions = $derived(
    suggestions.filter(
      (app) => !favorites.some((favorite) => favorite.url === app.url) && !dismissedSuggestions.includes(app.url),
    ),
  );

  function openApp(app: AppBookmark) {
    onnavigate(app.url);
  }

  function removeFromFavorites(url: string) {
    appsStore.remove(url);
  }

  function addToFavorites(app: AppBookmark) {
    appsStore.add({ ...app, addedAt: Date.now() });
  }

  function dismissSuggestion(url: string) {
    dismissedSuggestionsStore.dismiss(url);
  }

  function resetSuggestions() {
    dismissedSuggestionsStore.reset();
  }

  function getInitial(name: string): string {
    return name.charAt(0).toUpperCase();
  }

  function getColor(name: string): string {
    const colors = [
      'bg-orange-500',
      'bg-blue-500',
      'bg-green-500',
      'bg-purple-500',
      'bg-pink-500',
      'bg-yellow-500',
      'bg-red-500',
      'bg-teal-500',
    ];
    return colors[name.charCodeAt(0) % colors.length];
  }

  function slugifyName(name: string): string {
    return name.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '');
  }

  function favoriteName(app: AppBookmark): string {
    return bookmarkDisplayName(app);
  }

  let failedIconUrls = $state<Set<string>>(new Set());

  function markIconFailed(iconUrl?: string) {
    if (!iconUrl || failedIconUrls.has(iconUrl)) return;
    const next = new Set(failedIconUrls);
    next.add(iconUrl);
    failedIconUrls = next;
  }

  function usableIcon(iconUrl?: string): string | null {
    if (!iconUrl || failedIconUrls.has(iconUrl) || isRemoteIconUrl(iconUrl)) return null;
    return iconUrl;
  }
</script>

<div class="flex-1 overflow-auto p-8 md:p-12">
  <div class="mx-auto max-w-4xl">
    <!-- Favourites -->
    <section class="mb-10">
      <h2 class="text-lg font-semibold text-text-1 mb-4">Favourites</h2>
      {#if favorites.length === 0}
        <p class="text-text-3 text-sm">No favourites yet. Add apps from suggestions below.</p>
      {:else}
        <div class="grid grid-cols-4 sm:grid-cols-6 md:grid-cols-8 gap-4">
          {#each favorites as app (app.url)}
            {@const displayName = favoriteName(app)}
            {@const iconUrl = usableIcon(app.icon)}
            <div class="group relative">
              <button
                class="w-full flex flex-col items-center gap-2"
                onclick={() => openApp(app)}
                data-testid={`favorite-${slugifyName(displayName)}`}
              >
                <div
                  class={`w-14 h-14 rounded-xl flex items-center justify-center text-white text-xl font-semibold shadow-lg hover:scale-105 transition-transform ${
                    iconUrl ? '' : getColor(displayName)
                  }`}
                  data-testid={`favorite-icon-${slugifyName(displayName)}`}
                >
                  {#if iconUrl}
                    <img
                      src={iconUrl}
                      alt=""
                      class="w-14 h-14 rounded-xl object-cover"
                      loading="lazy"
                      onerror={() => markIconFailed(iconUrl)}
                    />
                  {:else}
                    {getInitial(displayName)}
                  {/if}
                </div>
                <span class="text-xs text-text-2 truncate w-full text-center">{displayName}</span>
              </button>
              <button
                class="absolute -top-1 -right-1 w-5 h-5 rounded-full bg-surface-2 text-text-3 opacity-0 group-hover:opacity-100 transition-opacity flex items-center justify-center text-xs hover:bg-red-600 hover:text-white"
                onclick={(e) => { e.stopPropagation(); removeFromFavorites(app.url); }}
                title="Remove"
              >
                <span class="i-lucide-x text-xs"></span>
              </button>
            </div>
          {/each}
        </div>
      {/if}
    </section>

    <!-- Suggestions -->
    <section>
      <div class="mb-4 flex items-center justify-between gap-3">
        <h2 class="text-lg font-semibold text-text-1">Suggestions</h2>
        {#if dismissedSuggestions.length > 0}
          <button
            class="shrink-0 rounded-lg px-3 py-1.5 text-sm text-text-2 hover:bg-surface-2"
            onclick={resetSuggestions}
          >
            Reset suggestions
          </button>
        {/if}
      </div>
      {#if visibleSuggestions.length === 0}
        <p class="text-text-3 text-sm">No suggestions right now.</p>
      {:else}
        <div class="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3 2xl:grid-cols-4">
          {#each visibleSuggestions as app (app.url)}
            {@const iconUrl = usableIcon(app.icon)}
            <div
              class="group flex items-center gap-3 rounded-2xl bg-surface-1 px-3 py-3 transition-colors hover:bg-surface-2"
              data-testid={`suggestion-card-${slugifyName(app.name)}`}
            >
              <button
                class="flex min-w-0 flex-1 items-center gap-3 text-left"
                data-testid={`suggestion-open-${slugifyName(app.name)}`}
                onclick={() => openApp(app)}
              >
                <div class="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-surface-2">
                  {#if iconUrl}
                    <img
                      src={iconUrl}
                      alt=""
                      class="h-7 w-7 rounded-lg object-cover"
                      loading="lazy"
                      onerror={() => markIconFailed(iconUrl)}
                    />
                  {:else}
                    <span class="text-lg font-semibold text-text-2">{getInitial(app.name)}</span>
                  {/if}
                </div>
                <div class="min-w-0 flex-1">
                  <div class="truncate text-sm font-medium leading-tight text-text-1">
                    {app.name}
                  </div>
                </div>
              </button>
              <div class="ml-auto flex shrink-0 items-center gap-1">
                <button
                  class="rounded-lg p-1.5 hover:bg-surface-3"
                  data-testid={`suggestion-add-${slugifyName(app.name)}`}
                  onclick={(e) => { e.stopPropagation(); addToFavorites(app); }}
                  title="Add to favourites"
                >
                  <span class="i-lucide-plus text-text-3"></span>
                </button>
                <button
                  class="rounded-lg p-1.5 hover:bg-surface-3"
                  data-testid={`suggestion-dismiss-${slugifyName(app.name)}`}
                  onclick={(e) => { e.stopPropagation(); dismissSuggestion(app.url); }}
                  title="Dismiss suggestion"
                >
                  <span class="i-lucide-x text-text-3"></span>
                </button>
              </div>
            </div>
          {/each}
        </div>
      {/if}
    </section>
  </div>
</div>
