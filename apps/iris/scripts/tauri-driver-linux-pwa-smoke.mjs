import { mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn, spawnSync } from 'node:child_process';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const appDir = path.resolve(__dirname, '..');
const launcherPath = path.join(appDir, 'scripts', 'launch-linux-debug-iris.sh');
const artifactsDir = process.env.IRIS_NATIVE_ARTIFACT_DIR ?? path.join(appDir, 'test-results', 'native-pwa-live');
const webdriverPort = Number(process.env.TAURI_DRIVER_PORT ?? 4444);
const automationPort = Number(process.env.IRIS_AUTOMATION_PORT ?? 21977);
const webdriverBase = `http://127.0.0.1:${webdriverPort}`;
const automationBase = `http://127.0.0.1:${automationPort}/automation`;
const elementRefKey = 'element-6066-11e4-a52e-4f735466cecf';
const pwaSiteUrl = process.env.IRIS_PWA_SMOKE_SITE_URL ?? 'https://jumble.social/';
const pwaExpectedTitle = process.env.IRIS_PWA_SMOKE_EXPECTED_TITLE ?? 'Jumble';

let driverProcess = null;
let sessionId = null;

function fail(message) {
  throw new Error(message);
}

function slugifyName(name) {
  return name.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '');
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
      await new Promise((resolve) => setTimeout(resolve, intervalMs));
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

async function clickElement(elementId) {
  await request('POST', `/session/${sessionId}/element/${elementId}/click`, {});
}

async function takeScreenshot(filename) {
  await mkdir(artifactsDir, { recursive: true });
  const importBinary = getFramebufferCaptureBinary();
  if (importBinary) {
    const captured = captureFramebufferToFile(importBinary, filename);
    if (captured) {
      return;
    }
  }
  const payload = await request('GET', `/session/${sessionId}/screenshot`);
  const encoded = payload.value;
  if (!encoded) {
    fail('WebDriver screenshot response did not contain image data');
  }
  await writeFile(path.join(artifactsDir, filename), Buffer.from(encoded, 'base64'));
}

function getFramebufferCaptureBinary() {
  if (process.env.IRIS_NATIVE_CAPTURE_FRAMEBUFFER === '0') {
    return null;
  }
  return which('import');
}

function captureFramebufferToFile(importBinary, filename) {
  const target = path.join(artifactsDir, filename);
  const result = spawnSync(importBinary, ['-window', 'root', target], {
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

function normalizeComparableUrl(value) {
  try {
    return new URL(value).href;
  } catch {
    return value;
  }
}

function matchesSiteUrl(state) {
  const normalizedTarget = normalizeComparableUrl(pwaSiteUrl);
  const currentUrl = typeof state.currentUrl === 'string' ? normalizeComparableUrl(state.currentUrl) : '';
  return state.currentView === 'webview' && currentUrl === normalizedTarget;
}

function liveSiteLoaded(state) {
  return matchesSiteUrl(state) &&
    state.childPageLoadState === 'finished' &&
    !state.childLastError &&
    typeof state.childDocumentTitle === 'string' &&
    state.childDocumentTitle.trim().length > 0 &&
    typeof state.childBodyText === 'string' &&
    state.childBodyText.trim().length > 20;
}

function immutableLaunchOpened(state) {
  const currentUrl = typeof state.currentUrl === 'string' ? state.currentUrl : '';
  return state.currentView === 'webview' && currentUrl.startsWith('htree://nhash1');
}

function immutableLaunchLoaded(state) {
  return immutableLaunchOpened(state) &&
    state.childPageLoadState === 'finished' &&
    !state.childLastError &&
    typeof state.childDocumentTitle === 'string' &&
    state.childDocumentTitle.trim().length > 0 &&
    typeof state.childBodyText === 'string' &&
    state.childBodyText.trim().length > 20;
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

  run('pnpm', ['exec', 'tauri', 'build', '--debug', '--no-bundle']);
  startTauriDriver(nativeDriverPath);

  const favoriteSelector = `[data-testid='favorite-${slugifyName(pwaExpectedTitle)}']`;

  try {
    await waitForWebDriver();
    await createSession();

    await waitForAutomationState(
      (state) => state.shellReady === true && state.currentView === 'launcher',
      'Iris launcher to be ready',
      60000,
    );
    await takeScreenshot('pwa-launcher.png');

    await postAutomationCommand({ action: 'open_url', url: pwaSiteUrl });

    const openedSiteState = await waitForAutomationState(
      matchesSiteUrl,
      'live PWA site to open',
      120000,
    );
    await takeScreenshot('pwa-site-opened.png');

    const loadedSiteState = await waitForAutomationState(
      liveSiteLoaded,
      'live PWA site to finish loading',
      180000,
    );

    const installButton = await waitFor(
      () => findElement('css selector', "[data-testid='install-pwa-button']"),
      'PWA install button to appear',
      120000,
    );
    await takeScreenshot('pwa-site-loaded.png');
    await clickElement(installButton);
    await takeScreenshot('pwa-install-clicked.png');

    await postAutomationCommand({ action: 'home' });
    await waitForAutomationState(
      (state) => state.currentView === 'launcher',
      'launcher after PWA install',
      60000,
    );

    const favoriteButton = await waitFor(
      () => findElement('css selector', favoriteSelector),
      'installed PWA favorite card to appear',
      120000,
    );
    await takeScreenshot('pwa-installed-launcher.png');
    await clickElement(favoriteButton);

    const openedImmutableState = await waitForAutomationState(
      immutableLaunchOpened,
      'installed immutable PWA launch to open',
      120000,
    );
    await takeScreenshot('pwa-installed-opened.png');

    const loadedImmutableState = await waitForAutomationState(
      immutableLaunchLoaded,
      'installed immutable PWA content to finish loading',
      180000,
    );
    await takeScreenshot('pwa-installed-loaded.png');

    console.log('Live site URL:', openedSiteState.currentUrl);
    console.log('Live site title:', loadedSiteState.childDocumentTitle);
    console.log('Installed launch URL:', openedImmutableState.currentUrl);
    console.log('Installed page load URL:', loadedImmutableState.childPageLoadUrl);
    console.log('Installed title:', loadedImmutableState.childDocumentTitle);
    console.log('Installed body preview:', String(loadedImmutableState.childBodyText ?? '').slice(0, 400));
    console.log(`Native live PWA smoke passed. Screenshots written to ${artifactsDir}`);
  } catch (error) {
    const failedState = await getAutomationState().catch(() => null);
    await takeScreenshot('pwa-live-failed.png').catch(() => {});
    if (failedState) {
      console.error('PWA smoke failed state:', JSON.stringify(failedState));
    }
    throw error;
  } finally {
    await deleteSession().catch(() => {});
    if (driverProcess) {
      driverProcess.kill('SIGTERM');
      driverProcess = null;
    }
  }
}

main().catch(async (error) => {
  console.error(error instanceof Error ? error.message : error);
  await deleteSession().catch(() => {});
  if (driverProcess) {
    driverProcess.kill('SIGTERM');
  }
  process.exit(1);
});
