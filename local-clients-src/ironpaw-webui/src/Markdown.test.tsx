import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

import { Markdown } from './Markdown';

describe('Markdown', () => {
  it('renders formatting and links', () => {
    const html = renderToStaticMarkup(<Markdown>**[Ironpaw](https://example.com) works**</Markdown>);

    expect(html).toContain('<strong><a href="https://example.com"');
    expect(html).toContain('Ironpaw</a> works</strong>');
  });
});
