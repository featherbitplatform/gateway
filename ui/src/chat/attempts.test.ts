import { describe, expect, it } from 'vitest';
import { buildRenderItems } from './attempts';
import type { ChatMessage } from './store';

const call = (id: string, name: string, args = '{}') => ({ id, name, arguments: args });
const tool = (id: string, name: string, status: 'done' | 'declined' | 'error', content = ''): ChatMessage => ({
  role: 'tool',
  toolCallId: id,
  name,
  status,
  content,
});

describe('buildRenderItems', () => {
  it('keeps user and assistant text in order and hides tool messages', () => {
    const items = buildRenderItems([
      { role: 'user', content: 'hi' },
      { role: 'assistant', content: 'hello' },
      { role: 'assistant', content: '', error: 'boom' },
    ]);
    expect(items).toEqual([
      { kind: 'user', content: 'hi' },
      { kind: 'assistant', content: 'hello' },
      { kind: 'assistant', content: '', error: 'boom' },
    ]);
  });

  it('folds retries of a failed tool into one group, in call order', () => {
    const items = buildRenderItems([
      { role: 'user', content: 'create it' },
      { role: 'assistant', content: '', toolCalls: [call('c1', 'put_policy', '{"a":1}')] },
      tool('c1', 'put_policy', 'error', 'bad'),
      { role: 'assistant', content: 'Let me fix that.', toolCalls: [call('c2', 'put_policy', '{"a":2}')] },
      tool('c2', 'put_policy', 'error', 'still bad'),
      { role: 'assistant', content: '', toolCalls: [call('c3', 'put_policy', '{"a":3}')] },
      tool('c3', 'put_policy', 'done', 'ok'),
      { role: 'assistant', content: 'Done.' },
    ]);
    const groups = items.filter((i) => i.kind === 'tools');
    expect(groups).toHaveLength(1);
    const g = groups[0].kind === 'tools' ? groups[0].group : null;
    expect(g?.name).toBe('put_policy');
    expect(g?.attempts.map((a) => a.call.id)).toEqual(['c1', 'c2', 'c3']);
    expect(g?.attempts.map((a) => a.result?.status)).toEqual(['error', 'error', 'done']);
    // The narration between attempts is kept, after the (single) card.
    expect(items.map((i) => i.kind)).toEqual(['user', 'tools', 'assistant', 'assistant']);
  });

  it('does not fold after a success, a decline, or a different tool', () => {
    const items = buildRenderItems([
      { role: 'assistant', content: '', toolCalls: [call('c1', 'get_policy')] },
      tool('c1', 'get_policy', 'done'),
      { role: 'assistant', content: '', toolCalls: [call('c2', 'get_policy')] },
      tool('c2', 'get_policy', 'done'),
      { role: 'assistant', content: '', toolCalls: [call('c3', 'put_policy')] },
      tool('c3', 'put_policy', 'declined', 'Declined by the user.'),
      { role: 'assistant', content: '', toolCalls: [call('c4', 'put_policy')] },
    ]);
    expect(items.filter((i) => i.kind === 'tools')).toHaveLength(4);
  });

  it('a pending retry (no result yet) joins the failed group', () => {
    const items = buildRenderItems([
      { role: 'assistant', content: '', toolCalls: [call('c1', 'run_sandbox')] },
      tool('c1', 'run_sandbox', 'error', 'invalid_input'),
      { role: 'assistant', content: '', toolCalls: [call('c2', 'run_sandbox')] },
    ]);
    const g = items[0].kind === 'tools' ? items[0].group : null;
    expect(items).toHaveLength(1);
    expect(g?.attempts).toHaveLength(2);
    expect(g?.attempts[1].result).toBeUndefined();
  });

  it('a user message closes open groups', () => {
    const items = buildRenderItems([
      { role: 'assistant', content: '', toolCalls: [call('c1', 'put_route')] },
      tool('c1', 'put_route', 'error', 'x'),
      { role: 'user', content: 'try again' },
      { role: 'assistant', content: '', toolCalls: [call('c2', 'put_route')] },
    ]);
    expect(items.filter((i) => i.kind === 'tools')).toHaveLength(2);
  });

  it('several calls in one assistant message stay separate groups', () => {
    const items = buildRenderItems([
      { role: 'assistant', content: '', toolCalls: [call('c1', 'get_route'), call('c2', 'get_policy')] },
    ]);
    expect(items.map((i) => (i.kind === 'tools' ? i.group.name : i.kind))).toEqual(['get_route', 'get_policy']);
  });
});
