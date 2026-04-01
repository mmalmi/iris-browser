import { access, readFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const appRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const configPath = path.join(appRoot, 'src-tauri', 'tauri.conf.json');
const trayIconPath = path.join(appRoot, 'src-tauri', 'icons', 'tray-icon.png');

const config = JSON.parse(await readFile(configPath, 'utf8'));
const trayIcon = config.app?.trayIcon;

if (!trayIcon) {
  console.error('Missing app.trayIcon in tauri.conf.json.');
  process.exit(1);
}

if (trayIcon.iconPath !== 'icons/tray-icon.png') {
  console.error(`Tray icon path must be icons/tray-icon.png, found ${trayIcon.iconPath ?? '<missing>'}.`);
  process.exit(1);
}

if (trayIcon.iconAsTemplate !== true) {
  console.error('Tray icon must stay configured as a macOS template icon.');
  process.exit(1);
}

try {
  await access(trayIconPath);
} catch {
  console.error(`Missing tray icon asset at ${path.relative(appRoot, trayIconPath)}.`);
  process.exit(1);
}

console.log('Tray icon config points at the dedicated template asset.');
