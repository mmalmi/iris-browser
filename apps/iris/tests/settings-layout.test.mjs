import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const settingsSource = fs.readFileSync(
  path.join(__dirname, '..', 'src', 'components', 'Settings.svelte'),
  'utf8',
);

test('settings navigation stays plain without grouped marketing copy', () => {
  assert.match(settingsSource, /label: 'App'/);
  assert.match(settingsSource, /label: 'Privacy'/);
  assert.match(settingsSource, /label: 'Users'/);
  assert.match(settingsSource, /label: 'Network'/);
  assert.match(settingsSource, /label: 'About'/);
  assert.doesNotMatch(settingsSource, /Startup behavior and shell-level preferences/);
  assert.doesNotMatch(settingsSource, /Local browsing history and device-only data/);
  assert.doesNotMatch(settingsSource, /Stored Nostr accounts and private-key export/);
  assert.doesNotMatch(settingsSource, /Daemon, relays, Blossom, and peer transport/);
  assert.doesNotMatch(settingsSource, /Build info, source links, and app actions/);
  assert.doesNotMatch(settingsSource, /Device behavior, local privacy controls, daemon details, and source links\./);
  assert.doesNotMatch(settingsSource, /const tabGroups =/);
  assert.doesNotMatch(settingsSource, /mx-auto w-full max-w-md/);
  assert.doesNotMatch(settingsSource, /mx-auto w-full max-w-4xl/);
  assert.doesNotMatch(settingsSource, /mx-auto max-w-3xl/);
});

test('settings navigation keeps colored icon pills for each top-level section', () => {
  assert.match(settingsSource, /bg-accent\/12 text-accent ring-1 ring-accent\/20/);
  assert.match(settingsSource, /bg-rose-500\/12 text-rose-500 ring-1 ring-rose-500\/20/);
  assert.match(settingsSource, /bg-emerald-500\/12 text-emerald-500 ring-1 ring-emerald-500\/20/);
  assert.match(settingsSource, /bg-sky-500\/12 text-sky-500 ring-1 ring-sky-500\/20/);
  assert.match(settingsSource, /bg-amber-500\/12 text-amber-500 ring-1 ring-amber-500\/20/);
});
