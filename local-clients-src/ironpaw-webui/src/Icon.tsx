type IconName = 'menu' | 'plus' | 'send' | 'attach' | 'file' | 'edit';

export function Icon({ name }: { name: IconName }) {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true">
      {name === 'menu' && <path d="M4 7h16M4 12h16M4 17h16" />}
      {name === 'plus' && <path d="M12 5v14M5 12h14" />}
      {name === 'send' && <><path d="m4 4 17 8-17 8 3-8-3-8Z" /><path d="M7 12h14" /></>}
      {name === 'attach' && <path d="m20.5 11.5-8.9 8.9a6 6 0 0 1-8.5-8.5l9.6-9.6a4 4 0 0 1 5.7 5.7l-9.6 9.6a2 2 0 1 1-2.8-2.8l8.9-8.9" />}
      {name === 'file' && <><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8Z" /><path d="M14 2v6h6" /></>}
      {name === 'edit' && <><path d="M12 20h9" /><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L8 18l-4 1 1-4Z" /></>}
    </svg>
  );
}

export function Logo({ className = 'paw-logo' }: { className?: string }) {
  return <img className={className} src="/paw.svg" alt="" />;
}
