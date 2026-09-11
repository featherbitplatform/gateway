/**
 * One user turn of the agent chat: stream a reply, run the requested tools
 * (reads immediately, writes behind a confirmation), feed the results back,
 * repeat until the model answers with text, the user aborts, or the round
 * cap is hit. Pure with respect to React and the network — both come in as
 * dependencies.
 *
 * @module chat/loop
 */
import { WRITE_TOOLS } from '../agentPrompts';
import { ProviderError, toWire, type StreamEvent, type ToolDef, type WireMessage } from './openai';
import { appendMessage, replaceLastAssistant, truncateToolResult, type Thread, type ToolCall } from './store';
import { systemPrompt } from './systemPrompt';

export const MAX_ROUNDS = 16;

export function isWriteTool(name: string): boolean {
  return WRITE_TOOLS.includes(name);
}

export interface Provider {
  stream(messages: WireMessage[], tools: ToolDef[], signal: AbortSignal): AsyncIterable<StreamEvent>;
}

export interface ToolRunner {
  tools: ToolDef[];
  call(name: string, args: unknown, signal: AbortSignal): Promise<{ text: string; isError: boolean }>;
}

export interface TurnHooks {
  /** Called after every thread mutation, streaming deltas included. */
  onThread(thread: Thread): void;
  /** Write-tool gate: resolve true to run, false to decline. */
  confirm(call: ToolCall): Promise<boolean>;
}

export interface TurnDeps {
  provider: Provider;
  tools: ToolRunner | null;
  hooks: TurnHooks;
  signal: AbortSignal;
  now?: () => number;
  /** Applied to tool result/error text before it is stored or replayed (default: identity). */
  redact?: (text: string) => string;
}

function isAbort(e: unknown): boolean {
  return (e instanceof DOMException && e.name === 'AbortError') || (e instanceof Error && e.name === 'AbortError');
}

function describeError(e: unknown): string {
  if (e instanceof ProviderError) return `Provider returned ${e.status}: ${e.body}`;
  if (e instanceof Error) return e.message;
  return String(e);
}

export async function runTurn(thread: Thread, deps: TurnDeps): Promise<Thread> {
  const now = deps.now ?? (() => Date.now());
  const redact = deps.redact ?? ((s: string) => s);
  const toolDefs = deps.tools?.tools ?? [];
  const system = systemPrompt({ toolsAvailable: toolDefs.length > 0 });
  let t = thread;
  const emit = (next: Thread) => {
    t = next;
    deps.hooks.onThread(t);
  };

  for (let round = 0; ; round++) {
    let content = '';
    let calls: ToolCall[] = [];
    try {
      for await (const ev of deps.provider.stream(toWire(system, t.messages), toolDefs, deps.signal)) {
        if (ev.type === 'text') {
          content += ev.delta;
          emit(replaceLastAssistant(t, { role: 'assistant', content }, now()));
        } else if (ev.type === 'tool_calls') {
          calls = ev.calls;
        }
      }
    } catch (e) {
      if (isAbort(e)) {
        if (content !== '') emit(replaceLastAssistant(t, { role: 'assistant', content }, now()));
        return t;
      }
      emit(appendMessage(t, { role: 'assistant', content: '', error: describeError(e) }, now()));
      return t;
    }

    if (calls.length === 0) {
      // Streaming already emitted the final text; only emit when the thread
      // does not yet end with exactly this assistant message.
      const last = t.messages.at(-1);
      if (!(last?.role === 'assistant' && last.content === content && !last.toolCalls && !last.error)) {
        emit(replaceLastAssistant(t, { role: 'assistant', content }, now()));
      }
      return t;
    }

    emit(replaceLastAssistant(t, { role: 'assistant', content, toolCalls: calls }, now()));

    for (const call of calls) {
      if (deps.signal.aborted) return t;
      let args: unknown;
      try {
        args = call.arguments.trim() === '' ? {} : JSON.parse(call.arguments);
      } catch {
        emit(
          appendMessage(
            t,
            { role: 'tool', toolCallId: call.id, name: call.name, status: 'error', content: `Invalid JSON arguments: ${call.arguments}` },
            now(),
          ),
        );
        continue;
      }
      if (isWriteTool(call.name)) {
        const ok = await deps.hooks.confirm(call);
        if (!ok) {
          emit(appendMessage(t, { role: 'tool', toolCallId: call.id, name: call.name, status: 'declined', content: 'Declined by the user.' }, now()));
          continue;
        }
      }
      if (!deps.tools) {
        emit(appendMessage(t, { role: 'tool', toolCallId: call.id, name: call.name, status: 'error', content: 'No tools are connected.' }, now()));
        continue;
      }
      try {
        const r = await deps.tools.call(call.name, args, deps.signal);
        emit(
          appendMessage(
            t,
            {
              role: 'tool',
              toolCallId: call.id,
              name: call.name,
              status: r.isError ? 'error' : 'done',
              content: truncateToolResult(redact(r.text)),
            },
            now(),
          ),
        );
      } catch (e) {
        if (isAbort(e)) return t;
        emit(appendMessage(t, { role: 'tool', toolCallId: call.id, name: call.name, status: 'error', content: redact(describeError(e)) }, now()));
      }
    }

    if (round + 1 >= MAX_ROUNDS) {
      emit(
        appendMessage(
          t,
          { role: 'assistant', content: `Stopped after ${MAX_ROUNDS} tool rounds. Send a message to continue.` },
          now(),
        ),
      );
      return t;
    }
  }
}
