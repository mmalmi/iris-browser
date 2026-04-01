import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const appRoot = path.resolve(__dirname, '..');

const builtInLauncherApps = [
  ['Iris Files', '/iris-files-icon.svg'],
  ['Iris Video', '/iris-video-icon.svg'],
  ['Iris Docs', '/iris-docs-icon.svg'],
  ['Iris Git', '/iris-git-icon.svg'],
  ['Iris Maps', '/iris-maps-icon.svg'],
  ['Iris Boards', '/iris-boards-icon.svg'],
  ['Iris Meet', '/iris-meet-icon.svg'],
];

test('iris launcher uses distinct icons for iris-files app suggestions', () => {
  const appsSource = fs.readFileSync(path.join(appRoot, 'src', 'lib', 'apps.ts'), 'utf8');

  for (const [name, iconPath] of builtInLauncherApps) {
    const iconKey = name.replace(/^Iris /, '').toLowerCase();
    assert.match(appsSource, new RegExp(`${iconKey}: '${iconPath.replace('/', '\\/')}'`));
    assert.match(appsSource, new RegExp(`name: '${name}', icon: irisLauncherIcons\\.${iconKey}`));
    assert.ok(fs.existsSync(path.join(appRoot, 'public', iconPath.slice(1))), `${iconPath} should exist`);
  }

  assert.match(appsSource, /\{ url: builtInAppUrl\('iris-client-site'\), name: 'Iris Social', icon: '\/iris-logo\.png'/);
  assert.match(appsSource, /name: 'Iris Chat', icon: '\/iris-logo\.png'/);
  assert.match(appsSource, /name: 'Iris Meet', icon: irisLauncherIcons\.meet/);
});
