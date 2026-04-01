import { expect, test } from '@playwright/test';

const ISOLATION_HTML = `<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <title>Origin Isolation Probe</title>
  </head>
  <body>
    origin isolation probe
  </body>
</html>`;

async function installIsolationRoute(page: import('@playwright/test').Page) {
  await page.context().route(/^http:\/\/[a-z0-9-]+\.htree\.localhost:1420\/.*$/i, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'text/html; charset=utf-8',
      body: ISOLATION_HTML,
    });
  });
}

test.describe('Origin isolation', () => {
  test('different isolated htree hosts do not share localStorage', async ({ page }) => {
    await installIsolationRoute(page);

    const originA = 'http://tree-a.htree.localhost:1420/origin-a/index.html';
    const originB = 'http://tree-b.htree.localhost:1420/origin-b/index.html';

    await page.goto(originA);
    await page.evaluate(() => localStorage.setItem('iris-origin-test', 'alpha'));

    await page.goto(originB);
    expect(await page.evaluate(() => localStorage.getItem('iris-origin-test'))).toBeNull();
    await page.evaluate(() => localStorage.setItem('iris-origin-test', 'beta'));

    await page.goto(originA);
    expect(await page.evaluate(() => localStorage.getItem('iris-origin-test'))).toBe('alpha');

    await page.goto(originB);
    expect(await page.evaluate(() => localStorage.getItem('iris-origin-test'))).toBe('beta');
  });

  test('the same isolated host shares localStorage across paths', async ({ page }) => {
    await installIsolationRoute(page);

    const pageA = 'http://tree-same.htree.localhost:1420/video/index.html';
    const pageB = 'http://tree-same.htree.localhost:1420/video/feed.html';

    await page.goto(pageA);
    await page.evaluate(() => localStorage.setItem('iris-origin-test', 'shared'));

    await page.goto(pageB);
    expect(await page.evaluate(() => localStorage.getItem('iris-origin-test'))).toBe('shared');
  });
});
