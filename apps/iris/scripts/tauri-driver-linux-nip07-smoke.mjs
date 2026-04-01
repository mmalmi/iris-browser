import { createServer } from 'node:http';
import { mkdir, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn, spawnSync } from 'node:child_process';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const appDir = path.resolve(__dirname, '..');
const launcherPath = path.join(appDir, 'scripts', 'launch-linux-debug-iris.sh');
const smokeMode = process.env.IRIS_NIP07_SMOKE_MODE ?? 'probe';
const isLiveSmoke = smokeMode === 'live';
const distributedOwner = 'npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm';
const socialUrl = process.env.IRIS_NIP07_SOCIAL_URL ?? `htree://${distributedOwner}/iris-client-site`;
const jumbleUrl = process.env.IRIS_NIP07_JUMBLE_URL ?? 'https://jumble.social/';
const artifactsDir = process.env.IRIS_NATIVE_ARTIFACT_DIR ?? path.join(
  appDir,
  'test-results',
  isLiveSmoke ? 'native-nip07-live' : 'native-nip07',
);
const webdriverPort = Number(process.env.TAURI_DRIVER_PORT ?? 4444);
const automationPort = Number(process.env.IRIS_AUTOMATION_PORT ?? 21977);
const probePort = Number(process.env.IRIS_NIP07_SMOKE_PORT ?? 21461);
const webdriverBase = `http://127.0.0.1:${webdriverPort}`;
const automationBase = `http://127.0.0.1:${automationPort}/automation`;
const elementRefKey = 'element-6066-11e4-a52e-4f735466cecf';
const firstSecretHex = '1111111111111111111111111111111111111111111111111111111111111111';
const secondPubkey = '466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f27';
const probeBase = `http://127.0.0.1:${probePort}`;
const alphaUrl = `${probeBase}/alpha`;
const betaUrl = `${probeBase}/beta`;
const probeHostQuery = `127.0.0.1:${probePort}`;

let driverProcess = null;
let sessionId = null;
let probeServer = null;

function fail(message) {
  throw new Error(message);
}

function which(binary) {
  const result = spawnSync('bash', ['-lc', `command -v ${binary}`], {
    encoding: 'utf8',
  });
  if (result.status !== 0) {
    return null;
  }
  return result.stdout.trim() || null;
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: appDir,
    stdio: 'inherit',
    env: process.env,
    ...options,
  });
  if (result.status !== 0) {
    fail(`${command} ${args.join(' ')} failed with exit code ${result.status ?? 'unknown'}`);
  }
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function request(method, pathname, body) {
  const response = await fetch(`${webdriverBase}${pathname}`, {
    method,
    headers: body ? { 'content-type': 'application/json' } : undefined,
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await response.text();
  const payload = text ? JSON.parse(text) : {};
  if (!response.ok) {
    fail(`${method} ${pathname} failed: ${response.status} ${JSON.stringify(payload)}`);
  }
  return payload;
}

async function waitFor(fn, description, timeoutMs = 30000, intervalMs = 250) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;
  while (Date.now() < deadline) {
    try {
      return await fn();
    } catch (error) {
      lastError = error;
      await sleep(intervalMs);
    }
  }
  if (lastError) {
    throw lastError;
  }
  fail(`Timed out waiting for ${description}`);
}

async function getAutomationState() {
  const response = await fetch(`${automationBase}/state`);
  if (!response.ok) {
    fail(`automation state returned ${response.status}`);
  }
  return await response.json();
}

async function waitForAutomationState(predicate, description, timeoutMs = 30000) {
  return waitFor(async () => {
    const state = await getAutomationState();
    if (!predicate(state)) {
      fail(`waiting for ${description}, current state: ${JSON.stringify(state)}`);
    }
    return state;
  }, description, timeoutMs);
}

async function postAutomationCommand(command) {
  const response = await fetch(`${automationBase}/command`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(command),
  });
  if (!response.ok) {
    const body = await response.text();
    fail(`automation command failed: ${response.status} ${body}`);
  }
}

async function postAutomationNip07Probe(request) {
  const response = await fetch(`${automationBase}/nip07-probe`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(request),
  });
  if (!response.ok) {
    const body = await response.text();
    fail(`automation nip07 probe failed: ${response.status} ${body}`);
  }
}

async function createSession() {
  const payload = await request('POST', '/session', {
    capabilities: {
      alwaysMatch: {
        'tauri:options': {
          application: launcherPath,
        },
      },
    },
  });

  sessionId = payload.value?.sessionId ?? payload.sessionId;
  if (!sessionId) {
    fail(`WebDriver did not return a session id: ${JSON.stringify(payload)}`);
  }
}

async function deleteSession() {
  if (!sessionId) {
    return;
  }
  try {
    await request('DELETE', `/session/${sessionId}`);
  } finally {
    sessionId = null;
  }
}

async function findElement(using, value) {
  const payload = await request('POST', `/session/${sessionId}/element`, { using, value });
  const element = payload.value;
  const elementId = element?.[elementRefKey] ?? element?.ELEMENT;
  if (!elementId) {
    fail(`WebDriver did not return an element id for ${using}=${value}`);
  }
  return elementId;
}

async function tryFindElement(using, value) {
  try {
    return await findElement(using, value);
  } catch {
    return null;
  }
}

async function clickElement(elementId) {
  await request('POST', `/session/${sessionId}/element/${elementId}/click`, {});
}

async function clearElement(elementId) {
  await request('POST', `/session/${sessionId}/element/${elementId}/clear`, {});
}

async function typeIntoElement(elementId, text) {
  await request('POST', `/session/${sessionId}/element/${elementId}/value`, {
    text,
    value: Array.from(text),
  });
}

async function takeScreenshot(filename) {
  await mkdir(artifactsDir, { recursive: true });
  const target = path.join(artifactsDir, filename);
  const usesNativePopupSurface = /native-menu|permission-dialog/.test(filename);
  const wantsChildComposite = /(alpha|beta)-loaded/.test(filename);
  const importBinary = getFramebufferCaptureBinary();

  if (usesNativePopupSurface && importBinary) {
    const captured = captureFramebufferToFile(importBinary, target);
    if (captured) {
      return;
    }
  }

  await writeWebdriverScreenshot(target);

  if (!wantsChildComposite || !importBinary) {
    return;
  }

  const framebufferPath = path.join(
    artifactsDir,
    `${path.parse(filename).name}.framebuffer.png`,
  );
  const captured = captureFramebufferToFile(importBinary, framebufferPath);
  if (!captured) {
    await rm(framebufferPath, { force: true }).catch(() => {});
    return;
  }

  try {
    const state = await getAutomationState();
    compositeChildViewportOntoScreenshot(target, framebufferPath, state);
  } finally {
    await rm(framebufferPath, { force: true }).catch(() => {});
  }
}

async function writeWebdriverScreenshot(target) {
  const payload = await request('GET', `/session/${sessionId}/screenshot`);
  const encoded = payload.value;
  if (!encoded) {
    fail('WebDriver screenshot response did not contain image data');
  }
  await writeFile(target, Buffer.from(encoded, 'base64'));
}

function getFramebufferCaptureBinary() {
  if (process.env.IRIS_NATIVE_CAPTURE_FRAMEBUFFER === '0') {
    return null;
  }
  return which('import');
}

function captureFramebufferToFile(importBinary, filename) {
  const target = path.isAbsolute(filename) ? filename : path.join(artifactsDir, filename);
  const irisWindowId = xdotoolSearchByName('Iris').at(-1) ?? 'root';
  const result = spawnSync(importBinary, ['-window', irisWindowId, target], {
    cwd: appDir,
    env: process.env,
    encoding: 'utf8',
  });
  if (result.status === 0) {
    return true;
  }
  console.warn(`Framebuffer capture failed: ${result.stderr || result.stdout || result.status}`);
  return false;
}

function getImageMagickBinary() {
  return which('magick') ?? which('convert');
}

function readImageDimensions(imagePath) {
  const identifyBinary = which('identify');
  if (!identifyBinary) {
    fail('identify is required to inspect screenshot dimensions');
  }
  const result = spawnSync(identifyBinary, ['-format', '%w %h', imagePath], {
    cwd: appDir,
    env: process.env,
    encoding: 'utf8',
  });
  if (result.status !== 0) {
    fail(`identify failed for ${imagePath}: ${result.stderr || result.stdout || result.status}`);
  }
  const [widthText, heightText] = result.stdout.trim().split(/\s+/);
  const width = Number(widthText);
  const height = Number(heightText);
  if (!Number.isFinite(width) || !Number.isFinite(height)) {
    fail(`Could not parse identify output for ${imagePath}: ${result.stdout}`);
  }
  return { width, height };
}

function compositeChildViewportOntoScreenshot(basePath, framebufferPath, state) {
  const magickBinary = getImageMagickBinary();
  if (!magickBinary) {
    fail('ImageMagick is required to composite child-webview screenshots');
  }

  const baseSize = readImageDimensions(basePath);
  const framebufferSize = readImageDimensions(framebufferPath);
  const sourceHeight = Math.max(
    1,
    Math.min(
      framebufferSize.height,
      Number(state?.childViewportHeight ?? 0) || framebufferSize.height,
    ),
  );
  const sourceY = Math.max(0, framebufferSize.height - sourceHeight);
  const destY = Math.max(
    0,
    Math.min(baseSize.height - 1, Number(state?.toolbarHeight ?? 0) || 0),
  );
  const destHeight = Math.max(1, baseSize.height - destY);
  const args = magickBinary.endsWith('/magick') || magickBinary === 'magick'
    ? [
        basePath,
        '(',
        framebufferPath,
        '-crop',
        `${framebufferSize.width}x${sourceHeight}+0+${sourceY}`,
        '-resize',
        `${baseSize.width}x${destHeight}!`,
        ')',
        '-geometry',
        `+0+${destY}`,
        '-composite',
        basePath,
      ]
    : [
        basePath,
        '(',
        framebufferPath,
        '-crop',
        `${framebufferSize.width}x${sourceHeight}+0+${sourceY}`,
        '-resize',
        `${baseSize.width}x${destHeight}!`,
        ')',
        '-geometry',
        `+0+${destY}`,
        '-composite',
        basePath,
      ];
  const result = spawnSync(magickBinary, args, {
    cwd: appDir,
    env: process.env,
    encoding: 'utf8',
  });
  if (result.status !== 0) {
    fail(`ImageMagick composite failed: ${result.stderr || result.stdout || result.status}`);
  }
}

function maybeReexecUnderXvfb() {
  if (process.env.DISPLAY || process.env.__IRIS_NATIVE_UNDER_XVFB === '1') {
    return;
  }
  const xvfbRun = which('xvfb-run');
  if (!xvfbRun) {
    fail('DISPLAY is not set and xvfb-run is not installed. Run this on Linux with Xvfb or inside a desktop-capable container.');
  }

  const result = spawnSync(
    xvfbRun,
    ['-a', process.execPath, __filename],
    {
      cwd: appDir,
      stdio: 'inherit',
      env: {
        ...process.env,
        __IRIS_NATIVE_UNDER_XVFB: '1',
        GDK_SCALE: process.env.GDK_SCALE ?? '1',
        GDK_DPI_SCALE: process.env.GDK_DPI_SCALE ?? '1',
      },
    },
  );
  process.exit(result.status ?? 1);
}

function startTauriDriver(nativeDriverPath) {
  driverProcess = spawn(
    'tauri-driver',
    ['--port', String(webdriverPort), '--native-driver', nativeDriverPath],
    {
      cwd: appDir,
      stdio: ['ignore', 'pipe', 'pipe'],
      env: process.env,
    },
  );

  driverProcess.stdout.on('data', (chunk) => {
    process.stdout.write(`[tauri-driver] ${chunk}`);
  });
  driverProcess.stderr.on('data', (chunk) => {
    process.stderr.write(`[tauri-driver] ${chunk}`);
  });
}

async function waitForWebDriver() {
  await waitFor(async () => {
    const response = await fetch(`${webdriverBase}/status`);
    if (!response.ok) {
      fail(`webdriver status returned ${response.status}`);
    }
    const payload = await response.json();
    if (!payload.value?.ready) {
      fail(`webdriver not ready: ${JSON.stringify(payload)}`);
    }
    return payload;
  }, 'tauri-driver to be ready');
}

function bodyIncludesPage(state, pageName) {
  const body = typeof state.childBodyText === 'string' ? state.childBodyText : '';
  return body.includes(`PASS ${pageName}`) && body.includes('NIP04:ok') && body.includes('NIP44:ok');
}

function currentUrlMatches(state, url) {
  return state.currentView === 'webview' && typeof state.currentUrl === 'string' && state.currentUrl === url;
}

function currentUrlMatchesJumble(state) {
  const currentUrl = typeof state.currentUrl === 'string' ? state.currentUrl : '';
  return state.currentView === 'webview' && currentUrl.startsWith('https://jumble.social');
}

function currentUrlMatchesSocial(state) {
  const currentUrl = typeof state.currentUrl === 'string' ? state.currentUrl : '';
  return state.currentView === 'webview' &&
    (
      currentUrl === socialUrl ||
      currentUrl === `${socialUrl}/` ||
      currentUrl === `${socialUrl}/index.html` ||
      currentUrl.startsWith('htree://nhash1') ||
      currentUrl.includes('/iris-client-site/') ||
      currentUrl.includes('/iris-client-site/index.html')
    );
}

function liveProbePassed(state, scenario) {
  const body = typeof state.childBodyText === 'string' ? state.childBodyText : '';
  const title = typeof state.childDocumentTitle === 'string' ? state.childDocumentTitle : '';
  if (
    (body.includes('IRIS NIP07 FAIL') || title.includes('IRIS NIP07 FAIL')) &&
    (body.includes(`SCENARIO:${scenario}`) || title.includes(scenario))
  ) {
    fail(`live NIP-07 probe failed for ${scenario}: ${JSON.stringify(state)}`);
  }
  return title.includes(`IRIS NIP07 PASS ${scenario}`) || (
    body.includes('IRIS NIP07 PASS') &&
    body.includes(`SCENARIO:${scenario}`)
  );
}

function extractPubkey(state) {
  const body = typeof state.childBodyText === 'string' ? state.childBodyText : '';
  const match = body.match(/PUBKEY:([0-9a-f]{64})/i);
  return match?.[1] ?? null;
}

function xdotoolSearchByName(name) {
  const result = spawnSync('xdotool', ['search', '--name', name], {
    cwd: appDir,
    env: process.env,
    encoding: 'utf8',
  });
  if (result.status !== 0) {
    return [];
  }
  return result.stdout
    .trim()
    .split(/\s+/)
    .filter(Boolean);
}

async function waitForWindowNamed(name, description, timeoutMs = 30000) {
  return waitFor(async () => {
    const ids = xdotoolSearchByName(name);
    if (ids.length === 0) {
      fail(`waiting for ${description}`);
    }
    return ids[ids.length - 1];
  }, description, timeoutMs);
}

function xdotoolKey(windowId, key) {
  const activate = spawnSync('xdotool', ['windowactivate', '--sync', windowId], {
    cwd: appDir,
    env: process.env,
    encoding: 'utf8',
  });
  if (activate.status !== 0) {
    fail(`xdotool failed to activate window ${windowId}: ${activate.stderr || activate.stdout}`);
  }
  const result = spawnSync('xdotool', ['key', '--window', windowId, key], {
    cwd: appDir,
    env: process.env,
    encoding: 'utf8',
  });
  if (result.status !== 0) {
    fail(`xdotool failed to send ${key}: ${result.stderr || result.stdout}`);
  }
}

function xdotoolClickWindow(windowId, x, y) {
  const activate = spawnSync('xdotool', ['windowactivate', '--sync', windowId], {
    cwd: appDir,
    env: process.env,
    encoding: 'utf8',
  });
  if (activate.status !== 0) {
    fail(`xdotool failed to activate window ${windowId}: ${activate.stderr || activate.stdout}`);
  }
  const result = spawnSync(
    'xdotool',
    ['mousemove', '--window', windowId, String(x), String(y), 'click', '1'],
    {
      cwd: appDir,
      env: process.env,
      encoding: 'utf8',
    },
  );
  if (result.status !== 0) {
    fail(`xdotool failed to click window ${windowId}: ${result.stderr || result.stdout}`);
  }
}

async function acceptPermissionPromptsUntil(
  predicate,
  description,
  screenshotName = 'nip07-permission-dialog.png',
  timeoutMs = 120000,
) {
  let screenshotTaken = false;
  return waitFor(async () => {
    const state = await getAutomationState();
    if (predicate(state)) {
      return state;
    }

    const ui = await (async () => {
      const dialogWindow = xdotoolSearchByName('NIP-07 Permission').at(-1) ?? null;
      if (dialogWindow) {
        return { kind: 'native-window', target: dialogWindow };
      }

      const domButton = await tryFindElement(
        'css selector',
        "[data-testid='nip07-permission-allow-session']",
      );
      if (domButton) {
        return { kind: 'shell-overlay', target: domButton };
      }

      return null;
    })();

    if (!ui) {
      fail(`waiting for ${description}`);
    }

    if (!screenshotTaken) {
      await takeScreenshot(screenshotName);
      screenshotTaken = true;
    }

    if (ui.kind === 'native-window') {
      xdotoolKey(ui.target, 'Return');
    } else {
      await clickElement(ui.target);
    }
    await sleep(350);
    fail(`waiting for ${description} after permission prompt`);
  }, description, timeoutMs);
}

async function acceptPermissionPromptsUntilPagePasses(pageName, url, timeoutMs = 120000) {
  return acceptPermissionPromptsUntil(
    (state) => currentUrlMatches(state, url) && bodyIncludesPage(state, pageName),
    `${pageName} probe page to pass`,
    'nip07-permission-dialog.png',
    timeoutMs,
  );
}

async function runLiveSiteProbe({
  name,
  scenario,
  url,
  urlMatches,
  screenshotName,
}) {
  await postAutomationCommand({ action: 'open_url', url });
  await waitForAutomationState(urlMatches, `${name} page to open`, 60000);
  await waitForAutomationState(
    (state) => urlMatches(state) && state.childPageLoadState === 'finished' && !state.childLastError,
    `${name} page to finish loading`,
    120000,
  );
  await postAutomationNip07Probe({ scenario });
  const loadedState = await acceptPermissionPromptsUntil(
    (state) => urlMatches(state) && liveProbePassed(state, scenario),
    `${name} NIP-07 probe to pass`,
    `nip07-${scenario}-permission-dialog.png`,
    120000,
  );
  await takeScreenshot(screenshotName);
  return loadedState;
}

function probeHtml(pageName) {
  return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Loading ${pageName}</title>
    <style>
      :root { color-scheme: dark; }
      html,
      body {
        margin: 0;
        min-height: 100%;
        background: #111216;
        color: #f3f4f7;
      }
      body {
        min-height: 100vh;
        box-sizing: border-box;
        padding: 32px;
        font: 16px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace;
      }
      main {
        white-space: pre-wrap;
        max-width: 60rem;
      }
    </style>
  </head>
  <body>
    <main id="status">Starting ${pageName} probe…</main>
    <script>
      const status = document.getElementById('status');
      const peerPubkey = ${JSON.stringify(secondPubkey)};
      const pageName = ${JSON.stringify(pageName)};

      async function run() {
        const lines = [\`PAGE:\${pageName}\`];
        try {
          if (!window.nostr) {
            throw new Error('window.nostr is not available');
          }

          const pubkey = await window.nostr.getPublicKey();
          lines.push(\`PUBKEY:\${pubkey}\`);

          const signed = await window.nostr.signEvent({
            created_at: 1711111111,
            kind: 1,
            tags: [['smoke', pageName]],
            content: \`probe \${pageName}\`,
          });
          if (!signed || signed.pubkey !== pubkey || typeof signed.id !== 'string' || typeof signed.sig !== 'string') {
            throw new Error('signEvent returned an invalid event');
          }
          lines.push('SIGN:ok');

          const nip04Plaintext = \`nip04 \${pageName} \${pubkey.slice(0, 8)}\`;
          const nip04Ciphertext = await window.nostr.nip04.encrypt(peerPubkey, nip04Plaintext);
          const nip04Decrypted = await window.nostr.nip04.decrypt(peerPubkey, nip04Ciphertext);
          if (nip04Decrypted !== nip04Plaintext) {
            throw new Error('NIP-04 round trip failed');
          }
          lines.push('NIP04:ok');

          const nip44Plaintext = \`nip44 \${pageName} \${pubkey.slice(0, 8)}\`;
          const nip44Ciphertext = await window.nostr.nip44.encrypt(peerPubkey, nip44Plaintext);
          const nip44Decrypted = await window.nostr.nip44.decrypt(peerPubkey, nip44Ciphertext);
          if (nip44Decrypted !== nip44Plaintext) {
            throw new Error('NIP-44 round trip failed');
          }
          lines.push('NIP44:ok');

          document.title = \`PASS \${pageName} \${pubkey.slice(0, 8)}\`;
          status.textContent = [\`PASS \${pageName}\`, ...lines].join('\\n');
        } catch (error) {
          const message = error instanceof Error ? (error.stack || error.message) : String(error);
          document.title = \`FAIL \${pageName}\`;
          status.textContent = [\`FAIL \${pageName}\`, ...lines, message].join('\\n');
          console.error(error);
        }
      }

      run();
    </script>
  </body>
</html>`;
}

async function startProbeServer() {
  probeServer = createServer((req, res) => {
    const url = new URL(req.url ?? '/', probeBase);
    const pageName = url.pathname === '/beta' ? 'beta' : 'alpha';
    const html = probeHtml(pageName);
    res.writeHead(200, {
      'content-type': 'text/html; charset=utf-8',
      'cache-control': 'no-store',
    });
    res.end(html);
  });

  await new Promise((resolve, reject) => {
    probeServer.once('error', reject);
    probeServer.listen(probePort, '127.0.0.1', resolve);
  });
}

async function stopProbeServer() {
  if (!probeServer) {
    return;
  }
  await new Promise((resolve, reject) => {
    probeServer.close((error) => {
      if (error) {
        reject(error);
      } else {
        resolve();
      }
    });
  }).catch(() => {});
  probeServer = null;
}

async function signInWithFirstAccount() {
  const accountButton = await waitFor(
    () => findElement('css selector', "[data-testid='account-button']"),
    'account button',
    30000,
  );
  await clickElement(accountButton);
  const addExistingButton = await waitFor(
    () => findElement('css selector', "[data-testid='toggle-add-account-button']"),
    'add existing account button',
    30000,
  );
  await clickElement(addExistingButton);
  const secretInput = await waitFor(
    () => findElement('css selector', "[data-testid='account-nsec-input']"),
    'account secret input',
    30000,
  );
  await clearElement(secretInput);
  await typeIntoElement(secretInput, firstSecretHex);
  const saveButton = await waitFor(
    () => findElement('css selector', "[data-testid='account-save-button']"),
    'account save button',
    30000,
  );
  await clickElement(saveButton);
  await waitFor(
    () => findElement('css selector', "[data-testid='account-button'][data-account-state='signed-in']"),
    'signed-in account button',
    30000,
  );
}

async function main() {
  if (process.platform !== 'linux') {
    fail('This native smoke harness is Linux-only. Use the Docker wrapper or a Linux VM.');
  }

  maybeReexecUnderXvfb();

  const nativeDriverPath = which('WebKitWebDriver');
  if (!nativeDriverPath) {
    fail('WebKitWebDriver is required on Linux. Install the distro package that provides it, such as webkit2gtk-driver.');
  }

  if (!which('tauri-driver')) {
    fail('tauri-driver is not installed. Run: cargo install tauri-driver --locked');
  }

  if (!which('xdotool')) {
    fail('xdotool is required for this smoke test. Use the Docker wrapper or install xdotool on Linux.');
  }

  if (!isLiveSmoke) {
    await startProbeServer();
  }
  run('pnpm', ['exec', 'tauri', 'build', '--debug', '--no-bundle']);
  startTauriDriver(nativeDriverPath);

  try {
    await waitForWebDriver();
    await createSession();

    await waitForAutomationState(
      (state) => state.shellReady === true && state.currentView === 'launcher',
      'Iris launcher to be ready',
      60000,
    );
    await takeScreenshot('nip07-launcher.png');

    await signInWithFirstAccount();
    await takeScreenshot('nip07-signed-in-launcher.png');

    if (isLiveSmoke) {
      const jumbleLoadedState = await runLiveSiteProbe({
        name: 'Jumble',
        scenario: 'jumble',
        url: jumbleUrl,
        urlMatches: currentUrlMatchesJumble,
        screenshotName: 'nip07-jumble-loaded.png',
      });
      const jumblePubkey = extractPubkey(jumbleLoadedState);

      const socialLoadedState = await runLiveSiteProbe({
        name: 'Iris Social',
        scenario: 'iris-social',
        url: socialUrl,
        urlMatches: currentUrlMatchesSocial,
        screenshotName: 'nip07-iris-social-loaded.png',
      });
      const socialPubkey = extractPubkey(socialLoadedState);

      console.log('Jumble URL:', jumbleLoadedState.currentUrl);
      console.log('Jumble title:', jumbleLoadedState.childDocumentTitle);
      console.log('Jumble pubkey:', jumblePubkey ?? 'unavailable');
      console.log('Iris Social URL:', socialLoadedState.currentUrl);
      console.log('Iris Social title:', socialLoadedState.childDocumentTitle);
      console.log('Iris Social pubkey:', socialPubkey ?? 'unavailable');
      console.log(`Native live NIP-07 smoke passed. Screenshots written to ${artifactsDir}`);
    } else {
      await postAutomationCommand({ action: 'open_url', url: alphaUrl });
      await waitForAutomationState(
        (state) => currentUrlMatches(state, alphaUrl),
        'alpha probe page to open',
        60000,
      );

      const alphaLoadedState = await acceptPermissionPromptsUntilPagePasses(
        'alpha',
        alphaUrl,
        120000,
      );
      const firstPubkey = extractPubkey(alphaLoadedState);
      if (!firstPubkey) {
        fail(`alpha probe did not expose a pubkey: ${JSON.stringify(alphaLoadedState)}`);
      }
      await takeScreenshot('nip07-alpha-loaded.png');

      const accountButton = await findElement('css selector', "[data-testid='account-button']");
      await clickElement(accountButton);
      await sleep(500);
      await takeScreenshot('nip07-account-native-menu.png');

      const mainWindow = await waitForWindowNamed('Iris', 'main Iris window');
      xdotoolClickWindow(mainWindow, 220, 220);
      await sleep(300);

      await postAutomationCommand({ action: 'open_url', url: betaUrl });
      await waitForAutomationState(
        (state) => currentUrlMatches(state, betaUrl) && bodyIncludesPage(state, 'beta'),
        'beta probe page to pass',
        120000,
      );
      await takeScreenshot('nip07-beta-loaded.png');

      const addressInput = await findElement('css selector', "[data-testid='address-bar'] input");
      await clickElement(addressInput);
      await clearElement(addressInput);
      await typeIntoElement(addressInput, probeHostQuery);
      await sleep(500);
      xdotoolKey(mainWindow, 'Down');
      await sleep(500);
      await takeScreenshot('nip07-history-native-menu.png');
      xdotoolClickWindow(mainWindow, 220, 220);
      await sleep(300);

      console.log('Alpha bounds:', JSON.stringify({
        windowInnerHeight: alphaLoadedState.windowInnerHeight,
        windowOuterHeight: alphaLoadedState.windowOuterHeight,
        toolbarHeight: alphaLoadedState.toolbarHeight,
        childBoundsTop: alphaLoadedState.childBoundsTop,
        childBoundsHeight: alphaLoadedState.childBoundsHeight,
        childViewportWidth: alphaLoadedState.childViewportWidth,
        childViewportHeight: alphaLoadedState.childViewportHeight,
      }));
      console.log('First pubkey:', firstPubkey);
      console.log('Beta URL:', betaUrl);
      console.log(`Native NIP-07 smoke passed. Screenshots written to ${artifactsDir}`);
    }
  } catch (error) {
    const failedState = await getAutomationState().catch(() => null);
    await takeScreenshot('nip07-smoke-failed.png').catch(() => {});
    if (failedState) {
      console.error('NIP-07 smoke failed state:', JSON.stringify(failedState));
    }
    throw error;
  } finally {
    await deleteSession().catch(() => {});
    if (driverProcess) {
      driverProcess.kill('SIGTERM');
      driverProcess = null;
    }
    await stopProbeServer();
  }
}

main().catch(async (error) => {
  console.error(error instanceof Error ? error.message : error);
  await deleteSession().catch(() => {});
  if (driverProcess) {
    driverProcess.kill('SIGTERM');
  }
  await stopProbeServer();
  process.exit(1);
});
