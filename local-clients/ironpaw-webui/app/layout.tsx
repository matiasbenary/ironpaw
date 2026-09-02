import type { Metadata } from 'next';
import type { ReactNode } from 'react';

import '../src/styles.css';

export const metadata: Metadata = {
  title: 'Ironpaw',
  description: 'A local client for Ironpaw',
};

export default function Layout({ children }: { children: ReactNode }) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}

