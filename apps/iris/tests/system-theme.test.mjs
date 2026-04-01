import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const appRoot = path.resolve(__dirname, '..');

test('iris shell theme uses CSS variables and system theme metadata', () => {
  const unoConfig = fs.readFileSync(path.join(appRoot, 'uno.config.ts'), 'utf8');
  assert.match(unoConfig, /0: 'rgb\(var\(--surface-0\) \/ <alpha-value>\)'/);
  assert.match(unoConfig, /1: 'rgb\(var\(--surface-1\) \/ <alpha-value>\)'/);
  assert.match(unoConfig, /1: 'rgb\(var\(--text-1\) \/ <alpha-value>\)'/);

  const themeCss = fs.readFileSync(path.join(appRoot, 'src/system-theme.css'), 'utf8');
  assert.match(themeCss, /color-scheme: light dark;/);
  assert.match(themeCss, /--surface-0: 245 245 245;/);
  assert.match(themeCss, /--surface-0: 15 15 15;/);

  const html = fs.readFileSync(path.join(appRoot, 'index.html'), 'utf8');
  assert.match(html, /<meta name="theme-color" content="#0f0f0f" media="\(prefers-color-scheme: dark\)">/);
  assert.match(html, /<meta name="theme-color" content="#f5f5f5" media="\(prefers-color-scheme: light\)">/);
  assert.match(html, /<link rel="stylesheet" href="\.\/src\/system-theme\.css" \/>/);
  assert.doesNotMatch(html, /<html lang="en" class="dark">/);
});
