/**
 * Copying text without assuming a secure context.
 *
 * The async Clipboard API (`navigator.clipboard`) is only exposed in **secure
 * contexts** — HTTPS, or `localhost`. An admin UI served over plain HTTP on an
 * IP has no `clipboard` object at all, so a bare
 * `navigator.clipboard.writeText(...)` throws
 * `Cannot read properties of undefined (reading 'writeText')` rather than
 * failing in any way the caller can act on.
 *
 * The legacy `document.execCommand('copy')` path is not gated that way and
 * still works there, so it is tried before giving up. It is deprecated, but it
 * is the only thing standing between a plain-HTTP admin origin and a copy
 * button that cannot copy.
 */

export interface ClipboardEnv {
  /** `navigator.clipboard.writeText`, when the browser exposes it. */
  writeTextAsync?: (text: string) => Promise<void>;
  /** Legacy selection-based copy. Returns whether it succeeded. */
  execCopy?: (text: string) => boolean;
}

/**
 * Copies via a throwaway `<textarea>`. The element has to be in the document
 * and selectable, so it is moved off-screen rather than hidden — `display:
 * none` or `hidden` would make the selection, and therefore the copy, fail.
 */
function execCopyViaTextarea(text: string): boolean {
  if (typeof document === 'undefined') return false;
  const area = document.createElement('textarea');
  area.value = text;
  area.setAttribute('readonly', '');
  area.style.position = 'fixed';
  area.style.top = '-9999px';
  area.style.opacity = '0';
  document.body.appendChild(area);
  try {
    area.select();
    area.setSelectionRange(0, text.length);
    return document.execCommand('copy');
  } catch {
    return false;
  } finally {
    document.body.removeChild(area);
  }
}

function browserEnv(): ClipboardEnv {
  const clipboard = globalThis.navigator?.clipboard;
  return {
    writeTextAsync: clipboard ? (text: string) => clipboard.writeText(text) : undefined,
    execCopy: execCopyViaTextarea,
  };
}

/**
 * Copies `text`, preferring the async Clipboard API and falling back to the
 * legacy path. Rejects with an actionable message when neither is available,
 * so the toast tells the operator what to do instead of surfacing a
 * `TypeError` about `undefined`.
 */
export async function writeToClipboard(
  text: string,
  env: ClipboardEnv = browserEnv(),
): Promise<void> {
  if (env.writeTextAsync) {
    try {
      await env.writeTextAsync(text);
      return;
    } catch {
      // Permission denied, or the document is not focused. The legacy path
      // often still succeeds, so fall through rather than reporting this.
    }
  }
  if (env.execCopy?.(text)) return;
  throw new Error(
    'Clipboard unavailable — the browser only allows programmatic copy from a secure context. ' +
      'Serve the admin UI over HTTPS or from localhost, or select the text and copy it manually.',
  );
}
