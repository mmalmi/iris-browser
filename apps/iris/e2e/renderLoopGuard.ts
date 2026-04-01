import type { Page } from '@playwright/test';

const RENDER_LOOP_PATTERNS = [
  'effect_update_depth_exceeded',
  'Maximum update depth exceeded',
];

export function isRenderLoopMessage(message: string): boolean {
  return RENDER_LOOP_PATTERNS.some((pattern) => message.includes(pattern));
}

export function formatRenderLoopFailures(failures: Set<string>): string {
  return `Detected Svelte render/update loop:\n${Array.from(failures).join('\n')}`;
}

export function attachRenderLoopGuardToPage(page: Page, failures: Set<string>) {
  const recordFailure = (source: 'console' | 'pageerror', message: string) => {
    if (!isRenderLoopMessage(message)) return;
    failures.add(`[${source}] ${page.url() || 'about:blank'} ${message}`);
  };

  page.on('pageerror', (error) => {
    recordFailure('pageerror', error.stack || error.message);
  });

  page.on('console', (message) => {
    if (message.type() !== 'error') return;
    recordFailure('console', message.text());
  });
}
