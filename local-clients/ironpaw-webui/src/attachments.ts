import type { Attachment } from './types';

export const attachmentUrl = (attachment: Attachment): string | null => attachment.data_base64
  ? `data:${attachment.mime_type};base64,${attachment.data_base64}`
  : null;
