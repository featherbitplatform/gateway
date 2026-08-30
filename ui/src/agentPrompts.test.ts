import { describe, expect, it } from 'vitest';
import { clientSnippets, mcpEndpoint, promptQuery, READ_TOOLS, withMcpHint, WRITE_TOOLS } from './agentPrompts';

describe('mcpEndpoint', () => {
  it('joins origin and path without doubling slashes', () => {
    expect(mcpEndpoint('http://localhost:9090', '/mcp')).toBe('http://localhost:9090/mcp');
    expect(mcpEndpoint('http://localhost:9090/', '/agent')).toBe('http://localhost:9090/agent');
  });
});

describe('clientSnippets', () => {
  const s = clientSnippets('http://gw:9090/mcp');
  it('claude code snippet uses http transport and a token placeholder', () => {
    expect(s.claudeCode).toContain('claude mcp add --transport http featherbit http://gw:9090/mcp');
    expect(s.claudeCode).toContain('Authorization: Bearer <TOKEN>');
  });
  it('mcpServers json is valid JSON with the url', () => {
    const parsed = JSON.parse(s.mcpJson);
    expect(parsed.mcpServers.featherbit.url).toBe('http://gw:9090/mcp');
    expect(parsed.mcpServers.featherbit.headers.Authorization).toBe('Bearer <TOKEN>');
  });
  it('curl snippet sends an initialize request', () => {
    expect(s.curl).toContain('"method":"initialize"');
    expect(s.curl).toContain('text/event-stream');
  });
});

describe('scope tool lists', () => {
  it('are disjoint and non-empty', () => {
    expect(READ_TOOLS.length).toBeGreaterThan(10);
    expect(WRITE_TOOLS.length).toBeGreaterThan(5);
    for (const t of WRITE_TOOLS) expect(READ_TOOLS).not.toContain(t);
    for (const t of WRITE_TOOLS) expect(/^(put_|delete_|reload_config$)/.test(t)).toBe(true);
  });
});

describe('withMcpHint', () => {
  it('appends the hint once', () => {
    const out = withMcpHint('body');
    expect(out.startsWith('body\n\n')).toBe(true);
    expect(out).toContain('featherbit');
    expect(out).toContain('get_trace_step');
  });
});

describe('promptQuery', () => {
  it('encodes present args only', () => {
    expect(promptQuery({ trace_id: 'a b', node_id: undefined })).toBe('trace_id=a+b');
    expect(promptQuery({})).toBe('');
  });
});
