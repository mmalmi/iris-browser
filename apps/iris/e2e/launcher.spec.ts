import {
  test,
  expect,
  emitTauriEvent,
  getInvocationsFor,
  setupPageErrorHandler,
  gotoHome,
} from './fixtures';
import { distributedOwner } from '../src/lib/apps';

async function openHome(page: import('@playwright/test').Page) {
  setupPageErrorHandler(page);
  await gotoHome(page);
}

function suggestionButton(page: import('@playwright/test').Page, slug: string) {
  return page.getByTestId(`suggestion-open-${slug}`);
}

test.describe('App Launcher', () => {
  test('shows launcher on startup', async ({ tauriPage: page }) => {
    await openHome(page);

    const favourites = page.locator('section').filter({
      has: page.getByRole('heading', { name: 'Favourites' }),
    });
    const suggestions = page.locator('section').filter({
      has: page.getByRole('heading', { name: 'Suggestions' }),
    });

    await expect(page.getByRole('heading', { name: 'Favourites' })).toBeVisible();
    await expect(page.getByRole('heading', { name: 'Suggestions' })).toBeVisible();
    await expect(page.getByText('No favourites yet')).toBeVisible();

    await expect(favourites.getByText('Iris Files')).not.toBeVisible();
    await expect(favourites.getByText('Iris Video')).not.toBeVisible();
    await expect(favourites.getByText('Iris Docs')).not.toBeVisible();
    await expect(favourites.getByText('Iris Git')).not.toBeVisible();
    await expect(favourites.getByText('Iris Maps')).not.toBeVisible();
    await expect(favourites.getByText('Iris Boards')).not.toBeVisible();
    await expect(favourites.getByText('Iris Social')).not.toBeVisible();
    await expect(favourites.getByText('Iris Chat')).not.toBeVisible();
    await expect(favourites.getByText('Iris Meet')).not.toBeVisible();

    await expect(suggestions.getByText('Iris Files')).toBeVisible();
    await expect(suggestions.getByText('Iris Video')).toBeVisible();
    await expect(suggestions.getByText('Iris Docs')).toBeVisible();
    await expect(suggestions.getByText('Iris Git')).toBeVisible();
    await expect(suggestions.getByText('Iris Maps')).toBeVisible();
    await expect(suggestions.getByText('Iris Boards')).toBeVisible();
    await expect(suggestions.getByText('Iris Social')).toBeVisible();
    await expect(suggestions.getByText('Iris Chat')).toBeVisible();
    await expect(suggestions.getByText('Iris Meet')).toBeVisible();
    await expect(suggestions.getByText('hashtree.cc')).toBeVisible();
    await expect(suggestions.getByText('files', { exact: true })).not.toBeVisible();
    await expect(suggestions.getByText('video', { exact: true })).not.toBeVisible();
    await expect(suggestions.getByText('docs', { exact: true })).not.toBeVisible();
    await expect(suggestions.getByText('git', { exact: true })).not.toBeVisible();
    await expect(suggestions.getByText('maps', { exact: true })).not.toBeVisible();
    await expect(suggestions.getByText('boards', { exact: true })).not.toBeVisible();
    await expect(suggestions.getByText('iris.to', { exact: true })).not.toBeVisible();

    const hashtreeSuggestion = page.getByTestId('suggestion-card-hashtree-cc');
    await expect(hashtreeSuggestion.locator('img')).toHaveAttribute('src', /hashtree-cc-favicon\.svg$/);
    await expect(suggestions).not.toContainText(distributedOwner);
  });

  test('clicking suggestion triggers webview creation', async ({ tauriPage: page }) => {
    await openHome(page);

    await suggestionButton(page, 'iris-files').click();

    await expect.poll(async () => (await getInvocationsFor(page, 'create_htree_webview')).length).toBe(1);
    const invocations = await page.evaluate(() => (window as any).__tauriInvocations);
    const cacheCalls = invocations.filter((i: any) => i.cmd === 'cache_tree_root');
    const clearCalls = invocations.filter((i: any) => i.cmd === 'clear_tree_root_cache');
    const createCalls = invocations.filter((i: any) => i.cmd === 'create_htree_webview');

    expect(cacheCalls.length).toBe(0);
    expect(clearCalls.length).toBe(0);
    expect(createCalls.length).toBe(1);
    expect(createCalls[0].args.host).toBe(distributedOwner);
    expect(createCalls[0].args.nhash).toBeNull();
    expect(createCalls[0].args.npub).toBe(distributedOwner);
    expect(createCalls[0].args.treename).toBe('files');
    expect(createCalls[0].args.path).toBe('/index.html');
  });

  test('typing a built-in htree url clears stale cache before opening', async ({ tauriPage: page }) => {
    await openHome(page);

    const addressInput = page.locator('[data-testid="address-bar"] input');
    await addressInput.click();
    await addressInput.fill(`htree://${distributedOwner}/video`);
    await addressInput.press('Enter');

    await expect.poll(async () => (await getInvocationsFor(page, 'create_htree_webview')).length).toBe(1);
    const invocations = await page.evaluate(() => (window as any).__tauriInvocations);
    const cacheCalls = invocations.filter((i: any) => i.cmd === 'cache_tree_root');
    const clearCalls = invocations.filter((i: any) => i.cmd === 'clear_tree_root_cache');
    const createCalls = invocations.filter((i: any) => i.cmd === 'create_htree_webview');

    expect(cacheCalls.length).toBe(0);
    expect(clearCalls.length).toBe(0);
    expect(createCalls.length).toBe(1);
    expect(createCalls[0].args.host).toBe(distributedOwner);
    expect(createCalls[0].args.treename).toBe('video');
    expect(createCalls[0].args.path).toBe('/');
  });

  test('clicking Iris Boards suggestion opens boards tree', async ({ tauriPage: page }) => {
    await openHome(page);

    await suggestionButton(page, 'iris-boards').click();

    await expect.poll(async () => (await getInvocationsFor(page, 'create_htree_webview')).length).toBe(1);
    const invocations = await page.evaluate(() => (window as any).__tauriInvocations);
    const createCalls = invocations.filter((i: any) => i.cmd === 'create_htree_webview');
    expect(createCalls.length).toBe(1);
    expect(createCalls[0].args.host).toBe(distributedOwner);
    expect(createCalls[0].args.treename).toBe('boards');
    expect(createCalls[0].args.path).toBe('/index.html');
  });

  test('clicking Iris Git suggestion opens git tree', async ({ tauriPage: page }) => {
    await openHome(page);

    await suggestionButton(page, 'iris-git').click();

    await expect.poll(async () => (await getInvocationsFor(page, 'create_htree_webview')).length).toBe(1);
    const invocations = await page.evaluate(() => (window as any).__tauriInvocations);
    const createCalls = invocations.filter((i: any) => i.cmd === 'create_htree_webview');
    expect(createCalls.length).toBe(1);
    expect(createCalls[0].args.host).toBe(distributedOwner);
    expect(createCalls[0].args.treename).toBe('git');
    expect(createCalls[0].args.path).toBe('/index.html');
  });

  test('clicking Iris Social suggestion opens the iris-client-site tree once', async ({ tauriPage: page }) => {
    await openHome(page);

    const socialSuggestion = page.getByTestId('suggestion-open-iris-social');
    await expect(socialSuggestion).toHaveCount(1);
    await socialSuggestion.click();

    await expect.poll(async () => (await getInvocationsFor(page, 'create_htree_webview')).length).toBe(1);

    const nip07Calls = await getInvocationsFor(page, 'create_nip07_webview');
    const createCalls = await getInvocationsFor(page, 'create_htree_webview');

    expect(nip07Calls.length).toBe(0);
    expect(createCalls.length).toBe(1);
    expect(createCalls[0].args.host).toBe(distributedOwner);
    expect(createCalls[0].args.nhash).toBeNull();
    expect(createCalls[0].args.npub).toBe(distributedOwner);
    expect(createCalls[0].args.treename).toBe('iris-client-site');
    expect(createCalls[0].args.path).toBe('/index.html');
  });

  test('blank built-in suggestion load clears stale cache and recreates the webview once', async ({ tauriPage: page }) => {
    await openHome(page);

    await suggestionButton(page, 'iris-video').click();

    await emitTauriEvent(page, 'child-webview-page-load', {
      label: 'content',
      url: `htree://${distributedOwner}/video`,
      event: 'finished',
    });

    await expect.poll(async () => {
      return {
        cacheCalls: (await getInvocationsFor(page, 'cache_tree_root')).length,
        clearCalls: (await getInvocationsFor(page, 'clear_tree_root_cache')).length,
        closeCalls: (await getInvocationsFor(page, 'close_webview')).length,
        createCalls: (await getInvocationsFor(page, 'create_htree_webview')).length,
      };
    }).toEqual({
      cacheCalls: 0,
      clearCalls: 0,
      closeCalls: 1,
      createCalls: 2,
    });

    const createCalls = await getInvocationsFor(page, 'create_htree_webview');
    expect(createCalls[1].args.host).toBe(distributedOwner);
    expect(createCalls[1].args.treename).toBe('video');
    expect(createCalls[1].args.path).toBe('/index.html');
  });

  test('stalled htree suggestion load recreates the webview with plain loopback transport', async ({ tauriPage: page }) => {
    await openHome(page);

    await suggestionButton(page, 'iris-video').click();

    await expect.poll(async () => {
      return {
        closeCalls: (await getInvocationsFor(page, 'close_webview')).length,
        createCalls: (await getInvocationsFor(page, 'create_htree_webview')).length,
      };
    }, { timeout: 10000 }).toEqual({
      closeCalls: 1,
      createCalls: 2,
    });

    const createCalls = await getInvocationsFor(page, 'create_htree_webview');
    expect(createCalls[0].args.preferPlainLoopbackHost).toBe(false);
    expect(createCalls[1].args.host).toBe(distributedOwner);
    expect(createCalls[1].args.treename).toBe('video');
    expect(createCalls[1].args.path).toBe('/index.html');
    expect(createCalls[1].args.preferPlainLoopbackHost).toBe(true);
  });

  test('dismissed suggestions stay hidden after reload', async ({ tauriPage: page }) => {
    await openHome(page);

    const suggestions = page.locator('section').filter({
      has: page.getByRole('heading', { name: 'Suggestions' }),
    });
    const gitSuggestion = page.getByTestId('suggestion-card-iris-git');

    await expect(gitSuggestion).toBeVisible();
    await page.getByTestId('suggestion-dismiss-iris-git').click();
    await expect(gitSuggestion).not.toBeVisible();

    await page.reload();
    await gotoHome(page);

    await expect(suggestions.getByText('Iris Git')).not.toBeVisible();
    await expect(page.getByRole('button', { name: 'Reset suggestions' })).toBeVisible();
  });

  test('reset suggestions restores dismissed suggestions', async ({ tauriPage: page }) => {
    await openHome(page);

    const suggestions = page.locator('section').filter({
      has: page.getByRole('heading', { name: 'Suggestions' }),
    });
    const boardsSuggestion = page.getByTestId('suggestion-card-iris-boards');

    await page.getByTestId('suggestion-dismiss-iris-boards').click();
    await expect(boardsSuggestion).not.toBeVisible();

    await page.getByRole('button', { name: 'Reset suggestions' }).click();

    await expect(suggestions.getByText('Iris Boards')).toBeVisible();
    await expect(page.getByRole('button', { name: 'Reset suggestions' })).not.toBeVisible();
  });

  test('favorites render a human-readable name instead of a raw npub label', async ({ tauriPage: page }) => {
    await openHome(page);
    await page.evaluate((owner) => {
      localStorage.setItem('iris:apps', JSON.stringify([
        {
          url: `htree://${owner}/video/index.html`,
          name: owner,
          addedAt: Date.now(),
        },
      ]));
    }, distributedOwner);
    await page.reload();
    await gotoHome(page);

    const favourites = page.locator('section').filter({
      has: page.getByRole('heading', { name: 'Favourites' }),
    });

    await expect(favourites.getByText('Iris Video')).toBeVisible();
    await expect(favourites).not.toContainText(distributedOwner);
  });

  test('caches remote favorite icons into htree on startup', async ({ tauriPage: page }) => {
    await page.addInitScript(() => {
      (window as any).__tauriInvokeResults = {
        ...((window as any).__tauriInvokeResults ?? {}),
        cache_bookmark_icon: 'htree://nhash1sample/icon.png',
      };
      localStorage.setItem('iris:apps', JSON.stringify([
        {
          url: 'https://example.com/app',
          name: 'Sample App',
          icon: 'https://example.com/icon.png',
          sourceUrl: 'https://example.com/app',
          sourceManifestUrl: 'https://example.com/manifest.webmanifest',
          addedAt: 123,
        },
      ]));
    });

    await gotoHome(page);

    await expect.poll(async () => (await getInvocationsFor(page, 'cache_bookmark_icon')).length).toBe(1);
    await expect.poll(async () => {
      const apps = await page.evaluate(() => JSON.parse(localStorage.getItem('iris:apps') ?? '[]'));
      return apps[0]?.icon ?? null;
    }).toBe('htree://nhash1sample/icon.png');
  });

  test('favorites fall back to initials when a saved icon fails to load', async ({ tauriPage: page }) => {
    await page.route('**/broken-jumble-icon.png', async (route) => {
      await route.fulfill({
        status: 200,
        contentType: 'text/html',
        body: '<!doctype html><title>broken</title>',
      });
    });
    await page.addInitScript(() => {
      localStorage.setItem('iris:apps', JSON.stringify([
        {
          url: 'https://jumble.social/',
          name: 'Jumble',
          icon: '/broken-jumble-icon.png',
          addedAt: 123,
        },
      ]));
    });

    await gotoHome(page);

    const favouriteIcon = page.getByTestId('favorite-icon-jumble');
    await expect.poll(async () => favouriteIcon.locator('img').count()).toBe(0);
    await expect(favouriteIcon).toContainText('J');
    await expect(favouriteIcon).toHaveClass(
      /(bg-orange-500|bg-blue-500|bg-green-500|bg-purple-500|bg-pink-500|bg-yellow-500|bg-red-500|bg-teal-500)/,
    );
  });

  test('add to favourites button works', async ({ tauriPage: page }) => {
    await openHome(page);
    await page.evaluate(() => localStorage.setItem('iris:apps', JSON.stringify([])));
    await page.reload();
    await gotoHome(page);

    const favourites = page.locator('section').filter({
      has: page.getByRole('heading', { name: 'Favourites' }),
    });
    const suggestions = page.locator('section').filter({
      has: page.getByRole('heading', { name: 'Suggestions' }),
    });

    await expect(page.getByText('No favourites yet')).toBeVisible();

    await page.getByTestId('suggestion-add-iris-files').click();

    await expect(page.getByText('No favourites yet')).not.toBeVisible();
    await expect(favourites.getByText('Iris Files')).toBeVisible();
    await expect(suggestions.getByText('Iris Files')).not.toBeVisible();
    await expect(page.getByTestId('favorite-icon-iris-files')).not.toHaveClass(
      /(bg-orange-500|bg-blue-500|bg-green-500|bg-purple-500|bg-pink-500|bg-yellow-500|bg-red-500|bg-teal-500)/,
    );
  });
});
