import { test, expect, gotoHome, setupPageErrorHandler } from './fixtures';
import { getPublicKey, nip04, nip19, nip44, verifyEvent } from 'nostr-tools';

const SECRET_HEX = '1111111111111111111111111111111111111111111111111111111111111111';
const SECRET_BYTES = Uint8Array.from(Buffer.from(SECRET_HEX, 'hex'));
const EXPECTED_PUBKEY = getPublicKey(SECRET_BYTES);
const EXPECTED_NSEC = nip19.nsecEncode(SECRET_BYTES);
const SECOND_SECRET_HEX = '2222222222222222222222222222222222222222222222222222222222222222';
const SECOND_SECRET_BYTES = Uint8Array.from(Buffer.from(SECOND_SECRET_HEX, 'hex'));

async function openAccountMenu(page: import('@playwright/test').Page) {
  await page.getByTestId('account-button').click();
  await expect(page.getByTestId('account-menu')).toBeVisible();
}

async function signInWithSecret(page: import('@playwright/test').Page, secret = SECRET_HEX) {
  await openAccountMenu(page);
  await page.getByTestId('toggle-add-account-button').click();
  await page.getByTestId('account-nsec-input').fill(secret);
  await page.getByTestId('account-save-button').click();
  await expect(page.getByTestId('account-button')).toHaveAttribute('data-account-state', 'signed-in');
}

async function openAddress(page: import('@playwright/test').Page, url: string) {
  const addressInput = page.locator('[data-testid="address-bar"] input');
  await addressInput.fill(url);
  await addressInput.press('Enter');
}

test.describe('NIP-07 account', () => {
  test('shows friendly user labels without exposing raw keys in the shell', async ({ tauriPage: page }) => {
    setupPageErrorHandler(page);
    await gotoHome(page);

    await expect(page.getByTestId('account-button')).toBeVisible();
    await expect(page.getByTestId('account-button')).toHaveAttribute('data-account-state', 'signed-out');

    await signInWithSecret(page);
    await openAccountMenu(page);

    await expect(page.getByTestId('account-current-name')).toBeVisible();
    await expect(page.getByTestId('account-item')).toHaveCount(1);
    await expect(page.getByTestId('active-account-name')).not.toContainText('npub1');
    await expect(page.getByTestId('active-account-name')).not.toContainText(EXPECTED_PUBKEY);
    await expect(page.getByTestId('account-menu')).not.toContainText('nsec1');
    await expect(page.getByTestId('account-menu')).not.toContainText(EXPECTED_PUBKEY);
    await expect(page.locator('[data-testid="account-npub"]')).toHaveCount(0);
    await expect(page.locator('[data-testid="account-pubkey"]')).toHaveCount(0);
  });

  test('changing the active user reloads the current child webview', async ({ tauriPage: page }) => {
    setupPageErrorHandler(page);
    await gotoHome(page);
    await signInWithSecret(page);
    await openAddress(page, 'https://example.com');

    await openAccountMenu(page);
    await page.getByTestId('toggle-add-account-button').click();
    await page.getByTestId('account-nsec-input').fill(SECOND_SECRET_HEX);
    await page.getByTestId('account-save-button').click();

    const reloadsAfterAddingSecondUser = await page.evaluate(() => {
      return ((window as Window & { __tauriInvocations?: Array<{ cmd: string }> }).__tauriInvocations ?? [])
        .filter((entry) => entry.cmd === 'reload_webview')
        .length;
    });
    expect(reloadsAfterAddingSecondUser).toBeGreaterThan(0);

    await openAccountMenu(page);
    await expect(page.getByTestId('account-item')).toHaveCount(2);
    await page.getByTestId('account-item').nth(0).click();

    const reloadsAfterSwitchingBack = await page.evaluate(() => {
      return ((window as Window & { __tauriInvocations?: Array<{ cmd: string }> }).__tauriInvocations ?? [])
        .filter((entry) => entry.cmd === 'reload_webview')
        .length;
    });
    expect(reloadsAfterSwitchingBack).toBeGreaterThan(reloadsAfterAddingSecondUser);
  });

  test('window.nostr satisfies the core NIP-07 methods after login', async ({ tauriPage: page }) => {
    setupPageErrorHandler(page);
    await gotoHome(page);
    await signInWithSecret(page);

    const signed = await page.evaluate(async () => {
      const nostrApi = (window as Window & {
        nostr?: {
          getPublicKey: () => Promise<string>;
          signEvent: (event: {
            created_at: number;
            kind: number;
            tags: string[][];
            content: string;
          }) => Promise<Record<string, unknown>>;
        };
      }).nostr;

      if (!nostrApi) {
        throw new Error('window.nostr is not available');
      }

      const pubkey = await nostrApi.getPublicKey();
      const event = await nostrApi.signEvent({
        created_at: 1_711_111_111,
        kind: 1,
        tags: [['t', 'iris']],
        content: 'hello from iris shell',
      });
      return { pubkey, event };
    });

    expect(signed.pubkey).toBe(EXPECTED_PUBKEY);
    expect(signed.event).toMatchObject({
      pubkey: signed.pubkey,
      created_at: 1_711_111_111,
      kind: 1,
      tags: [['t', 'iris']],
      content: 'hello from iris shell',
    });
    expect(typeof signed.event.id).toBe('string');
    expect(typeof signed.event.sig).toBe('string');
    expect(verifyEvent(signed.event as Parameters<typeof verifyEvent>[0])).toBe(true);
  });

  test('window.nostr supports nip04 and nip44 round trips after login', async ({ tauriPage: page }) => {
    setupPageErrorHandler(page);
    await gotoHome(page);
    await signInWithSecret(page);

    const otherPubkey = getPublicKey(Uint8Array.from(Buffer.from(SECOND_SECRET_HEX, 'hex')));
    const expectedNip04 = await nip04.encrypt(SECOND_SECRET_HEX, EXPECTED_PUBKEY, 'hello from nip04');
    const expectedNip44 = await nip44.encrypt(
      'hello from nip44',
      nip44.getConversationKey(SECOND_SECRET_BYTES, EXPECTED_PUBKEY),
    );

    const decrypted = await page.evaluate(
      async ({ pubkey, nip04Ciphertext, nip44Ciphertext }) => {
        const nostrApi = (window as Window & {
          nostr?: {
            nip04: {
              encrypt: (pubkey: string, plaintext: string) => Promise<string>;
              decrypt: (pubkey: string, ciphertext: string) => Promise<string>;
            };
            nip44: {
              encrypt: (pubkey: string, plaintext: string) => Promise<string>;
              decrypt: (pubkey: string, ciphertext: string) => Promise<string>;
            };
          };
        }).nostr;

        if (!nostrApi) {
          throw new Error('window.nostr is not available');
        }

        return {
          nip04Encrypted: await nostrApi.nip04.encrypt(pubkey, 'hello from nip04'),
          nip04Decrypted: await nostrApi.nip04.decrypt(pubkey, nip04Ciphertext),
          nip44Encrypted: await nostrApi.nip44.encrypt(pubkey, 'hello from nip44'),
          nip44Decrypted: await nostrApi.nip44.decrypt(pubkey, nip44Ciphertext),
        };
      },
      {
        pubkey: otherPubkey,
        nip04Ciphertext: expectedNip04,
        nip44Ciphertext: expectedNip44,
      },
    );

    expect(await nip04.decrypt(SECOND_SECRET_HEX, EXPECTED_PUBKEY, decrypted.nip04Encrypted)).toBe(
      'hello from nip04',
    );
    expect(decrypted.nip04Decrypted).toBe('hello from nip04');
    expect(
      await nip44.decrypt(
        decrypted.nip44Encrypted,
        nip44.getConversationKey(SECOND_SECRET_BYTES, EXPECTED_PUBKEY),
      ),
    ).toBe('hello from nip44');
    expect(decrypted.nip44Decrypted).toBe('hello from nip44');
  });

  test('external sites are prompted once and can be remembered per site', async ({ tauriPage: page }) => {
    setupPageErrorHandler(page);
    await gotoHome(page);
    await signInWithSecret(page);

    const firstRequest = page.evaluate(() => {
      return (window as Window & {
        __TAURI_INTERNALS__?: {
          invoke: (cmd: string, args: Record<string, unknown>) => Promise<Record<string, unknown>>;
        };
      }).__TAURI_INTERNALS__?.invoke('nip07_request', {
        method: 'getPublicKey',
        params: {},
        origin: 'https://jumble.social',
      });
    });

    await expect(page.getByTestId('nip07-permission-prompt')).toBeVisible();
    await expect(page.getByTestId('nip07-permission-prompt')).toContainText('jumble.social');
    await page.getByTestId('nip07-permission-allow-always').click();

    const firstResponse = await firstRequest;
    expect(firstResponse?.result).toBe(EXPECTED_PUBKEY);
    expect(firstResponse?.error ?? null).toBeNull();
    await expect(page.getByTestId('nip07-permission-prompt')).toHaveCount(0);

    const secondResponse = await page.evaluate(() => {
      return (window as Window & {
        __TAURI_INTERNALS__?: {
          invoke: (cmd: string, args: Record<string, unknown>) => Promise<Record<string, unknown>>;
        };
      }).__TAURI_INTERNALS__?.invoke('nip07_request', {
        method: 'getPublicKey',
        params: {},
        origin: 'https://jumble.social',
      });
    });

    expect(secondResponse?.result).toBe(EXPECTED_PUBKEY);
    expect(secondResponse?.error ?? null).toBeNull();
    await expect(page.getByTestId('nip07-permission-prompt')).toHaveCount(0);
  });

  test('copies nsec from settings without showing it and clears only if unchanged', async ({ tauriPage: page }) => {
    await page.addInitScript(() => {
      (window as Window & { __IRIS_TEST_CLIPBOARD_CLEAR_DELAY_MS__?: number })
        .__IRIS_TEST_CLIPBOARD_CLEAR_DELAY_MS__ = 25;
    });
    setupPageErrorHandler(page);
    await gotoHome(page);
    await signInWithSecret(page);

    await openAccountMenu(page);
    await page.getByTestId('manage-users-button').click();
    await expect(page.getByTestId('settings-users-panel')).toBeVisible();

    const copyButton = page.getByTestId(`copy-nsec-button-${EXPECTED_PUBKEY}`);
    await copyButton.click();
    await expect(copyButton).toHaveText('Copied');
    await expect(page.locator('body')).not.toContainText(EXPECTED_NSEC);

    const firstClipboard = await page.evaluate(() => {
      return (window as Window & { __irisClipboardText?: string }).__irisClipboardText ?? '';
    });
    expect(firstClipboard).toBe(EXPECTED_NSEC);

    await page.waitForTimeout(60);
    const clearedClipboard = await page.evaluate(() => {
      return (window as Window & { __irisClipboardText?: string }).__irisClipboardText ?? '';
    });
    expect(clearedClipboard).toBe('');

    await copyButton.click();
    await page.evaluate(() => {
      (window as Window & { __irisClipboardText?: string }).__irisClipboardText = 'keep me';
    });
    await page.waitForTimeout(60);
    const preservedClipboard = await page.evaluate(() => {
      return (window as Window & { __irisClipboardText?: string }).__irisClipboardText ?? '';
    });
    expect(preservedClipboard).toBe('keep me');
  });

  test('clears pasted private keys from the add-existing field clipboard immediately', async ({ tauriPage: page }) => {
    await page.addInitScript(() => {
      (window as Window & { __IRIS_TEST_CLIPBOARD_CLEAR_DELAY_MS__?: number })
        .__IRIS_TEST_CLIPBOARD_CLEAR_DELAY_MS__ = 250;
    });
    setupPageErrorHandler(page);
    await gotoHome(page);

    await openAccountMenu(page);
    await page.getByTestId('toggle-add-account-button').click();

    await page.getByTestId('account-nsec-input').evaluate((input, value) => {
      (window as Window & { __irisClipboardText?: string }).__irisClipboardText = value;
      const pasteEvent = new Event('paste', { bubbles: true, cancelable: true });
      Object.defineProperty(pasteEvent, 'clipboardData', {
        configurable: true,
        value: {
          getData: (type: string) => type === 'text/plain' ? value : '',
        },
      });
      input.dispatchEvent(pasteEvent);
      (input as HTMLInputElement).value = value;
      input.dispatchEvent(new Event('input', { bubbles: true }));
    }, EXPECTED_NSEC);

    await page.waitForTimeout(10);
    const clipboardAfterPaste = await page.evaluate(() => {
      return (window as Window & { __irisClipboardText?: string }).__irisClipboardText ?? '';
    });
    expect(clipboardAfterPaste).toBe('');
  });
});
