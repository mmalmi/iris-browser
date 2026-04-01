import { createHash } from 'node:crypto';
import { access, mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const appRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const tempDir = await mkdtemp(path.join(tmpdir(), 'iris-bundle-icon-check-'));

const generatedIconFiles = [
  '32x32.png',
  '64x64.png',
  '128x128.png',
  '128x128@2x.png',
  'Square30x30Logo.png',
  'Square44x44Logo.png',
  'Square71x71Logo.png',
  'Square89x89Logo.png',
  'Square107x107Logo.png',
  'Square142x142Logo.png',
  'Square150x150Logo.png',
  'Square284x284Logo.png',
  'Square310x310Logo.png',
  'StoreLogo.png',
  'icon.icns',
  'icon.ico',
].map((name) => ({
  expected: path.join(appRoot, 'src-tauri/icons', name),
  actual: path.join(tempDir, name),
}));

const sha256 = async (filePath) => {
  const data = await readFile(filePath);
  return createHash('sha256').update(data).digest('hex');
};

const normalizedIcnsHash = async (filePath) => {
  const tempDir = await mkdtemp(path.join(tmpdir(), 'iris-bundle-iconset-'));
  const iconsetDir = path.join(tempDir, 'generated.iconset');
  try {
    const result = spawnSync('iconutil', ['--convert', 'iconset', '--output', iconsetDir, filePath], {
      cwd: appRoot,
      stdio: 'inherit',
    });
    if (result.status !== 0) {
      process.exit(result.status ?? 1);
    }

    const iconsetFiles = [
      'icon_16x16.png',
      'icon_16x16@2x.png',
      'icon_32x32.png',
      'icon_32x32@2x.png',
      'icon_128x128.png',
      'icon_128x128@2x.png',
      'icon_256x256.png',
      'icon_256x256@2x.png',
      'icon_512x512.png',
      'icon_512x512@2x.png',
    ];

    const hashes = await Promise.all(iconsetFiles.map(async (name) => sha256(path.join(iconsetDir, name))));
    return createHash('sha256').update(hashes.join('\n')).digest('hex');
  } finally {
    await rm(tempDir, { recursive: true, force: true });
  }
};

const exists = async (filePath) => {
  try {
    await access(filePath);
    return true;
  } catch {
    return false;
  }
};

const result = spawnSync('pnpm', ['tauri', 'icon', 'src-tauri/icons/bundle-icon.png', '-o', tempDir], {
  cwd: appRoot,
  stdio: 'inherit',
});

if (result.status !== 0) {
  await rm(tempDir, { recursive: true, force: true });
  process.exit(result.status ?? 1);
}

const mismatches = [];

for (const file of generatedIconFiles) {
  const [expectedExists, actualExists] = await Promise.all([exists(file.expected), exists(file.actual)]);
  if (!expectedExists || !actualExists) {
    mismatches.push(path.relative(appRoot, file.expected));
    continue;
  }

  const [expectedHash, actualHash] = file.expected.endsWith('.icns')
    ? await Promise.all([normalizedIcnsHash(file.expected), normalizedIcnsHash(file.actual)])
    : await Promise.all([sha256(file.expected), sha256(file.actual)]);
  if (expectedHash !== actualHash) {
    mismatches.push(path.relative(appRoot, file.expected));
  }
}

await rm(tempDir, { recursive: true, force: true });

if (mismatches.length > 0) {
  console.error('Generated Tauri bundle icons are out of sync:');
  for (const mismatch of mismatches) {
    console.error(`- ${mismatch}`);
  }
  process.exit(1);
}

console.log('Generated Tauri bundle icons match src-tauri/icons/bundle-icon.png.');
