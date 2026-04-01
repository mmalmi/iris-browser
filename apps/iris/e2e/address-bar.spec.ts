import { test, expect, getInvocationsFor, setupPageErrorHandler, gotoHome, emitTauriEvent } from './fixtures';
import { distributedOwner } from '../src/lib/apps';
import { ownerProfileUrl } from '../src/lib/addressIdentity';

const DISTRIBUTED_OWNER_PROFILE_NAME = 'Sirius Business Ltd';

async function openHome(page: import('@playwright/test').Page) {
  setupPageErrorHandler(page);
  await gotoHome(page);
}

test.describe('Address Bar', () => {
  test('navigating via address bar creates webview', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('https://example.com');
    await input.press('Enter');

    const calls = await getInvocationsFor(page, 'create_nip07_webview');
    expect(calls.length).toBe(1);
    expect(calls[0].args.url).toBe('https://example.com');
  });

  test('bare domain gets https prefix', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('example.com');
    await input.press('Enter');

    const calls = await getInvocationsFor(page, 'create_nip07_webview');
    expect(calls.length).toBe(1);
    expect(calls[0].args.url).toBe('https://example.com');
  });

  test('htree URL uses create_htree_webview', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('htree://npub1abc123def456/my-tree');
    await input.press('Enter');

    const calls = await getInvocationsFor(page, 'create_htree_webview');
    expect(calls.length).toBe(1);
    expect(calls[0].args.npub).toBe('npub1abc123def456');
    expect(calls[0].args.treename).toBe('my-tree');
    expect(calls[0].args.path).toBe('/');
  });

  test('htree URL with path parses correctly', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('htree://npub1abc123def456/my-tree/some/path');
    await input.press('Enter');

    const calls = await getInvocationsFor(page, 'create_htree_webview');
    expect(calls.length).toBe(1);
    expect(calls[0].args.npub).toBe('npub1abc123def456');
    expect(calls[0].args.treename).toBe('my-tree');
    expect(calls[0].args.path).toBe('/some/path');
  });

  test('htree URL preserves git hash routes', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('htree://npub1app123/git/#/npub1repo456/hashtree?tab=pulls');
    await input.press('Enter');

    const calls = await getInvocationsFor(page, 'create_htree_webview');
    expect(calls.length).toBe(1);
    expect(calls[0].args.npub).toBe('npub1app123');
    expect(calls[0].args.treename).toBe('git');
    expect(calls[0].args.path).toBe('/');
    expect(calls[0].args.fragment).toBe('/npub1repo456/hashtree?tab=pulls');
  });

  test('legacy dot-host htree URL stays backward compatible', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('htree://npub1abc123def456.public/index.html');
    await input.press('Enter');

    const calls = await getInvocationsFor(page, 'create_htree_webview');
    expect(calls.length).toBe(1);
    expect(calls[0].args.npub).toBe('npub1abc123def456');
    expect(calls[0].args.treename).toBe('public');
    expect(calls[0].args.path).toBe('/index.html');
  });

  test('bare npub1 gets htree:// prefix', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('npub1abc123def456/my-tree');
    await input.press('Enter');

    const calls = await getInvocationsFor(page, 'create_htree_webview');
    expect(calls.length).toBe(1);
    expect(calls[0].args.npub).toBe('npub1abc123def456');
    expect(calls[0].args.treename).toBe('my-tree');
  });

  test('htree self URL uses create_htree_webview with self host', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('htree://self/video');
    await input.press('Enter');

    const calls = await getInvocationsFor(page, 'create_htree_webview');
    expect(calls.length).toBe(1);
    expect(calls[0].args.host).toBe('self');
    expect(calls[0].args.npub).toBeNull();
    expect(calls[0].args.treename).toBe('video');
    expect(calls[0].args.path).toBe('/');
  });

  test('bare self tree gets htree:// prefix', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('self/video');
    await input.press('Enter');

    const calls = await getInvocationsFor(page, 'create_htree_webview');
    expect(calls.length).toBe(1);
    expect(calls[0].args.host).toBe('self');
    expect(calls[0].args.npub).toBeNull();
    expect(calls[0].args.treename).toBe('video');
    expect(calls[0].args.path).toBe('/');
  });

  test('bare nhash1 gets htree:// prefix', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('nhash1abc123/Featured.jpg');
    await input.press('Enter');

    const calls = await getInvocationsFor(page, 'create_htree_webview');
    expect(calls.length).toBe(1);
    expect(calls[0].args.nhash).toBe('nhash1abc123');
    expect(calls[0].args.path).toBe('/Featured.jpg');
  });

  test('trailing slash stripped from display URL', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('https://video.iris.to/');
    await input.press('Enter');

    // Blur via a real click outside the field to avoid worker timing flakiness.
    await page.locator('main').click({ position: { x: 20, y: 20 } });
    await expect(input).toHaveValue('video.iris.to');
  });

  test('focus shows full URL, blur shows display URL', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('https://video.iris.to/');
    await input.press('Enter');

    // Blurred: display URL without protocol/trailing slash
    await page.locator('main').click({ position: { x: 20, y: 20 } });
    await expect(input).toHaveValue('video.iris.to');

    // Focus: full URL
    await input.click();
    await expect(input).toHaveValue('https://video.iris.to/');

    // Blur again: display URL
    await page.getByTitle('Home').click();
  });

  test('blurred htree npub routes render an owner pill and restore the full URL on focus', async ({ tauriPage: page }) => {
    await openHome(page);

    const url = `htree://${distributedOwner}/video/index.html`;
    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill(url);
    await input.press('Enter');
    await expect.poll(async () => (await getInvocationsFor(page, 'create_htree_webview')).length).toBe(1);
    await page.locator('main').click({ position: { x: 20, y: 20 } });

    await expect(page.getByTestId('address-owner-pill')).toBeVisible();
    await expect(page.getByTestId('address-owner-name')).toHaveText(DISTRIBUTED_OWNER_PROFILE_NAME);
    await expect(page.getByTestId('address-path')).toHaveText('video');
    await expect(input).toHaveValue(`${distributedOwner}/video/index.html`);

    await page.getByTestId('address-bar').click();
    await expect(page.getByTestId('address-owner-pill')).toHaveCount(0);
    await expect(input).toHaveValue(url);
  });

  test('clicking the blurred owner pill opens the distributed iris-files profile route', async ({ tauriPage: page }) => {
    await openHome(page);

    const url = `htree://${distributedOwner}/video/index.html`;
    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill(url);
    await input.press('Enter');
    await expect.poll(async () => (await getInvocationsFor(page, 'create_htree_webview')).length).toBe(1);
    await page.locator('main').click({ position: { x: 20, y: 20 } });

    const ownerPill = page.getByTestId('address-owner-pill');
    await expect(ownerPill).toBeVisible();
    await expect(ownerPill).toHaveAttribute('data-profile-url', ownerProfileUrl(distributedOwner));
    await ownerPill.evaluate((element) => {
      (element as HTMLButtonElement).click();
    });

    await expect.poll(async () => (await getInvocationsFor(page, 'create_htree_webview')).length).toBe(2);
    const calls = await getInvocationsFor(page, 'create_htree_webview');
    expect(calls.length).toBe(2);
    expect(calls[1].args.npub).toBe(distributedOwner);
    expect(calls[1].args.treename).toBe('files');
    expect(calls[1].args.path).toBe('/index.html');
    expect(calls[1].args.fragment).toBe(`/${distributedOwner}/profile`);
  });

  test('blurred htree nhash routes render plain page title text and restore the full URL on focus', async ({ tauriPage: page }) => {
    await openHome(page);

    const url = 'htree://nhash1example/index.html';
    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill(url);
    await input.press('Enter');
    await expect.poll(async () => (await getInvocationsFor(page, 'create_htree_webview')).length).toBe(1);

    await emitTauriEvent(page, 'child-webview-diagnostic', {
      label: 'content',
      url,
      source: 'load',
      title: 'Immutable demo',
      bodyText: 'Loaded from hashtree',
      mediaSummary: null,
      error: null,
    });

    await page.locator('main').click({ position: { x: 20, y: 20 } });

    await expect(page.getByTestId('address-title-text')).toBeVisible();
    await expect(page.getByTestId('address-title-text')).toHaveText('Immutable demo');
    await expect(input).toHaveValue('nhash1example/index.html');

    await page.getByTestId('address-bar').click();
    await expect(page.getByTestId('address-title-text')).toHaveCount(0);
    await expect(input).toHaveValue(url);
  });

  test('address bar loading state uses a centered svg spinner', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('https://example.com');
    await input.press('Enter');

    const spinner = page.locator('svg[data-testid="address-loading-spinner"]');
    await expect(spinner).toBeVisible();
    await expect(spinner).toHaveAttribute('viewBox', '0 0 16 16');
    await expect(spinner.locator('circle')).toHaveAttribute('cx', '8');
    await expect(spinner.locator('circle')).toHaveAttribute('cy', '8');
  });

  test('history dropdown shows owner label and page title for npub routes', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill(`htree://${distributedOwner}/video/index.html`);
    await input.press('Enter');
    await emitTauriEvent(page, 'child-webview-diagnostic', {
      label: 'content',
      url: `htree://${distributedOwner}/video/index.html`,
      source: 'load',
      title: 'Iris Video',
      bodyText: 'Recent videos',
      mediaSummary: 'thumbs=3/4 visible=2 videos=1/1',
      error: null,
    });
    await expect.poll(async () => {
      return await page.evaluate(() => ((window as any).__historyStore ?? [])[0]?.label ?? '');
    }).toBe('Iris Video');
    await page.getByTitle('Home').click();
    await expect.poll(async () => {
      return await page.evaluate(() => ((window as any).__historyStore ?? []).length);
    }).toBe(1);

    await input.click();
    const dropdown = page.locator('[role="listbox"]');
    await expect(dropdown).toBeVisible();
    await expect(dropdown.getByText(DISTRIBUTED_OWNER_PROFILE_NAME).first()).toBeVisible();
    await expect(dropdown.getByText('Iris Video').first()).toBeVisible();
  });

  test('empty address bar submit does nothing', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('');
    await input.press('Enter');

    // No webview creation
    const nip07 = await getInvocationsFor(page, 'create_nip07_webview');
    const htree = await getInvocationsFor(page, 'create_htree_webview');
    expect(nip07.length).toBe(0);
    expect(htree.length).toBe(0);

    // Launcher still visible
    await expect(page.getByRole('heading', { name: 'Suggestions' })).toBeVisible();
  });

  test('macOS function-key glyphs do not end up in the address bar', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();

    const result = await input.evaluate((element) => {
      const field = element as HTMLInputElement;
      const beforeInput = new InputEvent('beforeinput', {
        bubbles: true,
        cancelable: true,
        inputType: 'insertText',
        data: '\uF702\uF703\uF700\uF701',
      });
      const allowed = field.dispatchEvent(beforeInput);

      field.value = `abc\uF702\uF703\uF700\uF701def`;
      field.dispatchEvent(new Event('input', { bubbles: true }));

      return {
        allowed,
        value: field.value,
      };
    });

    expect(result.allowed).toBe(false);
    await expect(input).toHaveValue('abcdef');
  });

  test('macOS private-use arrow keys are handled as arrows, not text', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.fill('abcdef');

    const result = await input.evaluate((element) => {
      const field = element as HTMLInputElement;
      field.focus();
      field.setSelectionRange(3, 3);

      const dispatchArrow = (key: string, keyCode: number) => {
        const event = new KeyboardEvent('keydown', {
          bubbles: true,
          cancelable: true,
          key,
        });
        Object.defineProperty(event, 'keyCode', { get: () => keyCode });
        Object.defineProperty(event, 'which', { get: () => keyCode });
        const allowed = field.dispatchEvent(event);
        return {
          allowed,
          value: field.value,
          start: field.selectionStart,
          end: field.selectionEnd,
        };
      };

      return {
        left: dispatchArrow('\uF702', 37),
        right: dispatchArrow('\uF703', 39),
      };
    });

    expect(result.left.allowed).toBe(false);
    expect(result.left.value).toBe('abcdef');
    expect(result.left.start).toBe(2);
    expect(result.left.end).toBe(2);

    expect(result.right.allowed).toBe(false);
    expect(result.right.value).toBe('abcdef');
    expect(result.right.start).toBe(3);
    expect(result.right.end).toBe(3);
    await expect(input).toHaveValue('abcdef');
  });

  test('legacy macOS WebKit arrow key codes are handled as arrows, not text', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.fill('abcdef');

    const result = await input.evaluate((element) => {
      const field = element as HTMLInputElement;
      field.focus();
      field.setSelectionRange(3, 3);

      const dispatchArrow = (keyCode: number) => {
        const event = new KeyboardEvent('keydown', {
          bubbles: true,
          cancelable: true,
          key: 'Unidentified',
        });
        Object.defineProperty(event, 'keyCode', { get: () => keyCode });
        Object.defineProperty(event, 'which', { get: () => keyCode });
        const allowed = field.dispatchEvent(event);
        return {
          allowed,
          value: field.value,
          start: field.selectionStart,
          end: field.selectionEnd,
        };
      };

      return {
        left: dispatchArrow(63234),
        right: dispatchArrow(63235),
      };
    });

    expect(result.left.allowed).toBe(false);
    expect(result.left.value).toBe('abcdef');
    expect(result.left.start).toBe(2);
    expect(result.left.end).toBe(2);

    expect(result.right.allowed).toBe(false);
    expect(result.right.value).toBe('abcdef');
    expect(result.right.start).toBe(3);
    expect(result.right.end).toBe(3);
    await expect(input).toHaveValue('abcdef');
  });

  test('macOS private-use arrow keyup sanitizes late glyph insertion', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.fill('abcdef');

    const result = await input.evaluate((element) => {
      const field = element as HTMLInputElement;
      field.focus();
      field.setSelectionRange(3, 3);

      const dispatchArrow = (type: 'keydown' | 'keyup') => {
        const event = new KeyboardEvent(type, {
          bubbles: true,
          cancelable: true,
          key: '\uF703',
        });
        Object.defineProperty(event, 'keyCode', { get: () => 39 });
        Object.defineProperty(event, 'which', { get: () => 39 });
        field.dispatchEvent(event);
      };

      dispatchArrow('keydown');
      field.value = 'abc\uF703def';
      field.setSelectionRange(4, 4);
      dispatchArrow('keyup');

      return new Promise<{ value: string; start: number | null; end: number | null }>((resolve) => {
        requestAnimationFrame(() => {
          resolve({
            value: field.value,
            start: field.selectionStart,
            end: field.selectionEnd,
          });
        });
      });
    });

    expect(result.value).toBe('abcdef');
    expect(result.start).toBe(3);
    expect(result.end).toBe(3);
    await expect(input).toHaveValue('abcdef');
  });

  test('standard arrow keys keep native input behavior', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.fill('abcdef');

    const result = await input.evaluate((element) => {
      const field = element as HTMLInputElement;
      field.focus();
      field.setSelectionRange(3, 3);

      const dispatchArrow = (key: string, keyCode: number) => {
        const event = new KeyboardEvent('keydown', {
          bubbles: true,
          cancelable: true,
          key,
        });
        Object.defineProperty(event, 'keyCode', { get: () => keyCode });
        Object.defineProperty(event, 'which', { get: () => keyCode });
        const allowed = field.dispatchEvent(event);
        return {
          allowed,
          value: field.value,
          start: field.selectionStart,
          end: field.selectionEnd,
        };
      };

      return {
        left: dispatchArrow('ArrowLeft', 37),
        right: dispatchArrow('ArrowRight', 39),
      };
    });

    expect(result.left.allowed).toBe(true);
    expect(result.left.value).toBe('abcdef');
    expect(result.left.start).toBe(3);
    expect(result.left.end).toBe(3);

    expect(result.right.allowed).toBe(true);
    expect(result.right.value).toBe('abcdef');
    expect(result.right.start).toBe(3);
    expect(result.right.end).toBe(3);
    await expect(input).toHaveValue('abcdef');
  });

  test('focus does not show a dropdown when history is empty', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();

    await expect(page.locator('[role="listbox"]')).toHaveCount(0);
  });
});

test.describe('Address Bar Autocomplete', () => {
  /** Navigate to a URL via the address bar, then go home. */
  async function visitAndGoHome(page: import('@playwright/test').Page, url: string) {
    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill(url);
    await input.press('Enter');
    // Go home so we're back on the launcher
    await page.getByTitle('Home').click();
  }

  test('navigating records history and dropdown shows it on focus', async ({ tauriPage: page }) => {
    await openHome(page);

    // Visit two sites
    await visitAndGoHome(page, 'https://video.iris.to/');
    await visitAndGoHome(page, 'https://example.com');

    // Focus the address bar — should show both visited URLs
    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();

    const dropdown = page.locator('[role="listbox"]');
    await expect(dropdown).toBeVisible();
    await expect(dropdown.locator('[role="option"]')).toHaveCount(2);
    await expect(dropdown.getByText('video.iris.to').first()).toBeVisible();
    await expect(dropdown.getByText('example.com').first()).toBeVisible();
  });

  test('dropdown still shows fallback history when native history is empty', async ({ tauriPage: page }) => {
    await openHome(page);

    await visitAndGoHome(page, 'https://example.com');

    await page.evaluate(() => {
      ((window as any).__historyStore ?? []).length = 0;
    });

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();

    const dropdown = page.locator('[role="listbox"]');
    await expect(dropdown).toBeVisible();
    await expect(dropdown.getByText('example.com').first()).toBeVisible();
  });

  test('search filters history results', async ({ tauriPage: page }) => {
    await openHome(page);

    await visitAndGoHome(page, 'https://video.iris.to/');
    await visitAndGoHome(page, 'https://example.com');

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('video');

    const dropdown = page.locator('[role="listbox"]');
    await expect(dropdown).toBeVisible();
    // Only the matching entry should appear
    await expect(dropdown.locator('[role="option"]')).toHaveCount(1);
    await expect(dropdown.getByText('video.iris.to').first()).toBeVisible();
  });

  test('arrow keys navigate items', async ({ tauriPage: page }) => {
    await openHome(page);

    await visitAndGoHome(page, 'https://a.com');
    await visitAndGoHome(page, 'https://b.com');

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();

    const dropdown = page.locator('[role="listbox"]');
    await expect(dropdown).toBeVisible();

    // Press down to select first item
    await input.press('ArrowDown');
    const firstOption = dropdown.locator('[role="option"]').first();
    await expect(firstOption).toHaveAttribute('aria-selected', 'true');

    // Press down again to select second
    await input.press('ArrowDown');
    const secondOption = dropdown.locator('[role="option"]').nth(1);
    await expect(secondOption).toHaveAttribute('aria-selected', 'true');
  });

  test('escape closes dropdown', async ({ tauriPage: page }) => {
    await openHome(page);

    await visitAndGoHome(page, 'https://example.com');

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();

    const dropdown = page.locator('[role="listbox"]');
    await expect(dropdown).toBeVisible();

    await page.keyboard.press('Escape');
    await expect(dropdown).toBeHidden();
  });

  test('clicking dropdown item navigates to that URL', async ({ tauriPage: page }) => {
    await openHome(page);

    await visitAndGoHome(page, 'https://video.iris.to/');

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();

    const dropdown = page.locator('[role="listbox"]');
    await expect(dropdown).toBeVisible();

    // Click the history item
    await dropdown.locator('[role="option"]').first().click();

    // Should have navigated (second create call — first was the initial visit)
    const calls = await getInvocationsFor(page, 'create_nip07_webview');
    expect(calls.length).toBe(2);
    expect(calls[1].args.url).toBe('https://video.iris.to/');
  });

  test('X button deletes entry from dropdown', async ({ tauriPage: page }) => {
    await openHome(page);

    await visitAndGoHome(page, 'https://video.iris.to/');
    await visitAndGoHome(page, 'https://example.com');

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();

    const dropdown = page.locator('[role="listbox"]');
    await expect(dropdown).toBeVisible();
    await expect(dropdown.locator('[role="option"]')).toHaveCount(2);

    // Delete the first entry
    await dropdown.locator('[role="option"]').first().getByTitle('Delete').click();

    await expect(dropdown.locator('[role="option"]')).toHaveCount(1);

    // Verify delete was invoked
    const calls = await getInvocationsFor(page, 'delete_history_entry');
    expect(calls.length).toBe(1);
  });

  test('opening dropdown does not move the page viewport', async ({ tauriPage: page }) => {
    await openHome(page);

    const input = page.locator('input[placeholder="Search or enter address"]');
    await input.click();
    await input.fill('https://example.com');
    await input.press('Enter');
    await input.press('Tab');

    await input.click();
    const dropdown = page.locator('[role="listbox"]');
    await expect(dropdown).toBeVisible();
    await expect(dropdown.locator('[role="option"]')).toHaveCount(1);
    await page.waitForTimeout(250);

    const after = await getInvocationsFor(page, 'set_webview_bounds');
    expect(after.at(-1)?.args.y).toBe(48);
  });
});
