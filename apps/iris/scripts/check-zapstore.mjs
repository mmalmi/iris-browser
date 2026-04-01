import assert from 'node:assert/strict';
import { access, readFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const appRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const zapstorePath = path.join(appRoot, 'zapstore.yaml');
const packageJsonPath = path.join(appRoot, 'package.json');
const tauriConfigPath = path.join(appRoot, 'src-tauri/tauri.conf.json');
const expectedRepository = 'https://github.com/mmalmi/hashtree';
const expectedIcon = 'src-tauri/gen/android/app/src/main/res/mipmap-xxxhdpi/ic_launcher.png';
const expectedReleaseSource = 'src-tauri/gen/android/app/build/outputs/apk/universal/release/*.apk';
const expectedSummary = 'Native shell for hashtree apps with embedded htree daemon';
const expectedBlossomServers = [
  'https://cdn.zapstore.dev',
  'https://blossom.band',
  'https://blossom.iris.to',
];

function parseSimpleYaml(text) {
  const result = {};
  let activeListKey = null;

  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trimEnd();
    if (!line.trim() || line.trimStart().startsWith('#')) {
      continue;
    }

    if (line.startsWith('  - ')) {
      if (!activeListKey || !Array.isArray(result[activeListKey])) {
        throw new Error(`Unexpected list item outside a list: ${line}`);
      }
      result[activeListKey].push(line.slice(4).trim());
      continue;
    }

    const match = /^([A-Za-z_][A-Za-z0-9_]*):(.*)$/.exec(line);
    if (!match) {
      throw new Error(`Unsupported YAML line: ${line}`);
    }

    const [, key, rest] = match;
    const value = rest.trim();
    if (value === '') {
      result[key] = [];
      activeListKey = key;
    } else {
      result[key] = value;
      activeListKey = null;
    }
  }

  return result;
}

async function assertExists(relativePath) {
  await access(path.join(appRoot, relativePath));
}

const [zapstoreRaw, packageJsonRaw, tauriConfigRaw] = await Promise.all([
  readFile(zapstorePath, 'utf8'),
  readFile(packageJsonPath, 'utf8'),
  readFile(tauriConfigPath, 'utf8'),
]);

const zapstore = parseSimpleYaml(zapstoreRaw);
const packageJson = JSON.parse(packageJsonRaw);
const tauriConfig = JSON.parse(tauriConfigRaw);

assert.equal(zapstore.name, tauriConfig.productName, 'Zapstore name should match Tauri productName');
assert.equal(zapstore.identifier, tauriConfig.identifier, 'Zapstore identifier should match Tauri identifier');
assert.equal(zapstore.license, packageJson.license, 'Zapstore license should match package.json');
assert.equal(zapstore.website, packageJson.homepage, 'Zapstore website should match package.json homepage');
assert.equal(zapstore.repository, expectedRepository, 'Zapstore repository should point at the GitHub repo');
assert.equal(zapstore.summary, expectedSummary, 'Zapstore summary drifted');
assert.equal(zapstore.description, expectedSummary, 'Zapstore description drifted');
assert.equal(zapstore.icon, expectedIcon, 'Zapstore icon should use the generated Android launcher icon');
assert.deepEqual(zapstore.tags, ['nostr'], 'Zapstore tags drifted');
assert.equal(zapstore.release_source, expectedReleaseSource, 'Zapstore release source drifted');
assert.deepEqual(zapstore.metadata_sources, ['github'], 'Zapstore metadata sources drifted');
assert.deepEqual(zapstore.blossom_servers, expectedBlossomServers, 'Zapstore blossom servers drifted');

await assertExists(zapstore.icon);

console.log('Zapstore metadata matches the Iris Tauri app configuration.');
