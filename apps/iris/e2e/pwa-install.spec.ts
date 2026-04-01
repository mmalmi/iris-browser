import {
  test,
  expect,
  emitTauriEvent,
  getInvocationsFor,
  gotoHome,
  setTauriCommandResult,
  setupPageErrorHandler,
} from './fixtures';

async function openSite(page: import('@playwright/test').Page, url = 'https://jumble.social/') {
  setupPageErrorHandler(page);
  await gotoHome(page);
  const addressInput = page.locator('[data-testid="address-bar"] input');
  await addressInput.click();
  await addressInput.fill(url);
  await addressInput.press('Enter');
}

async function emitPwaDiagnostic(page: import('@playwright/test').Page) {
  await emitTauriEvent(page, 'child-webview-diagnostic', {
    label: 'content',
    url: 'https://jumble.social/',
    title: 'Jumble',
    manifestUrl: 'https://jumble.social/manifest.webmanifest',
    manifestName: 'Jumble',
    manifestIconUrl: 'https://jumble.social/pwa-192x192.png',
  });
}

async function seedInstalledApps(
  page: import('@playwright/test').Page,
  apps: Array<Record<string, unknown>>,
) {
  await page.addInitScript((seedApps) => {
    localStorage.setItem('iris:apps', JSON.stringify(seedApps));
  }, apps);
}

test.describe('PWA install', () => {
  test('shows an Add to Iris home screen action for pages with a manifest', async ({ tauriPage: page }) => {
    await openSite(page);
    await emitPwaDiagnostic(page);

    await expect(page.getByTestId('install-pwa-button')).toBeVisible();
  });

  test('shows Update for an already installed site without changing the saved app until clicked', async ({ tauriPage: page }) => {
    await seedInstalledApps(page, [{
      name: 'Jumble',
      url: 'htree://nhash1jumble-old/index.html',
      icon: 'htree://nhash1jumble-old/icon-192.png',
      sourceUrl: 'https://jumble.social',
      sourceManifestUrl: 'https://jumble.social/manifest.webmanifest',
      addedAt: 123,
    }]);

    await openSite(page);
    await emitPwaDiagnostic(page);

    const installButton = page.getByTestId('install-pwa-button');
    await expect(installButton).toBeVisible();
    await expect(installButton).toHaveAttribute('title', 'Update in Iris home screen');

    const installedApps = await page.evaluate(() => JSON.parse(localStorage.getItem('iris:apps') ?? '[]'));
    expect(installedApps).toHaveLength(1);
    expect(installedApps[0]).toMatchObject({
      url: 'htree://nhash1jumble-old/index.html',
      sourceUrl: 'https://jumble.social',
      addedAt: 123,
    });
  });

  test('installs the current PWA into the Iris launcher using an immutable htree url', async ({ tauriPage: page }) => {
    await openSite(page);
    await setTauriCommandResult(page, 'install_site_pwa', {
      name: 'Jumble',
      launchUrl: 'htree://nhash1jumble/index.html',
      iconUrl: 'htree://nhash1jumble/pwa-192x192.png',
      sourceUrl: 'https://jumble.social/',
      sourceManifestUrl: 'https://jumble.social/manifest.webmanifest',
      sourceAppId: null,
    });
    await emitPwaDiagnostic(page);

    await page.getByTestId('install-pwa-button').click();

    await expect.poll(async () => (await getInvocationsFor(page, 'install_site_pwa')).length).toBe(1);

    await expect.poll(async () => {
      const apps = await page.evaluate(() => JSON.parse(localStorage.getItem('iris:apps') ?? '[]'));
      return apps.length;
    }).toBe(1);

    const installedApps = await page.evaluate(() => JSON.parse(localStorage.getItem('iris:apps') ?? '[]'));
    expect(installedApps).toHaveLength(1);
    expect(installedApps[0]).toMatchObject({
      name: 'Jumble',
      url: 'htree://nhash1jumble/index.html',
      icon: 'htree://nhash1jumble/pwa-192x192.png',
      sourceUrl: 'https://jumble.social/',
      sourceManifestUrl: 'https://jumble.social/manifest.webmanifest',
    });
    expect(typeof installedApps[0].addedAt).toBe('number');
  });

  test('reinstall updates an existing PWA entry in place when the manifest app id matches', async ({ tauriPage: page }) => {
    await seedInstalledApps(page, [{
      name: 'Jumble',
      url: 'htree://nhash1jumble-old/index.html',
      icon: 'htree://nhash1jumble-old/icon-192.png',
      sourceAppId: 'https://jumble.social/app',
      sourceUrl: 'https://old.example/jumble',
      sourceManifestUrl: 'https://old.example/manifest.webmanifest',
      addedAt: 123,
    }]);

    await openSite(page);
    await setTauriCommandResult(page, 'install_site_pwa', {
      name: 'Jumble',
      launchUrl: 'htree://nhash1jumble-new/index.html',
      iconUrl: 'htree://nhash1jumble-new/pwa-192x192.png',
      sourceAppId: 'https://jumble.social/app',
      sourceUrl: 'https://jumble.social/',
      sourceManifestUrl: 'https://jumble.social/manifest.webmanifest',
    });
    await emitPwaDiagnostic(page);

    await page.getByTestId('install-pwa-button').click();

    await expect.poll(async () => {
      const apps = await page.evaluate(() => JSON.parse(localStorage.getItem('iris:apps') ?? '[]'));
      return apps.length;
    }).toBe(1);

    const installedApps = await page.evaluate(() => JSON.parse(localStorage.getItem('iris:apps') ?? '[]'));
    expect(installedApps).toHaveLength(1);
    expect(installedApps[0]).toMatchObject({
      name: 'Jumble',
      url: 'htree://nhash1jumble-new/index.html',
      icon: 'htree://nhash1jumble-new/pwa-192x192.png',
      sourceAppId: 'https://jumble.social/app',
      sourceUrl: 'https://jumble.social/',
      sourceManifestUrl: 'https://jumble.social/manifest.webmanifest',
      addedAt: 123,
    });
  });
});
