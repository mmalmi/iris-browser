import test from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import os from 'node:os'
import { join } from 'node:path'

import {
  assetPrefixForTag,
  buildReleaseManifest,
  describeAsset,
  normalizeTag,
  readCargoPackageVersionTag,
  renderReleaseNotes,
} from '../scripts/local-release-lib.mjs'

test('assetPrefixForTag normalizes the release tag', () => {
  assert.equal(assetPrefixForTag('0.2.14'), 'iris-v0.2.14')
  assert.equal(assetPrefixForTag('v0.2.14'), 'iris-v0.2.14')
  assert.equal(normalizeTag('0.2.14'), 'v0.2.14')
})

test('readCargoPackageVersionTag reads the app version from Cargo.toml', () => {
  const cargoToml = [
    '[package]',
    'name = "iris"',
    'version = "0.1.4"',
    'edition = "2021"',
  ].join('\n')

  assert.equal(readCargoPackageVersionTag(cargoToml), 'v0.1.4')
})

test('describeAsset labels Iris desktop installers and bundles', () => {
  assert.equal(describeAsset('iris-v0.2.14-macos-arm64.zip'), 'macOS Apple Silicon app')
  assert.equal(describeAsset('iris-v0.2.14-linux-x86_64.AppImage'), 'Linux x64 AppImage')
  assert.equal(describeAsset('iris-v0.2.14-linux-x86_64.deb'), 'Linux x64 Debian package')
  assert.equal(describeAsset('iris-v0.2.14-windows-x64-setup.exe'), 'Windows x64 installer')
})

test('buildReleaseManifest sorts assets by file name', () => {
  const tempDir = mkdtempSync(join(os.tmpdir(), 'iris-release-lib-'))
  try {
    const bPath = join(tempDir, 'iris-v0.2.14-windows-x64-setup.exe')
    const aPath = join(tempDir, 'iris-v0.2.14-linux-x86_64.deb')
    writeFileSync(bPath, 'bbbb')
    writeFileSync(aPath, 'aaaa')

    const manifest = buildReleaseManifest({
      tag: 'v0.2.14',
      commit: 'abc123',
      createdAt: 1,
      assetPaths: [bPath, aPath],
    })

    assert.deepEqual(
      manifest.assets.map((asset) => asset.name),
      ['iris-v0.2.14-linux-x86_64.deb', 'iris-v0.2.14-windows-x64-setup.exe'],
    )
  } finally {
    rmSync(tempDir, { recursive: true, force: true })
  }
})

test('renderReleaseNotes leads with installation instructions by platform', () => {
  const notes = renderReleaseNotes({
    tag: 'v0.2.14',
    commit: 'abc123',
    assetNames: [
      'iris-v0.2.14-windows-x64-setup.exe',
      'iris-v0.2.14-linux-x86_64.AppImage',
      'iris-v0.2.14-linux-x86_64.deb',
      'iris-v0.2.14-macos-arm64.zip',
    ],
    builtLines: ['Ran pnpm build and test:icons for Iris.'],
  })

  assert.ok(notes.startsWith('## Installation\n'))
  assert.ok(notes.includes('### macOS\n'))
  assert.ok(notes.includes('Download `iris-v0.2.14-macos-arm64.zip`, unzip it, and move `Iris.app` into `/Applications`.'))
  assert.ok(notes.includes('### Linux\n'))
  assert.ok(notes.includes('Run `chmod +x iris-v0.2.14-linux-x86_64.AppImage && ./iris-v0.2.14-linux-x86_64.AppImage`.'))
  assert.ok(notes.includes('Install `iris-v0.2.14-linux-x86_64.deb` with `sudo apt install ./iris-v0.2.14-linux-x86_64.deb`.'))
  assert.ok(notes.includes('### Windows\n'))
  assert.ok(notes.includes('Run `iris-v0.2.14-windows-x64-setup.exe`.'))
  assert.ok(notes.includes('## Downloads\n'))
  assert.ok(notes.includes('## Build Info\n'))
  assert.ok(notes.includes('- Release `v0.2.14` from commit `abc123`.'))
  assert.ok(notes.includes('- Ran pnpm build and test:icons for Iris.'))
  assert.ok(notes.indexOf('## Installation') < notes.indexOf('## Downloads'))
})
