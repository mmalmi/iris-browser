import { createHash } from 'node:crypto';
import { access, mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const appRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const tempDir = await mkdtemp(path.join(tmpdir(), 'iris-icon-check-'));

const generatedIconFiles = [
  ...[
    'AppIcon-20x20@1x.png',
    'AppIcon-20x20@2x-1.png',
    'AppIcon-20x20@2x.png',
    'AppIcon-20x20@3x.png',
    'AppIcon-29x29@1x.png',
    'AppIcon-29x29@2x-1.png',
    'AppIcon-29x29@2x.png',
    'AppIcon-29x29@3x.png',
    'AppIcon-40x40@1x.png',
    'AppIcon-40x40@2x-1.png',
    'AppIcon-40x40@2x.png',
    'AppIcon-40x40@3x.png',
    'AppIcon-60x60@2x.png',
    'AppIcon-60x60@3x.png',
    'AppIcon-76x76@1x.png',
    'AppIcon-76x76@2x.png',
    'AppIcon-83.5x83.5@2x.png',
    'AppIcon-512@2x.png',
  ].map((name) => ({
    expected: path.join(appRoot, 'src-tauri/gen/apple/Assets.xcassets/AppIcon.appiconset', name),
    actual: path.join(tempDir, 'ios', name),
  })),
  ...[
    'mipmap-anydpi-v26/ic_launcher.xml',
    'mipmap-hdpi/ic_launcher.png',
    'mipmap-hdpi/ic_launcher_foreground.png',
    'mipmap-hdpi/ic_launcher_round.png',
    'mipmap-mdpi/ic_launcher.png',
    'mipmap-mdpi/ic_launcher_foreground.png',
    'mipmap-mdpi/ic_launcher_round.png',
    'mipmap-xhdpi/ic_launcher.png',
    'mipmap-xhdpi/ic_launcher_foreground.png',
    'mipmap-xhdpi/ic_launcher_round.png',
    'mipmap-xxhdpi/ic_launcher.png',
    'mipmap-xxhdpi/ic_launcher_foreground.png',
    'mipmap-xxhdpi/ic_launcher_round.png',
    'mipmap-xxxhdpi/ic_launcher.png',
    'mipmap-xxxhdpi/ic_launcher_foreground.png',
    'mipmap-xxxhdpi/ic_launcher_round.png',
    'values/ic_launcher_background.xml',
  ].map((name) => ({
    expected: path.join(appRoot, 'src-tauri/gen/android/app/src/main/res', name),
    actual: path.join(tempDir, 'android', name),
  })),
];

const sha256 = async (filePath) => {
  const data = await readFile(filePath);
  return createHash('sha256').update(data).digest('hex');
};

const exists = async (filePath) => {
  try {
    await access(filePath);
    return true;
  } catch {
    return false;
  }
};

const result = spawnSync('pnpm', ['tauri', 'icon', 'src-tauri/icons/icon.png', '-o', tempDir], {
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
  const [expectedHash, actualHash] = await Promise.all([sha256(file.expected), sha256(file.actual)]);
  if (expectedHash !== actualHash) {
    mismatches.push(path.relative(appRoot, file.expected));
  }
}

await rm(tempDir, { recursive: true, force: true });

if (mismatches.length > 0) {
  console.error('Generated Tauri mobile icons are out of sync:');
  for (const mismatch of mismatches) {
    console.error(`- ${mismatch}`);
  }
  process.exit(1);
}

console.log('Generated Tauri mobile icons match src-tauri/icons/icon.png.');
