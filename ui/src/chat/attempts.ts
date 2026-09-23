/**
 * Turns a thread's messages into the items the message list renders, folding
 * retries of the same tool into one group: when a tool call fails and the
 * model calls the same tool again (usually with corrected arguments), the
 * later attempts join the first card instead of stacking a fresh card per
 * try. A user message closes every open group. Pure — no React.
 *
 * @module chat/attempts
 */
import type { ChatMessage, ToolCall } from './store';

export type ToolMessage = Extract<ChatMessage, { role: 'tool' }>;

export interface ToolAttempt {
  call: ToolCall;
  /** Missing while the call is still running or awaiting confirmation. */
  result?: ToolMessage;
}

export interface ToolGroup {
  name: string;
  /** Oldest first; the last entry is the current attempt. */
  attempts: ToolAttempt[];
}

export type RenderItem =
  | { kind: 'user'; content: string }
  | { kind: 'assistant'; content: string; error?: string }
  | { kind: 'tools'; group: ToolGroup };

/**
 * Builds the render list. A tool call joins the most recent group with the
 * same tool name when that group's latest attempt ended in `error` (a retry);
 * otherwise it starts a new group. Assistant text between attempts stays
 * where it was; a user message resets grouping.
 */
export function buildRenderItems(messages: ChatMessage[]): RenderItem[] {
  const results = new Map<string, ToolMessage>();
  for (const m of messages) if (m.role === 'tool') results.set(m.toolCallId, m);

  const items: RenderItem[] = [];
  const openGroups = new Map<string, ToolGroup>();

  for (const m of messages) {
    if (m.role === 'user') {
      openGroups.clear();
      items.push({ kind: 'user', content: m.content });
      continue;
    }
    if (m.role === 'tool') continue;
    // Whitespace-only text (models often stream a bare newline alongside
    // tool calls) would render as an empty bubble, so it counts as no text.
    if (m.content.trim() !== '' || m.error) {
      const item: RenderItem = { kind: 'assistant', content: m.content.trim() === '' ? '' : m.content };
      if (m.error) item.error = m.error;
      items.push(item);
    }
    for (const call of m.toolCalls ?? []) {
      const attempt: ToolAttempt = { call };
      const result = results.get(call.id);
      if (result) attempt.result = result;
      const open = openGroups.get(call.name);
      if (open && open.attempts.at(-1)?.result?.status === 'error') {
        open.attempts.push(attempt);
      } else {
        const group: ToolGroup = { name: call.name, attempts: [attempt] };
        openGroups.set(call.name, group);
        items.push({ kind: 'tools', group });
      }
    }
  }
  return items;
}
