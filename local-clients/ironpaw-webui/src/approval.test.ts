import { describe, expect, it } from 'vitest';

import { approvalPresentation } from './approval';

const usdc = 'nep141:17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1';
const swap = {
  token_in: 'nep141:wrap.near', token_out: usdc,
  amount_in: '5000000000000000000000000', min_amount_out: '4500000',
};
const question = (body: unknown, path = '/wallet/v1/intents/swap') =>
  `Approve this action?\nWallet: main\nPOST http://localhost:4400${path}\n${JSON.stringify(body)}`;

describe('approvalPresentation', () => {
  it('names the action and preserves the exact request in readable details', () => {
    const result = approvalPresentation(question(swap))!;
    expect(result.title).toBe('Swap 5 NEAR for USDC');
    expect(result.details).toBe(`Wallet: main\nPOST http://localhost:4400/wallet/v1/intents/swap\n${JSON.stringify(swap, null, 2)}`);
  });

  it('formats fractional and very large amounts without rounding', () => {
    expect(approvalPresentation(question({ ...swap, amount_in: '1' }))?.title)
      .toBe('Swap 0.000000000000000000000001 NEAR for USDC');
    expect(approvalPresentation(question({ ...swap, amount_in: '1234567890123456789012345678901' }))?.title)
      .toBe('Swap 1234567.890123456789012345678901 NEAR for USDC');
    expect(approvalPresentation(question({ ...swap, token_in: usdc, token_out: 'nep141:wrap.near', amount_in: '1250000' }))?.title)
      .toBe('Swap 1.25 USDC for NEAR');
  });

  it('never guesses asset symbols or units for unknown or invalid data', () => {
    for (const body of [
      { ...swap, token_in: 'nep141:fake-wrap.near' },
      { ...swap, token_out: 'nep141:USDC' },
      { ...swap, token_out: '__proto__' },
      { ...swap, amount_in: 5000000 },
      { ...swap, amount_in: '-1' },
      null,
    ]) expect(approvalPresentation(question(body))?.title).toBe('Swap tokens');
  });

  it('keeps malformed bodies inspectable and names other supported actions', () => {
    const malformed = question(swap).replace(JSON.stringify(swap), '{broken');
    expect(approvalPresentation(malformed)?.details).toContain('{broken');
    expect(approvalPresentation(question({}, '/wallet/v1/transfer'))?.title).toBe('Transfer funds');
    expect(approvalPresentation(question({}, '/unknown'))?.title).toBe('Approve this action?');
  });

  it('does not reinterpret ordinary choices or shell approvals', () => {
    expect(approvalPresentation('Which wallet?')).toBeUndefined();
    expect(approvalPresentation('Approve this command?\nTool: git\nstatus')).toBeUndefined();
  });
});
