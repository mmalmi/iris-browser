import { mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn, spawnSync } from 'node:child_process';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const appDir = path.resolve(__dirname, '..');
const launcherPath = path.join(appDir, 'scripts', 'launch-linux-debug-iris.sh');
const artifactsDir = process.env.IRIS_NATIVE_ARTIFACT_DIR ?? path.join(appDir, 'test-results', 'native');
const webdriverPort = Number(process.env.TAURI_DRIVER_PORT ?? 4444);
const automationPort = Number(process.env.IRIS_AUTOMATION_PORT ?? 21977);
const launcherSmokeMode = process.env.IRIS_LAUNCHER_SMOKE_MODE ?? 'files';
const webdriverBase = `http://127.0.0.1:${webdriverPort}`;
const automationBase = `http://127.0.0.1:${automationPort}/automation`;
const elementRefKey = 'element-6066-11e4-a52e-4f735466cecf';
const distributedOwner = 'npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm';
const defaultFilesUrl = `htree://${distributedOwner}/files`;
const launcherIconsFavoriteSlug = process.env.IRIS_LAUNCHER_ICONS_FAVORITE_SLUG ?? 'iris-files';
const launcherIconsFavoriteSrc = process.env.IRIS_LAUNCHER_ICONS_FAVORITE_SRC ?? '/iris-files-icon.svg';

let driverProcess = null;
let sessionId = null;

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

async function waitForAutomationState(predicate, description, timeoutMs = 30000) {
  return waitFor(async () => {
    const response = await fetch(`${automationBase}/state`);
    if (!response.ok) {
      fail(`automation state returned ${response.status}`);
    }
    const state = await response.json();
    if (!predicate(state)) {
      fail(`waiting for ${description}, current state: ${JSON.stringify(state)}`);
    }
    return state;
  }, description, timeoutMs);
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

async function waitForElement(using, value, description, timeoutMs = 30000) {
  return waitFor(() => findElement(using, value), description, timeoutMs);
}

function webdriverElement(elementId) {
  return { [elementRefKey]: elementId };
}

async function performActions(actions) {
  await request('POST', `/session/${sessionId}/actions`, { actions });
}

async function releaseActions() {
  await request('DELETE', `/session/${sessionId}/actions`);
}

async function clickElementOffset(elementId, x, y) {
  try {
    await performActions([
      {
        type: 'pointer',
        id: 'mouse',
        parameters: { pointerType: 'mouse' },
        actions: [
          { type: 'pointerMove', duration: 0, origin: webdriverElement(elementId), x, y },
          { type: 'pointerDown', button: 0 },
          { type: 'pointerUp', button: 0 },
        ],
      },
    ]);
  } finally {
    await releaseActions().catch(() => {});
  }
}

async function dragElementOffset(elementId, x, y, deltaX, deltaY) {
  try {
    await performActions([
      {
        type: 'pointer',
        id: 'mouse',
        parameters: { pointerType: 'mouse' },
        actions: [
          { type: 'pointerMove', duration: 0, origin: webdriverElement(elementId), x, y },
          { type: 'pointerDown', button: 0 },
          { type: 'pause', duration: 120 },
          { type: 'pointerMove', duration: 240, origin: 'pointer', x: deltaX, y: deltaY },
          { type: 'pause', duration: 120 },
          { type: 'pointerUp', button: 0 },
        ],
      },
    ]);
  } finally {
    await releaseActions().catch(() => {});
  }
}

async function getWindowRect() {
  const payload = await request('GET', `/session/${sessionId}/window/rect`);
  const rect = payload.value ?? payload;
  if (
    typeof rect?.x !== 'number' ||
    typeof rect?.y !== 'number' ||
    typeof rect?.width !== 'number' ||
    typeof rect?.height !== 'number'
  ) {
    fail(`WebDriver did not return a valid window rect: ${JSON.stringify(payload)}`);
  }
  return rect;
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

async function main() {
  if (process.platform !== 'linux') {
    fail('This native smoke harness is Linux-only. Use Playwright web-shell tests or the automation bridge on macOS.');
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

  try {
    await waitForWebDriver();
    await createSession();

    await waitForAutomationState(
      (state) => state.shellReady === true && state.currentView === 'launcher',
      'Iris launcher to be ready',
    );
    await takeScreenshot(launcherSmokeMode === 'icons' ? 'launcher-icons.png' : 'launcher.png');

    if (launcherSmokeMode === 'icons') {
      const addFavoriteButton = await waitForElement(
        'css selector',
        `[data-testid='suggestion-add-${launcherIconsFavoriteSlug}']`,
        `${launcherIconsFavoriteSlug} launcher add-to-favourites button`,
      );
      await clickElement(addFavoriteButton);

      await waitForElement(
        'css selector',
        `[data-testid='favorite-icon-${launcherIconsFavoriteSlug}'] img[src='${launcherIconsFavoriteSrc}']`,
        `${launcherIconsFavoriteSlug} favourite icon to use the distinct launcher svg`,
      );
      await takeScreenshot('launcher-icons-favorite.png');
      console.log(`Launcher icon smoke passed. Screenshots written to ${artifactsDir}`);
      return;
    }

    const toolbar = await waitForElement('css selector', "[data-testid='toolbar']", 'toolbar');
    const windowBeforeFocusClick = await getWindowRect();
    await clickElementOffset(toolbar, 20, 20);
    const windowAfterFocusClick = await getWindowRect();
    if (
      Math.abs(windowAfterFocusClick.x - windowBeforeFocusClick.x) > 4 ||
      Math.abs(windowAfterFocusClick.y - windowBeforeFocusClick.y) > 4
    ) {
      fail(
        `Toolbar focus click should not move the window: before=${JSON.stringify(windowBeforeFocusClick)} after=${JSON.stringify(windowAfterFocusClick)}`,
      );
    }

    await dragElementOffset(toolbar, 20, 20, 140, 48);
    const windowAfterDrag = await waitFor(async () => {
      const rect = await getWindowRect();
      if (
        Math.abs(rect.x - windowAfterFocusClick.x) < 16 &&
        Math.abs(rect.y - windowAfterFocusClick.y) < 16
      ) {
        fail(
          `waiting for focused toolbar drag to move the window, current rect=${JSON.stringify(rect)} baseline=${JSON.stringify(windowAfterFocusClick)}`,
        );
      }
      return rect;
    }, 'focused toolbar drag to move the window');
    await takeScreenshot('launcher-dragged.png');
    console.log(
      `Focused toolbar drag moved the window from (${windowAfterFocusClick.x}, ${windowAfterFocusClick.y}) to (${windowAfterDrag.x}, ${windowAfterDrag.y})`,
    );

    const irisFilesCard = await waitForElement(
      'css selector',
      "[data-testid='suggestion-open-iris-files']",
      'Iris Files launcher suggestion',
    );
    await clickElement(irisFilesCard);

    await waitForAutomationState(
      (state) => state.currentView === 'webview' && (
        state.currentUrl === defaultFilesUrl ||
        state.currentUrl === `${defaultFilesUrl}/` ||
        state.currentUrl === `${defaultFilesUrl}/index.html` ||
        state.currentUrl.startsWith('htree://nhash1') ||
        state.currentUrl.includes('/files/') ||
        state.currentUrl.includes('/files/index.html')
      ),
      'Iris Files to open through htree',
    );
    await takeScreenshot('files.png');

    await waitForAutomationState(
      (state) => state.childPageLoadState === 'finished' &&
        state.childDocumentTitle === 'Iris Files' &&
        typeof state.childBodyText === 'string' &&
        state.childBodyText.trim().length > 0 &&
        !state.childLastError,
      'Iris Files shell to finish loading through htree',
      60000,
    );
    await takeScreenshot('files-loaded.png');

    const homeButton = await findElement('css selector', "button[title='Home']");
    await clickElement(homeButton);

    await waitForAutomationState(
      (state) => state.currentView === 'launcher',
      'launcher to return after clicking Home',
    );
    await takeScreenshot('launcher-returned.png');

    console.log(`Native smoke passed. Screenshots written to ${artifactsDir}`);
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
