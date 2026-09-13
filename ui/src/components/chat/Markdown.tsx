/**
 * Renders assistant Markdown (GitHub flavour) with the app's theme tokens.
 *
 * `react-markdown` never emits raw HTML from the source (no `rehype-raw`),
 * so a model reply containing `<script>` or `<img onerror>` is shown as
 * escaped text — the reply is untrusted data, the same as a tool result.
 * Links open in a new tab without a referrer.
 */
import type { CSSProperties, ReactNode } from 'react';
import ReactMarkdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';

interface MarkdownProps {
  text: string;
}

const mono: CSSProperties = { fontFamily: 'var(--font-mono)', fontSize: 'var(--text-2xs)' };

const inlineCode: CSSProperties = {
  ...mono,
  padding: '1px 4px',
  borderRadius: 'var(--radius-sm)',
  background: 'var(--surface)',
  border: '1px solid var(--border)',
};

const block: CSSProperties = {
  margin: '6px 0',
  padding: 8,
  borderRadius: 'var(--radius-sm)',
  background: 'var(--surface)',
  border: '1px solid var(--border)',
  overflowX: 'auto',
};

const components: Components = {
  p: ({ children }) => <p style={{ margin: '4px 0' }}>{children}</p>,
  ul: ({ children }) => <ul style={{ margin: '4px 0', paddingLeft: 18 }}>{children}</ul>,
  ol: ({ children }) => <ol style={{ margin: '4px 0', paddingLeft: 18 }}>{children}</ol>,
  li: ({ children }) => <li style={{ margin: '2px 0' }}>{children}</li>,
  h1: ({ children }) => <h3 style={{ margin: '8px 0 4px', fontSize: 'var(--text-sm)' }}>{children}</h3>,
  h2: ({ children }) => <h3 style={{ margin: '8px 0 4px', fontSize: 'var(--text-sm)' }}>{children}</h3>,
  h3: ({ children }) => <h4 style={{ margin: '6px 0 4px', fontSize: 'var(--text-xs)' }}>{children}</h4>,
  h4: ({ children }) => <h4 style={{ margin: '6px 0 4px', fontSize: 'var(--text-xs)' }}>{children}</h4>,
  a: ({ href, children }) => (
    <a href={href} target="_blank" rel="noopener noreferrer" style={{ color: 'var(--accent)' }}>
      {children}
    </a>
  ),
  // Fenced blocks arrive as <pre><code>; inline code has no <pre> parent.
  pre: ({ children }) => <pre style={{ ...block, ...mono, whiteSpace: 'pre' }}>{children}</pre>,
  code: ({ children, className }) => <Code className={className}>{children}</Code>,
  table: ({ children }) => (
    <div style={{ overflowX: 'auto', margin: '6px 0' }}>
      <table style={{ borderCollapse: 'collapse', fontSize: 'var(--text-2xs)' }}>{children}</table>
    </div>
  ),
  th: ({ children }) => (
    <th style={{ textAlign: 'left', padding: '3px 8px', borderBottom: '1px solid var(--border)', color: 'var(--text-secondary)' }}>{children}</th>
  ),
  td: ({ children }) => <td style={{ padding: '3px 8px', borderBottom: '1px solid var(--border)' }}>{children}</td>,
  blockquote: ({ children }) => (
    <blockquote style={{ margin: '4px 0', paddingLeft: 8, borderLeft: '2px solid var(--border)', color: 'var(--text-secondary)' }}>
      {children}
    </blockquote>
  ),
  hr: () => <hr style={{ border: 0, borderTop: '1px solid var(--border)', margin: '8px 0' }} />,
};

/**
 * `react-markdown` passes no "inline" flag. Fenced blocks carry a
 * `language-*` class when a language is given and always contain a newline;
 * they live inside the styled <pre> already, so only inline code gets the chip.
 */
function Code({ className, children }: { className?: string; children?: ReactNode }) {
  const text = typeof children === 'string' ? children : Array.isArray(children) ? children.join('') : '';
  const fenced = (className ?? '').startsWith('language-') || text.includes('\n');
  return fenced ? <code className={className}>{children}</code> : <code style={inlineCode}>{children}</code>;
}

export function Markdown({ text }: MarkdownProps) {
  return (
    <div style={{ wordBreak: 'break-word' }}>
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components}>
        {text}
      </ReactMarkdown>
    </div>
  );
}
