// Display-only metadata, keyed by exact asset ID; never infer decimals from a symbol.
const assets: Record<string, { symbol: string; decimals: number }> = {
  'wrap.near': { symbol: 'NEAR', decimals: 24 },
  'wrap.testnet': { symbol: 'NEAR', decimals: 24 },
  '17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1': { symbol: 'USDC', decimals: 6 },
};

function asset(id: unknown) {
  const key = typeof id === 'string' ? id.replace(/^nep141:/, '') : '';
  return Object.hasOwn(assets, key) ? assets[key] : undefined;
}

function amount(value: unknown, decimals: number): string | undefined {
  if (typeof value !== 'string' || !/^\d+$/.test(value)) return;
  // String arithmetic preserves every minimal unit, including values beyond Number's precision.
  const digits = value.replace(/^0+(?=\d)/, '').padStart(decimals + 1, '0');
  const fraction = digits.slice(-decimals).replace(/0+$/, '');
  return `${digits.slice(0, -decimals)}${fraction ? `.${fraction}` : ''}`;
}

/** Adapt existing host questions, including ones already stored in chat history. */
export function approvalPresentation(question: string): { title: string; details: string } | undefined {
  const match = /^Approve this action\?\n(Wallet: [^\n]+)\n([A-Z]+) (https?:\/\/[^\n]+)\n([\s\S]*)$/.exec(question);
  if (!match) return;
  const [, wallet, method, url, rawBody] = match;
  let path: string;
  try { path = new URL(url!).pathname; } catch { return; }
  let title = 'Approve this action?';
  const names: Record<string, string> = {
    '/wallet/v1/intents/swap': 'Swap tokens',
    '/wallet/v1/intents/deposit': 'Deposit into NEAR Intents',
    '/wallet/v1/intents/withdraw': 'Withdraw from NEAR Intents',
    '/wallet/v1/intents/transfer': 'Transfer within NEAR Intents',
    '/wallet/v1/transfer': 'Transfer funds',
    '/wallet/v1/storage-deposit': 'Register token storage',
    '/wallet/v1/call': 'Call contract',
  };
  if (method === 'POST') title = names[path] || title;
  let bodyText = rawBody!;
  try {
    const body = JSON.parse(rawBody!);
    bodyText = JSON.stringify(body, null, 2);
    if (method === 'POST' && path === '/wallet/v1/intents/swap' && body) {
      const input = asset(body.token_in);
      const output = asset(body.token_out);
      const value = input && amount(body.amount_in, input.decimals);
      if (input && output && value) title = `Swap ${value} ${input.symbol} for ${output.symbol}`;
    }
  } catch { /* Keep malformed request details visible verbatim. */ }
  return { title, details: `${wallet}\n${method} ${url}\n${bodyText}` };
}
