import { beforeEach, vi, type MockInstance } from 'vitest';
import { shell } from './client';

/** Captures the codes the store logs in place of the removed bottom-centred notice, per test. */
export function captureNotices(): () => string[] {
  let spy: MockInstance<typeof shell.logNotice> | null = null;
  beforeEach(() => {
    spy = vi.spyOn(shell, 'logNotice');
    spy.mockClear();
    spy.mockResolvedValue(undefined);
  });
  return () => spy?.mock.calls.map(([code]) => code) ?? [];
}
