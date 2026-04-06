import { type Page, expect } from '@playwright/test';

export interface ConsoleCollector {
  messages: string[];
}

export function attachConsoleCollector(page: Page): ConsoleCollector {
  const collector: ConsoleCollector = { messages: [] };

  page.on('console', (msg) => {
    const text = msg.text();
    // Ignore known benign messages
    if (text.includes('service worker')) return;
    if (text.includes('[HMR]')) return;
    // WebSocket reconnect attempts are expected in test environment
    if (text.includes('WebSocket')) return;
    // Chrome warning about integrity attribute on WASM preloads (crbug.com/981419)
    if (text.includes('integrity')) return;

    if (msg.type() === 'error' || msg.type() === 'warning') {
      collector.messages.push(`[${msg.type()}] ${text}`);
    }
  });

  return collector;
}

export function assertCleanConsole(collector: ConsoleCollector) {
  expect(
    collector.messages,
    `Unexpected console errors/warnings:\n${collector.messages.join('\n')}`
  ).toHaveLength(0);
}
