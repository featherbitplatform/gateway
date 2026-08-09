/**
 * Searchable, executable catalog of editor actions (Ctrl+K).
 *
 * Renders the registry from commands.ts: typing filters by title, arrows
 * move the selection, Enter runs the action and closes. Each row shows the
 * action's shortcut, so the palette is also the shortcut reference.
 *
 * @module components/CommandPalette
 */
import { useEffect, useMemo, useState } from 'react';
import { buildCommands, type CommandContext } from '../commands';

/** Props for {@link CommandPalette}. */
interface CommandPaletteProps {
  open: boolean;
  onClose: () => void;
  ctx: CommandContext;
}

/** Modal palette; renders nothing when closed. */
export function CommandPalette({ open, onClose, ctx }: CommandPaletteProps) {
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);

  // Only available actions are listed; `when` false also makes keys inert.
  const matches = useMemo(() => {
    const q = query.trim().toLowerCase();
    return buildCommands()
      .filter((c) => (c.when ? c.when(ctx) : true))
      .filter((c) => c.title.toLowerCase().includes(q));
  }, [query, ctx]);

  // Reset per opening, and keep the cursor inside the filtered list.
  useEffect(() => {
    if (open) {
      setQuery('');
      setActive(0);
    }
  }, [open]);
  useEffect(() => setActive(0), [query]);

  if (!open) return null;

  const run = (index: number) => {
    const cmd = matches[index];
    if (!cmd) return;
    onClose();
    cmd.run(ctx);
  };

  return (
    <div
      onClick={onClose}
      style={{
        position: 'fixed',
        inset: 0,
        background: 'rgba(0,0,0,0.45)',
        display: 'flex',
        justifyContent: 'center',
        alignItems: 'flex-start',
        paddingTop: '12vh',
        zIndex: 100,
      }}
    >
      <div
        role="dialog"
        aria-label="Command palette"
        onClick={(e) => e.stopPropagation()}
        style={{
          width: 460,
          maxWidth: '90vw',
          background: 'var(--surface-raised)',
          border: '1px solid var(--border)',
          borderRadius: 'var(--radius-md)',
          boxShadow: 'var(--shadow-md)',
          overflow: 'hidden',
        }}
      >
        <input
          autoFocus
          value={query}
          placeholder="Type a command…"
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Escape') {
              e.preventDefault();
              onClose();
            } else if (e.key === 'ArrowDown') {
              e.preventDefault();
              setActive((i) => Math.min(i + 1, matches.length - 1));
            } else if (e.key === 'ArrowUp') {
              e.preventDefault();
              setActive((i) => Math.max(i - 1, 0));
            } else if (e.key === 'Enter') {
              e.preventDefault();
              run(active);
            }
          }}
          style={{
            width: '100%',
            padding: '10px 12px',
            border: 'none',
            borderBottom: '1px solid var(--border)',
            background: 'transparent',
            color: 'var(--text-primary)',
            fontFamily: 'var(--font-sans)',
            fontSize: 'var(--text-sm)',
            outline: 'none',
          }}
        />
        <div style={{ maxHeight: 320, overflowY: 'auto' }}>
          {matches.length === 0 && (
            <div style={{ padding: '10px 12px', color: 'var(--text-muted)', fontSize: 'var(--text-xs)' }}>
              No matching command
            </div>
          )}
          {matches.map((cmd, i) => (
            <div
              key={cmd.id}
              onClick={() => run(i)}
              onMouseEnter={() => setActive(i)}
              className="flex items-center justify-between cursor-pointer"
              style={{
                padding: '8px 12px',
                background: i === active ? 'var(--surface-hover)' : 'transparent',
                color: 'var(--text-primary)',
                fontSize: 'var(--text-sm)',
              }}
            >
              <span>{cmd.title}</span>
              {cmd.shortcut && (
                <kbd
                  style={{
                    fontFamily: 'var(--font-mono)',
                    fontSize: 'var(--text-2xs)',
                    color: 'var(--text-muted)',
                    border: '1px solid var(--border)',
                    borderRadius: 'var(--radius-sm)',
                    padding: '1px 5px',
                  }}
                >
                  {cmd.shortcut}
                </kbd>
              )}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
