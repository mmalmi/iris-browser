import {
  test,
  expect,
  emitTauriEvent,
  failTauriCommand,
  getInvocationsFor,
  setupPageErrorHandler,
  gotoHome,
} from './fixtures';

async function openHome(page: import('@playwright/test').Page) {
  setupPageErrorHandler(page);
  await gotoHome(page);
}

test.describe('Navigation', () => {
  test('mobile chrome docks to the footer with back and more actions', async ({ tauriPage: page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await openHome(page);
    await expect(page.locator('input[placeholder="Search or enter address"]')).toBeVisible();

    const toolbarLayout = await page.evaluate(() => {
      const toolbar = document.querySelector<HTMLElement>('div[data-tauri-drag-region][data-testid="toolbar"]');
      const input = document.querySelector<HTMLInputElement>('input[placeholder="Search or enter address"]');
      const backButton = document.querySelector<HTMLButtonElement>('button[title="Back"]');
      const moreButton = document.querySelector<HTMLButtonElement>('button[title="More"]');
      const settingsButton = document.querySelector<HTMLButtonElement>('button[title="Settings"]');

      if (!toolbar || !input || !backButton || !moreButton) {
        throw new Error('mobile footer controls not found');
      }

      const toolbarRect = toolbar.getBoundingClientRect();
      const inputRect = input.getBoundingClientRect();

      return {
        viewportWidth: window.innerWidth,
        viewportHeight: window.innerHeight,
        toolbarLeft: toolbarRect.left,
        toolbarRight: toolbarRect.right,
        toolbarTop: toolbarRect.top,
        toolbarBottom: toolbarRect.bottom,
        inputRight: inputRect.right,
        hasSettingsButton: !!settingsButton,
      };
    });

    expect(toolbarLayout.toolbarLeft).toBeGreaterThanOrEqual(0);
    expect(toolbarLayout.toolbarRight).toBeLessThanOrEqual(toolbarLayout.viewportWidth);
    expect(toolbarLayout.toolbarTop).toBeGreaterThan(toolbarLayout.viewportHeight - 180);
    expect(toolbarLayout.toolbarBottom).toBeLessThanOrEqual(toolbarLayout.viewportHeight);
    expect(toolbarLayout.inputRight).toBeLessThanOrEqual(toolbarLayout.viewportWidth - 8);
    expect(toolbarLayout.hasSettingsButton).toBe(false);
  });

  test('mobile address field expands while editing', async ({ tauriPage: page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await openHome(page);
    await expect(page.locator('input[placeholder="Search or enter address"]')).toBeVisible();

    const before = await page.evaluate(() => {
      const addressBar = document.querySelector<HTMLElement>('[data-testid="address-bar"]');
      const moreButton = document.querySelector<HTMLButtonElement>('button[title="More"]');

      if (!addressBar || !moreButton) {
        throw new Error('mobile address bar not found');
      }

      return {
        addressWidth: addressBar.getBoundingClientRect().width,
        moreVisible: moreButton.offsetParent !== null,
      };
    });

    await page.locator('input[placeholder="Search or enter address"]').click();

    const after = await page.evaluate(() => {
      const addressBar = document.querySelector<HTMLElement>('[data-testid="address-bar"]');
      const moreButton = document.querySelector<HTMLButtonElement>('button[title="More"]');
      const backButton = document.querySelector<HTMLButtonElement>('button[title="Back"]');

      if (!addressBar) {
        throw new Error('mobile address bar not found after focus');
      }

      return {
        addressWidth: addressBar.getBoundingClientRect().width,
        moreVisible: !!moreButton && moreButton.offsetParent !== null,
        backVisible: !!backButton && backButton.offsetParent !== null,
      };
    });

    expect(before.moreVisible).toBe(true);
    expect(after.addressWidth).toBeGreaterThan(before.addressWidth + 40);
    expect(after.moreVisible).toBe(false);
    expect(after.backVisible).toBe(false);
  });

  test('address field disables autocorrect and capitalization helpers', async ({ tauriPage: page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await openHome(page);
    await expect(page.locator('input[placeholder="Search or enter address"]')).toBeVisible();

    const attributes = await page.evaluate(() => {
      const input = document.querySelector<HTMLInputElement>('input[placeholder="Search or enter address"]');
      if (!input) {
        throw new Error('address input not found');
      }

      return {
        autocorrect: input.getAttribute('autocorrect'),
        autocapitalize: input.getAttribute('autocapitalize'),
        autocomplete: input.getAttribute('autocomplete'),
        spellcheck: input.spellcheck,
      };
    });

    expect(attributes.autocorrect).toBe('off');
    expect(attributes.autocapitalize).toBe('none');
    expect(attributes.autocomplete).toBe('off');
    expect(attributes.spellcheck).toBe(false);
  });

  test('mobile native browser commands include the device scale factor', async ({ tauriPage: page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await openHome(page);

    const deviceScale = await page.evaluate(() => window.devicePixelRatio);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('https://example.com');
    await input.press('Enter');

    await expect.poll(async () => (await getInvocationsFor(page, 'set_webview_bounds')).length > 0).toBe(true);

    const createCalls = await getInvocationsFor(page, 'create_nip07_webview');
    const boundsCalls = await getInvocationsFor(page, 'set_webview_bounds');

    expect(createCalls[0]?.args?.scale).toBe(deviceScale);
    expect(boundsCalls.at(-1)?.args?.scale).toBe(deviceScale);
  });

  test('mobile browsing reserves footer space without shell passthrough', async ({ tauriPage: page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('https://example.com');
    await input.press('Enter');

    await expect.poll(async () => (await getInvocationsFor(page, 'set_webview_bounds')).length > 0).toBe(true);

    const shellLayout = await page.evaluate(() => {
      const root = document.body.firstElementChild as HTMLElement | null;
      const toolbar = document.querySelector<HTMLElement>('[data-testid="toolbar"]');
      if (!root || !toolbar) {
        throw new Error('shell root or toolbar not found');
      }

      return {
        toolbarBackground: getComputedStyle(toolbar).backgroundColor,
        viewportHeight: window.innerHeight,
        toolbarTop: toolbar.getBoundingClientRect().top,
      };
    });

    const overlayCalls = await getInvocationsFor(page, 'set_mobile_shell_overlay');
    const boundsCalls = await getInvocationsFor(page, 'set_webview_bounds');
    const lastBounds = boundsCalls.at(-1)?.args;
    const reservedBottom = shellLayout.viewportHeight - (lastBounds?.y ?? 0) - (lastBounds?.height ?? 0);

    expect(overlayCalls).toHaveLength(0);
    expect(lastBounds?.x).toBe(0);
    expect(lastBounds?.y).toBe(0);
    expect(lastBounds?.width).toBe(390);
    expect(lastBounds?.height).toBeLessThan(shellLayout.viewportHeight - 40);
    expect(reservedBottom).toBeGreaterThan(40);
    expect(reservedBottom).toBeLessThan(220);
    expect(shellLayout.toolbarBackground).not.toBe('rgba(0, 0, 0, 0)');
  });

  test('shows a real error when embedded page creation fails', async ({ tauriPage: page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await openHome(page);
    await failTauriCommand(page, 'create_nip07_webview', 'Mobile child webviews are not supported yet');

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('https://example.com');
    await input.press('Enter');

    await expect(page.getByTestId('webview-error')).toBeVisible();
    await expect(page.getByText('Embedded browsing is not available on this device yet')).toBeVisible();
    await expect(page.getByText('Iris uses child webviews for in-app pages, and the current mobile runtime does not provide them yet.')).toBeVisible();
  });

  test('ignores child media resource errors in shell chrome', async ({ tauriPage: page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('https://video.iris.to');
    await input.press('Enter');

    await emitTauriEvent(page, 'child-webview-diagnostic', {
      label: 'content',
      url: 'https://video.iris.to',
      source: 'resource-error',
      title: 'Iris Video',
      bodyText: 'Recent videos',
      mediaSummary: 'thumbs=3/4 visible=2 videos=1/1',
      error: 'img failed to load: https://video.iris.to/htree/thumbnail.webp',
    });

    await expect(page.getByTestId('webview-error')).toBeHidden();

    await expect(page.getByTestId('address-bar')).toBeVisible();
    await expect(page.locator('.i-lucide-triangle-alert')).toHaveCount(0);
  });

  test('toolbar does not depend on the JS drag fallback', async ({ tauriPage: page }) => {
    await openHome(page);
    await expect(page.locator('input[placeholder="Search or enter address"]')).toBeVisible();

    const toolbar = page.getByTestId('toolbar');
    await toolbar.click({ position: { x: 20, y: 20 }, force: true });

    const dragCalls = await getInvocationsFor(page, 'plugin:window|start_dragging');
    expect(dragCalls).toHaveLength(0);
  });

  test('toolbar controls do not trigger the JS drag fallback', async ({ tauriPage: page }) => {
    await openHome(page);

    await page.locator('input[placeholder="Search or enter address"]').click();
    await page.getByTitle('Home').click();
    await page.getByTitle('Settings').click();

    const dragCalls = await getInvocationsFor(page, 'plugin:window|start_dragging');
    expect(dragCalls).toHaveLength(0);
  });

  test('toolbar marks only non-interactive header regions as draggable', async ({ tauriPage: page }) => {
    await openHome(page);
    await expect(page.locator('input[placeholder="Search or enter address"]')).toBeVisible();

    const dragRegions = await page.evaluate(() => {
      const toolbar = document.querySelector<HTMLElement>('[data-testid="toolbar"]');
      const input = document.querySelector<HTMLInputElement>('input[placeholder="Search or enter address"]');
      const backButton = document.querySelector<HTMLButtonElement>('button[title="Back"]');
      const settingsButton = document.querySelector<HTMLButtonElement>('button[title="Settings"]');
      const addressBar = document.querySelector<HTMLElement>('[data-testid="address-bar"]');

      if (!toolbar || !input || !backButton || !settingsButton || !addressBar) {
        throw new Error('toolbar controls not found');
      }

      const centerRegion = addressBar?.parentElement;
      const navRegion = backButton.parentElement;
      const searchIcon = addressBar?.querySelector('.i-lucide-search');

      return {
        toolbar: toolbar.getAttribute('data-tauri-drag-region'),
        navRegion: navRegion?.getAttribute('data-tauri-drag-region'),
        centerRegion: centerRegion?.getAttribute('data-tauri-drag-region'),
        addressBar: addressBar?.getAttribute('data-tauri-drag-region'),
        searchIcon: searchIcon?.getAttribute('data-tauri-drag-region'),
        backButton: backButton.getAttribute('data-tauri-drag-region'),
        settingsButton: settingsButton.getAttribute('data-tauri-drag-region'),
        input: input.getAttribute('data-tauri-drag-region'),
      };
    });

    expect(dragRegions.toolbar).not.toBeNull();
    expect(dragRegions.navRegion).not.toBeNull();
    expect(dragRegions.centerRegion).not.toBeNull();
    expect(dragRegions.addressBar).toBe('false');
    expect(dragRegions.searchIcon).toBe('false');
    expect(dragRegions.backButton).toBe('false');
    expect(dragRegions.settingsButton).toBe('false');
    expect(dragRegions.input).toBe('false');
  });

  test('home button closes webview and shows launcher', async ({ tauriPage: page }) => {
    await openHome(page);

    // Navigate to a URL first
    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('https://example.com');
    await input.press('Enter');

    // Launcher should be hidden
    await expect(page.getByRole('heading', { name: 'Suggestions' })).not.toBeVisible();

    // Click home button
    await page.getByTitle('Home').click();

    // close_webview should have been called
    const closeCalls = await getInvocationsFor(page, 'close_webview');
    expect(closeCalls.length).toBeGreaterThan(0);

    // Launcher should be visible again
    await expect(page.getByRole('heading', { name: 'Suggestions' })).toBeVisible();

    // Address bar should be cleared
    const inputValue = await input.inputValue();
    expect(inputValue).toBe('');
  });

  test('settings button shows settings page', async ({ tauriPage: page }) => {
    await openHome(page);

    await page.getByTitle('Settings').click();

    await expect(page.getByRole('heading', { name: 'Settings' })).toBeVisible();
    await expect(page.getByTestId('settings-nav-app')).toBeVisible();
    await expect(page.getByTestId('settings-nav-network')).toBeVisible();
  });

  test('back button from settings returns to launcher', async ({ tauriPage: page }) => {
    await openHome(page);

    // Go to settings
    await page.getByTitle('Settings').click();
    await expect(page.getByText('Launch at startup')).toBeVisible();

    // Click back
    await page.getByTitle('Back').click();

    // Should be on launcher
    await expect(page.getByRole('heading', { name: 'Suggestions' })).toBeVisible();
  });

  test('back and forward buttons are disabled when no history', async ({ tauriPage: page }) => {
    await openHome(page);

    const backBtn = page.getByTitle('Back');
    const fwdBtn = page.getByTitle('Forward');

    await expect(backBtn).toBeDisabled();
    await expect(fwdBtn).toBeDisabled();
  });

  test('forward button works after home -> page -> back', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    const backBtn = page.getByTitle('Back');
    const fwdBtn = page.getByTitle('Forward');

    // Navigate to a page
    await input.click();
    await input.fill('https://example.com');
    await input.press('Enter');

    // Back should be enabled, forward disabled
    await expect(backBtn).toBeEnabled();
    await expect(fwdBtn).toBeDisabled();

    // Go back to launcher
    await backBtn.click();
    await expect(page.getByRole('heading', { name: 'Suggestions' })).toBeVisible();

    // Forward should now be enabled
    await expect(fwdBtn).toBeEnabled();

    // Go forward — should navigate back to the page
    await fwdBtn.click();
    await expect(page.getByRole('heading', { name: 'Suggestions' })).not.toBeVisible();

    const navCalls = await getInvocationsFor(page, 'create_nip07_webview');
    expect(navCalls.length).toBeGreaterThanOrEqual(2); // initial + forward
  });

  test('address bar updates when navigating', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('https://example.com');
    await input.press('Enter');

    // Blur via a real click outside the field to avoid worker timing flakiness.
    await page.locator('main').click({ position: { x: 20, y: 20 } });
    await expect(input).toHaveValue('example.com');
  });

  test('refresh does not create synthetic webview history', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('https://example.com');
    await input.press('Enter');

    await emitTauriEvent(page, 'child-webview-location', {
      label: 'content',
      url: 'https://example.com',
      source: 'load',
    });
    await emitTauriEvent(page, 'child-webview-page-load', {
      label: 'content',
      url: 'https://example.com',
      event: 'finished',
    });

    await expect(page.getByTitle('Refresh')).toBeVisible();
    await page.getByTitle('Refresh').click();
    const reloadCalls = await getInvocationsFor(page, 'reload_webview');
    expect(reloadCalls).toHaveLength(1);

    await emitTauriEvent(page, 'child-webview-location', {
      label: 'content',
      url: 'https://example.com',
      source: 'load',
    });
    await emitTauriEvent(page, 'child-webview-page-load', {
      label: 'content',
      url: 'https://example.com',
      event: 'finished',
    });

    await page.getByTitle('Back').click();

    await expect(page.getByRole('heading', { name: 'Suggestions' })).toBeVisible();
    const historyCalls = await getInvocationsFor(page, 'webview_history');
    expect(historyCalls).toHaveLength(0);
  });

  test('recreates the embedded browser when navigation changes isolation scope', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('https://example.com');
    await input.press('Enter');

    await emitTauriEvent(page, 'child-webview-location', {
      label: 'content',
      url: 'https://example.com/',
      source: 'navigation',
    });

    await emitTauriEvent(page, 'child-webview-location', {
      label: 'content',
      url: 'https://second.example.com/',
      source: 'navigation',
    });

    await emitTauriEvent(page, 'child-webview-page-load', {
      label: 'content',
      url: 'https://second.example.com/',
      event: 'started',
    });

    await expect.poll(async () => (await getInvocationsFor(page, 'create_nip07_webview')).length).toBe(2);

    const closeCalls = await getInvocationsFor(page, 'close_webview');
    expect(closeCalls.length).toBeGreaterThan(0);

    const createCalls = await getInvocationsFor(page, 'create_nip07_webview');
    expect(createCalls[1]?.args?.url).toBe('https://second.example.com/');
  });

  test('waits for page load before recreating after a cross-scope navigation signal', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('https://yle.fi');
    await input.press('Enter');

    await emitTauriEvent(page, 'child-webview-location', {
      label: 'content',
      url: 'https://tag.userreport.com/server.html#instanceId=242704&origin=https%3A%2F%2Fyle.fi',
      source: 'navigation',
    });

    await expect.poll(async () => (await getInvocationsFor(page, 'create_nip07_webview')).length).toBe(1);

    await emitTauriEvent(page, 'child-webview-page-load', {
      label: 'content',
      url: 'https://tag.userreport.com/server.html#instanceId=242704&origin=https%3A%2F%2Fyle.fi',
      event: 'started',
    });

    await expect.poll(async () => (await getInvocationsFor(page, 'create_nip07_webview')).length).toBe(2);

    const createCalls = await getInvocationsFor(page, 'create_nip07_webview');
    expect(createCalls[1]?.args?.url).toBe(
      'https://tag.userreport.com/server.html#instanceId=242704&origin=https%3A%2F%2Fyle.fi',
    );
  });
});
