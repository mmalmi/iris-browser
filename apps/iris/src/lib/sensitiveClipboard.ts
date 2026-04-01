const testGlobals = globalThis as typeof globalThis & {
  __IRIS_TEST_CLIPBOARD_CLEAR_DELAY_MS__?: number;
};

export const DEFAULT_SENSITIVE_CLIPBOARD_CLEAR_DELAY_MS = 30_000;

export function sensitiveClipboardClearDelayMs(): number {
  return Number.isFinite(testGlobals.__IRIS_TEST_CLIPBOARD_CLEAR_DELAY_MS__)
    ? Math.max(0, Number(testGlobals.__IRIS_TEST_CLIPBOARD_CLEAR_DELAY_MS__))
    : DEFAULT_SENSITIVE_CLIPBOARD_CLEAR_DELAY_MS;
}

export async function writeClipboardText(text: string) {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(text);
    return;
  }

  const textarea = document.createElement('textarea');
  textarea.value = text;
  textarea.setAttribute('readonly', 'true');
  textarea.style.position = 'fixed';
  textarea.style.left = '-9999px';
  textarea.style.opacity = '0';
  document.body.appendChild(textarea);
  textarea.select();
  const copied = document.execCommand('copy');
  document.body.removeChild(textarea);
  if (!copied) {
    throw new Error('Clipboard copy is unavailable');
  }
}

export async function readClipboardText(): Promise<string | null> {
  if (!navigator.clipboard?.readText) {
    return null;
  }
  try {
    return await navigator.clipboard.readText();
  } catch {
    return null;
  }
}

export async function clearClipboardIfUnchanged(secret: string) {
  const currentClipboard = await readClipboardText();
  if (currentClipboard === secret) {
    try {
      await writeClipboardText('');
    } catch {
      // Best effort only.
    }
  }
}
