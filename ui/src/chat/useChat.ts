/**
 * React state for the agent chat: settings, threads (persisted through
 * chat/store), the MCP connection, one in-flight turn at a time, and the
 * write-tool confirmation gate.
 *
 * @module chat/useChat
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { isWriteTool, runTurn, type Provider, type ToolRunner } from './loop';
import { createMcpClient, resultText, toToolDefs, type McpClient } from './mcpClient';
import { streamChat, type ToolDef } from './openai';
import { redactSecrets } from './redact';
import {
  appendMessage,
  loadSettings,
  loadThreads,
  newThread as makeThread,
  saveSettings as persistSettings,
  saveThreads,
  upsertThread,
  type ChatSettings,
  type Thread,
  type ThreadSeed,
  type ToolCall,
} from './store';

export type ConnectionState =
  | { kind: 'no-key' }
  | { kind: 'toolless'; reason: 'no-token' | 'mcp-off' }
  | { kind: 'connecting' }
  | { kind: 'ready'; toolCount: number; scope: 'read' | 'write' }
  | { kind: 'error'; message: string };

export interface PendingConfirm {
  threadId: string;
  call: ToolCall;
}

export interface ChatController {
  settings: ChatSettings;
  saveSettings(next: ChatSettings): void;
  forgetCredentials(): void;
  threads: Thread[];
  activeId: string | null;
  setActive(id: string | null): void;
  newThread(): string;
  deleteThread(id: string): void;
  clearAll(): void;
  connection: ConnectionState;
  connect(): Promise<void>;
  send(threadId: string, text: string): Promise<void>;
  /** The new thread's id, or null when a turn is already running (nothing created). */
  seedThread(seed: ThreadSeed, text: string): Promise<string | null>;
  stop(): void;
  pendingConfirm: PendingConfirm | null;
  resolveConfirm(run: boolean): void;
  busyThreadId: string | null;
  storageBlocked: boolean;
}

/** `window.localStorage`, or null where the accessor itself throws (blocked storage). */
function safeStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function newId(): string {
  const c = globalThis.crypto as Crypto | undefined;
  if (c && typeof c.randomUUID === 'function') return c.randomUUID();
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

export function useChat(opts: {
  mcpUrl: string;
  mcpEnabled: boolean;
  /** Called after a write tool succeeds, so the editor can re-fetch what the agent changed. */
  onConfigChanged?: () => void;
}): ChatController {
  const [settings, setSettings] = useState<ChatSettings>(() => loadSettings(safeStorage()));
  const [threads, setThreads] = useState<Thread[]>(() => loadThreads(safeStorage()));
  const [activeId, setActiveId] = useState<string | null>(() => threads[0]?.id ?? null);
  const [connection, setConnection] = useState<ConnectionState>({ kind: 'connecting' });
  const [pendingConfirm, setPendingConfirm] = useState<PendingConfirm | null>(null);
  const [busyThreadId, setBusyThreadId] = useState<string | null>(null);
  const [storageBlocked, setStorageBlocked] = useState(false);
  const [connectNonce, setConnectNonce] = useState(0);

  const mcpRef = useRef<McpClient | null>(null);
  // A ref, so an inline callback from the caller does not churn runOn.
  const onConfigChangedRef = useRef(opts.onConfigChanged);
  useEffect(() => {
    onConfigChangedRef.current = opts.onConfigChanged;
  }, [opts.onConfigChanged]);
  const toolsRef = useRef<ToolDef[]>([]);
  const abortRef = useRef<AbortController | null>(null);
  const confirmRef = useRef<((ok: boolean) => void) | null>(null);
  const threadsRef = useRef(threads);
  threadsRef.current = threads;
  /** Mirrors `busyThreadId` synchronously so `runOn`'s guard is not a stale closure. */
  const busyRef = useRef<string | null>(null);
  /** Thread ids dismissed (deleted / cleared) while a turn may still be streaming into them. */
  const dismissedRef = useRef(new Set<string>());

  /**
   * Writes the whole thread store to local storage. Debounced below: a
   * streaming turn changes `threads` on every token, and serialising every
   * thread per delta is the panel's one hot spot.
   */
  const flushThreads = useCallback(() => {
    const current = threadsRef.current;
    const out = saveThreads(current, safeStorage());
    if (!out.ok) setStorageBlocked(true);
    else if (out.dropped > 0 && out.threads.length !== current.length) setThreads(out.threads);
  }, []);
  const flushRef = useRef(flushThreads);
  flushRef.current = flushThreads;
  const saveTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    // Only a streaming turn mutates `threads` fast enough to matter (one
    // change per token). Every other mutation — send, delete, clear all — is a
    // single user action, so it persists at once and a reload immediately
    // afterwards never loses it.
    if (busyRef.current === null) {
      if (saveTimerRef.current !== null) {
        clearTimeout(saveTimerRef.current);
        saveTimerRef.current = null;
      }
      flushRef.current();
      return;
    }
    if (saveTimerRef.current !== null) clearTimeout(saveTimerRef.current);
    saveTimerRef.current = setTimeout(() => {
      saveTimerRef.current = null;
      flushRef.current();
    }, 500);
  }, [threads]);

  // A finished turn is the point the user may close the tab or reload, so
  // persist it immediately instead of waiting out the trailing timer.
  useEffect(() => {
    if (busyThreadId !== null) return;
    if (saveTimerRef.current !== null) {
      clearTimeout(saveTimerRef.current);
      saveTimerRef.current = null;
    }
    flushRef.current();
  }, [busyThreadId]);

  useEffect(
    () => () => {
      if (saveTimerRef.current !== null) {
        clearTimeout(saveTimerRef.current);
        saveTimerRef.current = null;
      }
      flushRef.current();
    },
    [],
  );

  const saveSettings = useCallback((next: ChatSettings) => {
    setSettings(next);
    persistSettings(next, safeStorage());
    mcpRef.current = null;
    toolsRef.current = [];
    // `connect`'s identity depends only on credential/URL fields, so a save that
    // only touches e.g. baseUrl/model/redact would not re-run the connect effect
    // on its own; force a reconnect unconditionally.
    setConnectNonce((n) => n + 1);
  }, []);

  const forgetCredentials = useCallback(() => {
    saveSettings({ ...settings, apiKey: '', mcpToken: '' });
  }, [saveSettings, settings]);

  const connect = useCallback(async () => {
    if (settings.apiKey === '') {
      setConnection({ kind: 'no-key' });
      return;
    }
    if (!opts.mcpEnabled) {
      toolsRef.current = [];
      setConnection({ kind: 'toolless', reason: 'mcp-off' });
      return;
    }
    if (settings.mcpToken === '') {
      toolsRef.current = [];
      setConnection({ kind: 'toolless', reason: 'no-token' });
      return;
    }
    setConnection({ kind: 'connecting' });
    try {
      const client = createMcpClient({ url: opts.mcpUrl, token: settings.mcpToken });
      const tools = await client.listTools();
      mcpRef.current = client;
      toolsRef.current = toToolDefs(tools);
      const scope = tools.some((t) => isWriteTool(t.name)) ? 'write' : 'read';
      setConnection({ kind: 'ready', toolCount: tools.length, scope });
    } catch (e) {
      mcpRef.current = null;
      toolsRef.current = [];
      setConnection({ kind: 'error', message: e instanceof Error ? e.message : String(e) });
    }
  }, [opts.mcpEnabled, opts.mcpUrl, settings.apiKey, settings.mcpToken]);

  useEffect(() => {
    void connect();
    // connectNonce forces a reconnect even when `connect`'s own identity is
    // unchanged (a settings save that touched no credential/URL field).
  }, [connect, connectNonce]);

  // Client-side secret redaction (chat/redact.ts) over everything stored or
  // sent; the user's own key/token are removed as literals wherever they appear.
  const redact = useCallback(
    (text: string) => (settings.redact ? redactSecrets(text, [settings.apiKey, settings.mcpToken]).text : text),
    [settings.redact, settings.apiKey, settings.mcpToken],
  );

  const updateThread = useCallback((t: Thread) => {
    // A deleted/cleared thread may still have a turn streaming into it; drop
    // those updates instead of letting upsertThread resurrect the thread.
    if (dismissedRef.current.has(t.id)) return;
    setThreads((list) => upsertThread(list, t));
  }, []);

  const newThread = useCallback((): string => {
    const t = makeThread(newId(), Date.now());
    setThreads((list) => upsertThread(list, t));
    setActiveId(t.id);
    return t.id;
  }, []);

  const stop = useCallback(() => {
    abortRef.current?.abort();
    confirmRef.current?.(false);
  }, []);

  const deleteThread = useCallback(
    (id: string) => {
      dismissedRef.current.add(id);
      if (busyRef.current === id) stop();
      setThreads((list) => list.filter((t) => t.id !== id));
      setActiveId((cur) => (cur === id ? null : cur));
    },
    [stop],
  );

  const clearAll = useCallback(() => {
    for (const t of threadsRef.current) dismissedRef.current.add(t.id);
    if (busyRef.current) dismissedRef.current.add(busyRef.current);
    stop();
    setThreads([]);
    setActiveId(null);
  }, [stop]);

  const resolveConfirm = useCallback((run: boolean) => {
    confirmRef.current?.(run);
  }, []);

  const runOn = useCallback(
    async (thread: Thread): Promise<boolean> => {
      // A ref check is synchronous, unlike `busyThreadId` state: two calls to
      // send/seedThread in the same tick must not both pass this guard.
      if (abortRef.current) return false;
      const ctl = new AbortController();
      abortRef.current = ctl;
      busyRef.current = thread.id;
      setBusyThreadId(thread.id);
      const provider: Provider = {
        stream: (messages, tools, signal) => streamChat(settings, messages, tools, signal),
      };
      const mcp = mcpRef.current;
      const tools: ToolRunner | null =
        mcp && toolsRef.current.length > 0
          ? {
              tools: toolsRef.current,
              call: async (name, args, signal) => {
                const r = await mcp.callTool(name, args, signal);
                if (!r.isError && isWriteTool(name)) onConfigChangedRef.current?.();
                return { text: resultText(r), isError: !!r.isError };
              },
            }
          : null;
      try {
        await runTurn(thread, {
          provider,
          tools,
          signal: ctl.signal,
          redact,
          hooks: {
            onThread: updateThread,
            confirm: (call) =>
              new Promise<boolean>((resolve) => {
                // "Auto-run writes" (settings.autoApprove): approve on the
                // spot, no card, no pause.
                if (settings.autoApprove) {
                  resolve(true);
                  return;
                }
                confirmRef.current = (ok) => {
                  confirmRef.current = null;
                  setPendingConfirm(null);
                  resolve(ok);
                };
                setPendingConfirm({ threadId: thread.id, call });
              }),
          },
        });
      } finally {
        abortRef.current = null;
        busyRef.current = null;
        setBusyThreadId(null);
        setPendingConfirm(null);
      }
      return true;
    },
    [settings, updateThread, redact],
  );

  const send = useCallback(
    async (threadId: string, text: string) => {
      // A thread created in the same tick (ChatPanel: `activeId ?? newThread()`)
      // is not in `threadsRef` yet; build it here so the send is not lost.
      const base = threadsRef.current.find((t) => t.id === threadId) ?? makeThread(threadId, Date.now());
      if (text.trim() === '') return;
      const next = appendMessage(base, { role: 'user', content: redact(text) }, Date.now());
      updateThread(next);
      await runOn(next);
    },
    [runOn, updateThread, redact],
  );

  const seedThread = useCallback(
    async (seed: ThreadSeed, text: string): Promise<string | null> => {
      // Refused by the same synchronous lock `runOn` uses — check it *before*
      // creating anything, or "Ask agent" during a running turn leaves a
      // switched-to thread holding a question nobody will ever answer.
      if (abortRef.current) return null;
      const t = appendMessage(makeThread(newId(), Date.now(), seed), { role: 'user', content: redact(text) }, Date.now());
      updateThread(t);
      setActiveId(t.id);
      await runOn(t);
      return t.id;
    },
    [runOn, updateThread, redact],
  );

  return useMemo(
    () => ({
      settings,
      saveSettings,
      forgetCredentials,
      threads,
      activeId,
      setActive: setActiveId,
      newThread,
      deleteThread,
      clearAll,
      connection,
      connect,
      send,
      seedThread,
      stop,
      pendingConfirm,
      resolveConfirm,
      busyThreadId,
      storageBlocked,
    }),
    [
      settings, saveSettings, forgetCredentials, threads, activeId, newThread, deleteThread, clearAll,
      connection, connect, send, seedThread, stop, pendingConfirm, resolveConfirm, busyThreadId, storageBlocked,
    ],
  );
}
