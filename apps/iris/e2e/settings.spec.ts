import { test, expect, getInvocationsFor, setupPageErrorHandler, gotoHome } from './fixtures';

const DISTRIBUTED_OWNER = 'npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm';

async function openHome(page: import('@playwright/test').Page) {
  setupPageErrorHandler(page);
  await gotoHome(page);
}

test.describe('Settings Page', () => {
  test('shows settings navigation and defaults to the app section on wide screens', async ({ tauriPage: page }) => {
    await openHome(page);
    await page.getByTitle('Settings').click();

    await expect(page.getByTestId('settings-nav-app')).toBeVisible();
    await expect(page.getByRole('button', { name: 'Privacy' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Users' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Network' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'About' })).toBeVisible();

    await expect(page.getByText('Launch at startup')).toBeVisible();
    await expect(page.getByText('Open Iris automatically when you log in')).toBeVisible();
  });

  test('supports direct settings routes', async ({ tauriPage: page }) => {
    setupPageErrorHandler(page);
    await page.goto('/#/settings/network');

    await expect(page.getByRole('heading', { name: 'Local Service' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Network' })).toBeVisible();
  });

  test('network tab shows local network settings', async ({ tauriPage: page }) => {
    await openHome(page);
    await page.getByTitle('Settings').click();

    await page.getByRole('button', { name: 'Network' }).click();

    await expect(page.getByRole('heading', { name: 'Local Service' })).toBeVisible();
    await expect(page.getByText('http://127.0.0.1:21417')).toBeVisible();
    await expect(page.getByRole('heading', { name: 'Nostr Relays' })).toBeVisible();
    await expect(page.getByRole('heading', { name: 'Blossom' })).toBeVisible();
    await expect(page.getByRole('heading', { name: 'Peer Router' })).toBeVisible();
    await expect(page.getByLabel('Toggle Nostr relays')).toBeVisible();
    await expect(page.getByLabel('Toggle Blossom fallback')).toBeVisible();
    await expect(page.getByLabel('Toggle WebRTC transport')).toBeVisible();
    await expect(page.getByLabel('Toggle LAN multicast transport')).toBeVisible();
    await expect(page.getByLabel('Add relay URL')).toBeVisible();
    await expect(page.getByLabel('Add Blossom server URL')).toBeVisible();
    await expect(page.getByLabel('Multicast group')).toBeVisible();
    await expect(page.getByLabel('Multicast port')).toBeVisible();

    const relayToggleBox = await page.getByLabel('Toggle Nostr relays').boundingBox();
    const blossomToggleBox = await page.getByLabel('Toggle Blossom fallback').boundingBox();
    const webrtcToggleBox = await page.getByLabel('Toggle WebRTC transport').boundingBox();
    expect(relayToggleBox?.width ?? 0).toBeGreaterThanOrEqual(40);
    expect(blossomToggleBox?.width ?? 0).toBeGreaterThanOrEqual(40);
    expect(webrtcToggleBox?.width ?? 0).toBeGreaterThanOrEqual(40);
  });

  test('network tab updates daemon transport settings without leaving settings', async ({ tauriPage: page }) => {
    await openHome(page);
    await page.getByTitle('Settings').click();
    await page.getByRole('button', { name: 'Network' }).click();

    await page.getByLabel('Toggle LAN multicast transport').click();

    const calls = await getInvocationsFor(page, 'update_daemon_network_settings');
    expect(calls.length).toBe(1);
    expect(calls[0].args.settings.bluetooth).toBe(false);
    expect(calls[0].args.settings.webrtc).toBe(true);
    expect(calls[0].args.settings.multicast).toBe(true);
    await expect(page.getByRole('heading', { name: 'Local Service' })).toBeVisible();
  });

  test('network status polling starts only on the network tab', async ({ tauriPage: page }) => {
    let statusRequests = 0;
    await page.route('http://127.0.0.1:21417/api/status', async (route) => {
      statusRequests += 1;
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          status: 'running',
          mesh: {
            enabled: true,
            total_peers: 0,
            connected: 0,
            with_data_channel: 0,
            bytes_sent: 0,
            bytes_received: 0,
            transport_counts: {
              webrtc: 0,
              bluetooth: 0,
            },
            peers: [],
          },
          upstream: {
            blossom_servers: 0,
          },
        }),
      });
    });

    await openHome(page);
    await page.getByTitle('Settings').click();
    await page.waitForTimeout(250);
    expect(statusRequests).toBe(0);

    await page.getByRole('button', { name: 'Network' }).click();
    await expect(page.getByRole('heading', { name: 'Local Service' })).toBeVisible();
    await expect.poll(() => statusRequests).toBeGreaterThan(0);
  });

  test('network tab applies relay and blossom config edits', async ({ tauriPage: page }) => {
    await openHome(page);
    await page.getByTitle('Settings').click();
    await page.getByRole('button', { name: 'Network' }).click();

    const relayInput = page.getByLabel('Add relay URL');
    await relayInput.fill('wss://relay.example');
    await relayInput.locator('xpath=..').getByRole('button', { name: 'Add' }).click();
    await expect(page.getByText('relay.example').first()).toBeVisible();

    const blossomInput = page.getByLabel('Add Blossom server URL');
    await blossomInput.fill('https://blossom.example');
    await blossomInput.locator('xpath=..').getByRole('button', { name: 'Add' }).click();
    await expect(page.getByText('blossom.example').first()).toBeVisible();

    await page.getByLabel('Multicast port').fill('49001');

    await page.getByRole('button', { name: 'Apply' }).click();

    const calls = await getInvocationsFor(page, 'update_daemon_network_settings');
    const latest = calls.at(-1);
    expect(latest).toBeTruthy();
    expect(latest?.args.settings.relayUrls).toContain('wss://relay.example');
    expect(latest?.args.settings.multicastPort).toBe(49001);
    expect(latest?.args.settings.blossomServers).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          url: 'https://blossom.example',
          read: true,
          write: false,
        }),
      ]),
    );
    expect(latest?.args.settings.nostrRelaysEnabled).toBe(true);
    expect(latest?.args.settings.blossomEnabled).toBe(true);
  });

  test('network tab can disable relays and blossom without clearing the configured lists', async ({ tauriPage: page }) => {
    await openHome(page);
    await page.getByTitle('Settings').click();
    await page.getByRole('button', { name: 'Network' }).click();

    await page.getByLabel('Toggle Nostr relays').click();
    await page.getByLabel('Toggle Blossom fallback').click();
    await page.getByRole('button', { name: 'Apply' }).click();

    const calls = await getInvocationsFor(page, 'update_daemon_network_settings');
    const latest = calls.at(-1);
    expect(latest?.args.settings.nostrRelaysEnabled).toBe(false);
    expect(latest?.args.settings.blossomEnabled).toBe(false);
    expect(latest?.args.settings.relayUrls.length).toBeGreaterThan(0);
    expect(latest?.args.settings.blossomServers.length).toBeGreaterThan(0);
  });

  test('network tab shows mesh traffic and active peers from daemon status', async ({ tauriPage: page }) => {
    await page.route('http://127.0.0.1:21417/api/status', async (route) => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          status: 'running',
          mesh: {
            enabled: true,
            total_peers: 2,
            connected: 2,
            with_data_channel: 2,
            bytes_sent: 15360,
            bytes_received: 28672,
            transport_counts: {
              webrtc: 2,
              bluetooth: 0,
            },
            peers: [
              {
                id: 'peer-a',
                peer_id: 'peer-a',
                pubkey: 'f'.repeat(64),
                state: 'Connected',
                pool: 'Follows',
                transport: 'webrtc',
                signal_paths: ['multicast'],
                connected: true,
                has_data_channel: true,
                bytes_sent: 4096,
                bytes_received: 8192,
              },
              {
                id: 'peer-b',
                peer_id: 'peer-b',
                pubkey: 'e'.repeat(64),
                state: 'Connected',
                pool: 'Other',
                transport: 'webrtc',
                signal_paths: ['relay'],
                connected: true,
                has_data_channel: true,
                bytes_sent: 11264,
                bytes_received: 20480,
              },
              {
                id: 'peer-c',
                peer_id: 'peer-c',
                pubkey: 'd'.repeat(64),
                state: 'Discovered',
                pool: 'Other',
                transport: 'webrtc',
                signal_paths: ['relay'],
                connected: false,
                has_data_channel: false,
                bytes_sent: 0,
                bytes_received: 0,
              },
            ],
          },
          webrtc: {
            enabled: true,
          },
          upstream: {
            blossom_servers: 2,
          },
        }),
      });
    });

    await openHome(page);
    await page.getByTitle('Settings').click();
    await page.getByRole('button', { name: 'Network' }).click();

    await expect(page.getByRole('heading', { name: 'Mesh' })).toBeVisible();
    await expect(page.getByText('2 connected')).toBeVisible();
    await expect(page.getByRole('group', { name: 'WebRTC 2 peers' })).toBeVisible();
    await expect(page.getByText('Upload', { exact: true })).toBeVisible();
    await expect(page.getByText('Download', { exact: true })).toBeVisible();
    await expect(page.getByText('Recent Throughput')).toBeVisible();
    await expect(page.getByText('Active Peers')).toBeVisible();
    await expect(page.getByText('Contact peer 1')).toBeVisible();
    await expect(page.getByText('Relay peer 2')).toBeVisible();
    await expect(page.getByText('1 discovered peer not connected yet')).toBeVisible();
    await expect(page.getByText('LAN multicast').first()).toBeVisible();
    await expect(page.getByText('Relay signaling')).toBeVisible();
    await expect(page.getByText('1 blossom read server · 2 relays')).toBeVisible();
  });

  test('about tab opens the hashtree repository in Iris Git', async ({ tauriPage: page }) => {
    await openHome(page);
    await page.getByTitle('Settings').click();

    await page.getByRole('button', { name: 'About' }).click();
    await expect(page.getByText('Source Browser')).toBeVisible();

    await page.getByRole('button', { name: 'Open hashtree repository' }).click();

    await expect.poll(async () => (await getInvocationsFor(page, 'create_htree_webview')).length).toBe(1);
    const calls = await getInvocationsFor(page, 'create_htree_webview');
    expect(calls.length).toBe(1);
    expect(calls[0].args.npub).toBe(DISTRIBUTED_OWNER);
    expect(calls[0].args.treename).toBe('git');
    expect(calls[0].args.path).toBe('/');
    expect(calls[0].args.fragment).toBe(`/${DISTRIBUTED_OWNER}/hashtree`);
  });

  test('autostart toggle sends invoke', async ({ tauriPage: page }) => {
    await openHome(page);
    await page.getByTitle('Settings').click();

    // Click the toggle
    await page.getByLabel('Toggle launch at startup').click();

    // Since autostart plugin is mocked, the toggle should have called
    // through the import('@tauri-apps/plugin-autostart') path which
    // will fail in browser context. The UI should handle the error gracefully.
    // Just verify no crash occurred.
    await expect(page.getByText('Launch at startup')).toBeVisible();
  });

  test('clear history button clears and shows feedback', async ({ tauriPage: page }) => {
    await openHome(page);
    await page.getByTitle('Settings').click();

    await page.getByRole('button', { name: 'Privacy' }).click();

    await expect(page.getByText('Browsing history', { exact: true })).toBeVisible();

    // Click clear history
    await page.getByRole('button', { name: 'Clear history' }).click();

    // Should show "Cleared!" feedback
    await expect(page.getByText('Cleared!')).toBeVisible();

    // Verify the command was invoked
    const calls = await getInvocationsFor(page, 'clear_history');
    expect(calls.length).toBe(1);

    // After 2 seconds, the button should reappear
    await expect(page.getByRole('button', { name: 'Clear history' })).toBeVisible({ timeout: 3000 });
  });
});
