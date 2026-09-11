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
  seedThread(seed: ThreadSeed, text: string): Promise<string>;
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

export function useChat(opts: { mcpUrl: string; mcpEnabled: boolean }): ChatController {
  const [settings, setSettings] = useState<ChatSettings>(() => loadSettings(safeStorage()));
  const [threads, setThreads] = useState<Thread[]>(() => loadThreads(safeStorage()));
  const [activeId, setActiveId] = useState<string | null>(() => threads[0]?.id ?? null);
  const [connection, setConnection] = useState<ConnectionState>({ kind: 'connecting' });
  const [pendingConfirm, setPendingConfirm] = useState<PendingConfirm | null>(null);
  const [busyThreadId, setBusyThreadId] = useState<string | null>(null);
  const [storageBlocked, setStorageBlocked] = useState(false);

  const mcpRef = useRef<McpClient | null>(null);
  const toolsRef = useRef<ToolDef[]>([]);
  const abortRef = useRef<AbortController | null>(null);
  const confirmRef = useRef<((ok: boolean) => void) | null>(null);
  const threadsRef = useRef(threads);
  threadsRef.current = threads;

  useEffect(() => {
    const out = saveThreads(threads, safeStorage());
    if (!out.ok) setStorageBlocked(true);
    else if (out.dropped > 0 && out.threads.length !== threads.length) setThreads(out.threads);
  }, [threads]);

  const saveSettings = useCallback((next: ChatSettings) => {
    setSettings(next);
    persistSettings(next, safeStorage());
    mcpRef.current = null;
    toolsRef.current = [];
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
  }, [connect]);

  // Client-side secret redaction (chat/redact.ts) over everything stored or
  // sent; the user's own key/token are removed as literals wherever they appear.
  const redact = useCallback(
    (text: string) => (settings.redact ? redactSecrets(text, [settings.apiKey, settings.mcpToken]).text : text),
    [settings.redact, settings.apiKey, settings.mcpToken],
  );

  const updateThread = useCallback((t: Thread) => {
    setThreads((list) => upsertThread(list, t));
  }, []);

  const newThread = useCallback((): string => {
    const t = makeThread(newId(), Date.now());
    setThreads((list) => upsertThread(list, t));
    setActiveId(t.id);
    return t.id;
  }, []);

  const deleteThread = useCallback((id: string) => {
    setThreads((list) => list.filter((t) => t.id !== id));
    setActiveId((cur) => (cur === id ? null : cur));
  }, []);

  const clearAll = useCallback(() => {
    setThreads([]);
    setActiveId(null);
  }, []);

  const stop = useCallback(() => {
    abortRef.current?.abort();
    confirmRef.current?.(false);
  }, []);

  const resolveConfirm = useCallback((run: boolean) => {
    confirmRef.current?.(run);
  }, []);

  const runOn = useCallback(
    async (thread: Thread) => {
      if (busyThreadId) return;
      const ctl = new AbortController();
      abortRef.current = ctl;
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
        setBusyThreadId(null);
        setPendingConfirm(null);
      }
    },
    [busyThreadId, settings, updateThread, redact],
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
    async (seed: ThreadSeed, text: string): Promise<string> => {
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
