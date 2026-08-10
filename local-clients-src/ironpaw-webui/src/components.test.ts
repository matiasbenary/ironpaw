import { describe, expect, it } from 'vitest';

import { attachmentUrl } from './attachments';

describe('attachmentUrl', () => {
  it('builds a renderable image data URL from backend bytes', () => {
    expect(attachmentUrl({
      filename: 'paw.png',
      mime_type: 'image/png',
      data_base64: 'aW1hZ2U=',
    })).toBe('data:image/png;base64,aW1hZ2U=');
  });

  it('does not invent a URL when bytes are absent', () => {
    expect(attachmentUrl({ filename: 'paw.png', mime_type: 'image/png' })).toBeNull();
  });
});
