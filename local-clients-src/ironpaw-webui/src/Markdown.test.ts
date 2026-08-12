import { describe, expect, it } from 'vitest';

import { stripMessageIdPrefix } from './Markdown';

describe('stripMessageIdPrefix', () => {
  it('removes leaked transport metadata', () => {
    expect(stripMessageIdPrefix(
      '[message_id:f17a7800-6a97-4827-a0c5-0e52221a5d9d] Is this Outlook?',
    )).toBe('Is this Outlook?');
  });

  it('preserves normal bracketed text', () => {
    expect(stripMessageIdPrefix('[message_id:example.com] text'))
      .toBe('[message_id:example.com] text');
  });
});
