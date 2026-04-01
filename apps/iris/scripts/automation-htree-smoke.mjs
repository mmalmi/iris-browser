import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn } from 'node:child_process';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const appDir = path.resolve(__dirname, '..');
const binaryPath = process.env.IRIS_BINARY ?? path.join(appDir, 'src-tauri', 'target', 'debug', 'iris');
const launchMode = process.env.IRIS_SMOKE_LAUNCH ?? (process.platform === 'darwin' ? 'dev' : 'binary');
const automationPort = Number(process.env.IRIS_AUTOMATION_PORT ?? 21977);
const automationBase = `http://127.0.0.1:${automationPort}/automation`;
const smokeUrl = process.env.IRIS_SMOKE_URL;
const expectedTitle = process.env.IRIS_SMOKE_TITLE ?? '';
const bodyPatterns = (process.env.IRIS_SMOKE_BODY_INCLUDES ?? '')
  .split('||')
  .map((pattern) => pattern.trim())
  .filter(Boolean);
const urlMatches = (process.env.IRIS_SMOKE_URL_MATCH ?? smokeUrl ?? '')
  .split('||')
  .map((pattern) => pattern.trim())
  .filter(Boolean);

let appProcess = null;
let appExitInfo = null;
let appStartError = null;

function fail(message) {
  throw new Error(message);
}

function assertAppAlive() {
  if (appStartError) {
    throw appStartError;
  }
  if (appExitInfo) {
    fail(`Iris process exited before smoke completed (${appExitInfo})`);
  }
}

async function waitFor(fn, description, timeoutMs = 60000, intervalMs = 250) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;

  while (Date.now() < deadline) {
    try {
      assertAppAlive();
      return await fn();
    } catch (error) {
      lastError = error;
      assertAppAlive();
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

function startApp() {
  if (launchMode === 'none') {
    return;
  }

  const launch =
    launchMode === 'dev'
      ? {
          command: 'pnpm',
          args: ['exec', 'tauri', 'dev', '--no-watch'],
        }
      : {
          command: binaryPath,
          args: [],
        };

  appProcess = spawn(launch.command, launch.args, {
    cwd: appDir,
    env: {
      ...process.env,
      IRIS_AUTOMATION: '1',
      IRIS_AUTOMATION_PORT: String(automationPort),
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });

  appProcess.on('error', (error) => {
    appStartError = error;
  });
  appProcess.on('exit', (code, signal) => {
    appExitInfo = `code=${code ?? 'null'} signal=${signal ?? 'null'}`;
  });

  appProcess.stdout.on('data', (chunk) => {
    process.stdout.write(`[iris] ${chunk}`);
  });
  appProcess.stderr.on('data', (chunk) => {
    process.stderr.write(`[iris] ${chunk}`);
  });
}

async function main() {
  if (!smokeUrl) {
    fail('IRIS_SMOKE_URL is required');
  }

  startApp();

  try {
    await waitFor(async () => {
      const state = await getAutomationState();
      if (state.shellReady !== true || state.currentView !== 'launcher') {
        fail(`shell not ready yet: ${JSON.stringify(state)}`);
      }
      return state;
    }, 'Iris launcher to be ready');

    await postAutomationCommand({ action: 'open_url', url: smokeUrl });

    await waitFor(async () => {
      const state = await getAutomationState();
      const currentUrl = typeof state.currentUrl === 'string' ? state.currentUrl : '';
      const urlMatched = urlMatches.length === 0 || urlMatches.some((pattern) => currentUrl.includes(pattern));
      if (state.currentView !== 'webview' || !urlMatched) {
        fail(`waiting for ${smokeUrl}, current state: ${JSON.stringify(state)}`);
      }
      return state;
    }, `${smokeUrl} to open`);

    const loadedState = await waitFor(async () => {
      const state = await getAutomationState();
      if (state.childPageLoadState !== 'finished') {
        fail(`page load not finished yet: ${JSON.stringify(state)}`);
      }
      if (expectedTitle && state.childDocumentTitle !== expectedTitle) {
        fail(`unexpected title for ${smokeUrl}: ${JSON.stringify(state)}`);
      }
      if (state.childLastError) {
        fail(`child webview reported an error: ${JSON.stringify(state)}`);
      }
      if (bodyPatterns.some((pattern) => !String(state.childBodyText ?? '').includes(pattern))) {
        fail(`body text for ${smokeUrl} missing expected markers: ${JSON.stringify(state)}`);
      }
      return state;
    }, `${smokeUrl} content to finish loading`, 120000);

    console.log('Smoke URL:', smokeUrl);
    console.log('Smoke title:', loadedState.childDocumentTitle);
    console.log('Smoke page load URL:', loadedState.childPageLoadUrl);
    console.log('Smoke body preview:', String(loadedState.childBodyText ?? '').slice(0, 400));
  } finally {
    if (appProcess) {
      appProcess.kill('SIGTERM');
      appProcess = null;
    }
  }
}

main().catch((error) => {
  console.error(error instanceof Error ? error.message : error);
  if (appProcess) {
    appProcess.kill('SIGTERM');
  }
  process.exit(1);
});
