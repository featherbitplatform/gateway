import { describe, expect, it } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { Markdown } from './Markdown';

const render = (text: string) => renderToStaticMarkup(<Markdown text={text} />);

describe('Markdown', () => {
  it('renders emphasis, inline code and paragraphs', () => {
    const html = render('Hello **world** and `code`.\n\nSecond paragraph.');
    expect(html).toContain('<strong>world</strong>');
    expect(html).toMatch(/<code[^>]*>code<\/code>/);
    expect(html.match(/<p[ >]/g)).toHaveLength(2);
  });

  it('renders fenced code blocks inside a scrollable pre', () => {
    const html = render('```yaml\nnodes:\n  - id: a\n```');
    expect(html).toMatch(/<pre[^>]*>/);
    expect(html).toContain('nodes:');
    expect(html).toContain('overflow');
  });

  it('renders GFM tables and lists', () => {
    const html = render('| a | b |\n|---|---|\n| 1 | 2 |\n\n- one\n- two');
    expect(html).toContain('<table');
    expect(html).toContain('<td');
    expect(html.match(/<li[ >]/g)).toHaveLength(2);
  });

  it('opens links in a new tab without a referrer', () => {
    const html = render('[docs](https://example.com/x)');
    expect(html).toContain('href="https://example.com/x"');
    expect(html).toContain('target="_blank"');
    expect(html).toContain('rel="noopener noreferrer"');
  });

  it('never renders raw HTML from the model', () => {
    const html = render('before <script>alert(1)</script> <img src=x onerror=alert(1)> after');
    expect(html).not.toContain('<script');
    expect(html).not.toContain('<img');
    expect(html).toContain('&lt;script&gt;');
  });
});
