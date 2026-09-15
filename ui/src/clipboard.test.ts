import { describe, expect, it, vi } from 'vitest';

import { writeToClipboard, type ClipboardEnv } from './clipboard';

describe('writeToClipboard', () => {
  it('uses the async Clipboard API when it is available', async () => {
    const writeTextAsync = vi.fn().mockResolvedValue(undefined);
    const execCopy = vi.fn().mockReturnValue(true);

    await writeToClipboard('hello', { writeTextAsync, execCopy });

    expect(writeTextAsync).toHaveBeenCalledWith('hello');
    expect(execCopy).not.toHaveBeenCalled();
  });

  /**
   * The reported bug. `navigator.clipboard` is gated on secure contexts, so an
   * admin UI served over plain HTTP on an IP has no `clipboard` object at all
   * and `navigator.clipboard.writeText` throws
   * "Cannot read properties of undefined (reading 'writeText')".
   */
  it('falls back to the legacy path when the Clipboard API is absent', async () => {
    const execCopy = vi.fn().mockReturnValue(true);

    await writeToClipboard('hello', { writeTextAsync: undefined, execCopy });

    expect(execCopy).toHaveBeenCalledWith('hello');
  });

  it('falls back to the legacy path when the Clipboard API rejects', async () => {
    const writeTextAsync = vi.fn().mockRejectedValue(new Error('denied'));
    const execCopy = vi.fn().mockReturnValue(true);

    await writeToClipboard('hello', { writeTextAsync, execCopy });

    expect(writeTextAsync).toHaveBeenCalled();
    expect(execCopy).toHaveBeenCalledWith('hello');
  });

  it('throws an actionable error naming the secure-context cause when both fail', async () => {
    const writeTextAsync = vi.fn().mockRejectedValue(new Error('denied'));
    const execCopy = vi.fn().mockReturnValue(false);

    await expect(writeToClipboard('hello', { writeTextAsync, execCopy })).rejects.toThrow(
      /secure context/i,
    );
  });

  it('reports the same actionable error when there is no clipboard support at all', async () => {
    const env: ClipboardEnv = { writeTextAsync: undefined, execCopy: undefined };

    await expect(writeToClipboard('hello', env)).rejects.toThrow(/secure context/i);
  });
});
