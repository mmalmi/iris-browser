import {
  test,
  expect,
  emitTauriEvent,
  getAutomationState,
  getInvocationsFor,
  gotoHome,
  setupPageErrorHandler,
} from './fixtures';

async function openHome(page: import('@playwright/test').Page) {
  setupPageErrorHandler(page);
  await gotoHome(page);
}

test.describe('Automation Bridge', () => {
  test('pending deep links open through the normal shell navigation path on boot', async ({ tauriPage: page }) => {
    await page.addInitScript(() => {
      (window as any).__pendingDeepLinks = ['htree://self/video/index.html?autoplay=1'];
    });

    await openHome(page);

    await expect.poll(async () => {
      const state = await getAutomationState(page);
      return state.currentUrl;
    }).toBe('htree://self/video/index.html?autoplay=1');

    const createCalls = await getInvocationsFor(page, 'create_htree_webview');
    expect(createCalls).toHaveLength(1);
    expect(createCalls[0].args.host).toBe('self');
    expect(createCalls[0].args.treename).toBe('video');
    expect(createCalls[0].args.path).toBe('/index.html');
    expect(createCalls[0].args.query).toBe('autoplay=1');
  });

  test('startup deep links can reach a loaded htree page state', async ({ tauriPage: page }) => {
    const url = 'htree://self/video/index.html?autoplay=1';
    await page.addInitScript((deepLinkUrl) => {
      (window as any).__pendingDeepLinks = [deepLinkUrl];
    }, url);

    await openHome(page);

    await expect.poll(async () => {
      const state = await getAutomationState(page);
      return state.currentUrl;
    }).toBe(url);

    const createCalls = await getInvocationsFor(page, 'create_htree_webview');
    expect(createCalls).toHaveLength(1);
    expect(createCalls[0].args.host).toBe('self');
    expect(createCalls[0].args.treename).toBe('video');
    expect(createCalls[0].args.path).toBe('/index.html');

    await emitTauriEvent(page, 'child-webview-page-load', {
      label: 'content',
      url,
      event: 'started',
    });
    await emitTauriEvent(page, 'child-webview-diagnostic', {
      label: 'content',
      url,
      source: 'load',
      title: 'Iris Video',
      bodyText: 'Video feed loaded',
      mediaSummary: 'thumbs=4/4 visible=3 videos=1/1',
      error: null,
    });
    await emitTauriEvent(page, 'child-webview-page-load', {
      label: 'content',
      url,
      event: 'finished',
    });

    await expect.poll(async () => {
      const state = await getAutomationState(page);
      return {
        currentUrl: state.currentUrl,
        childPageLoadState: state.childPageLoadState,
        childPageLoadUrl: state.childPageLoadUrl,
        childDocumentTitle: state.childDocumentTitle,
        childBodyText: state.childBodyText,
        childMediaSummary: state.childMediaSummary,
      };
    }).toEqual({
      currentUrl: url,
      childPageLoadState: 'finished',
      childPageLoadUrl: url,
      childDocumentTitle: 'Iris Video',
      childBodyText: 'Video feed loaded',
      childMediaSummary: 'thumbs=4/4 visible=3 videos=1/1',
    });
  });

  test('publishes shell state snapshots', async ({ tauriPage: page }) => {
    await openHome(page);

    await page.waitForFunction(() => {
      return (window as any).__automationState?.shellReady === true;
    });

    await expect.poll(async () => {
      const state = await getAutomationState(page);
      return {
        shellReady: state.shellReady,
        currentView: state.currentView,
        canGoBack: state.canGoBack,
        historyIndex: state.historyIndex,
      };
    }).toEqual({
      shellReady: true,
      currentView: 'launcher',
      canGoBack: false,
      historyIndex: -1,
    });
  });

  test('open_url command drives normal navigation flow', async ({ tauriPage: page }) => {
    await openHome(page);
    await page.waitForFunction(() => (window as any).__automationState?.shellReady === true);

    await emitTauriEvent(page, 'automation-command', {
      action: 'open_url',
      url: 'https://files.iris.to',
    });

    await expect.poll(async () => {
      const state = await getAutomationState(page);
      return state.currentUrl;
    }).toBe('https://files.iris.to');

    const createCalls = await getInvocationsFor(page, 'create_nip07_webview');
    expect(createCalls).toHaveLength(1);
    expect(createCalls[0].args.url).toBe('https://files.iris.to');

    const state = await getAutomationState(page);
    expect(state.currentView).toBe('webview');
    expect(state.canGoBack).toBe(true);
    expect(state.historyIndex).toBe(0);
  });

  test('child page-load and diagnostic events flow into automation state', async ({ tauriPage: page }) => {
    await openHome(page);
    await page.waitForFunction(() => (window as any).__automationState?.shellReady === true);

    await emitTauriEvent(page, 'child-webview-page-load', {
      label: 'content',
      url: 'htree://self/video',
      event: 'started',
    });
    await emitTauriEvent(page, 'child-webview-diagnostic', {
      label: 'content',
      url: 'htree://self/video',
      source: 'load',
      title: 'Iris Video',
      bodyText: 'Search videos',
      mediaSummary: 'thumbs=3/4 visible=2 videos=1/1',
      error: null,
    });
    await emitTauriEvent(page, 'child-webview-page-load', {
      label: 'content',
      url: 'htree://self/video',
      event: 'finished',
    });

    await expect.poll(async () => {
      const state = await getAutomationState(page);
      return {
        childPageLoadState: state.childPageLoadState,
        childPageLoadUrl: state.childPageLoadUrl,
        childDocumentTitle: state.childDocumentTitle,
        childBodyText: state.childBodyText,
        childMediaSummary: state.childMediaSummary,
      };
    }).toEqual({
      childPageLoadState: 'finished',
      childPageLoadUrl: 'htree://self/video',
      childDocumentTitle: 'Iris Video',
      childBodyText: 'Search videos',
      childMediaSummary: 'thumbs=3/4 visible=2 videos=1/1',
    });
  });

  test('home and reload commands reuse existing shell actions', async ({ tauriPage: page }) => {
    await openHome(page);
    await page.waitForFunction(() => (window as any).__automationState?.shellReady === true);

    await emitTauriEvent(page, 'automation-command', {
      action: 'open_url',
      url: 'https://video.iris.to',
    });

    await expect.poll(async () => {
      const state = await getAutomationState(page);
      return state.currentView;
    }).toBe('webview');

    await emitTauriEvent(page, 'automation-command', { action: 'reload' });
    await expect.poll(async () => {
      const reloadCalls = await getInvocationsFor(page, 'reload_webview');
      return reloadCalls.length;
    }).toBe(1);

    await emitTauriEvent(page, 'automation-command', { action: 'home' });
    await expect.poll(async () => {
      const state = await getAutomationState(page);
      return state.currentView;
    }).toBe('launcher');

    const closeCalls = await getInvocationsFor(page, 'close_webview');
    expect(closeCalls.length).toBeGreaterThan(0);
  });

  test('shutdown command uses the native shutdown invoke', async ({ tauriPage: page }) => {
    await openHome(page);
    await page.waitForFunction(() => (window as any).__automationState?.shellReady === true);

    await emitTauriEvent(page, 'automation-command', { action: 'shutdown' });

    await expect.poll(async () => {
      const shutdownCalls = await getInvocationsFor(page, 'automation_shutdown');
      return shutdownCalls.length;
    }).toBe(1);
  });
});
