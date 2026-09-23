/**
 * Sign-in for the admin UI, and the gate that decides when it shows.
 *
 * {@link LoginGate} wraps the whole app. With nothing stored it probes
 * `GET /api/status` first: a 401 shows the full-page sign-in and the editor
 * is not mounted at all. With stored credentials the editor mounts at once;
 * from then on a 401 from any later call (the password was changed server-side, or
 * the stored pair was edited) brings the same form up as an overlay *over*
 * the still-mounted editor, so unsaved canvas edits survive re-entering the
 * password. Sign out forgets the credentials and returns to the full page.
 *
 * @module components/LoginScreen
 */
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { LogIn } from 'lucide-react';
import { getCredentials, getUsername, onSignedOut, onUnauthorized, reportSignedIn, saveCredentials } from '../auth';
import { api, verifyCredentials } from '../api/client';

type GateState = 'checking' | 'signed-out' | 'signed-in';

export function LoginGate({ children }: { children: ReactNode }) {
  // With stored credentials the editor mounts at once (no probe round-trip
  // or blank flash); if the gateway rejects them, its first call's 401
  // raises the overlay below. With none, probe first: the browser may still
  // be authenticated on its own (e.g. a reverse proxy adding the header).
  const [state, setState] = useState<GateState>(() => (getCredentials() !== null ? 'signed-in' : 'checking'));
  // A 401 after sign-in: keep the editor mounted, overlay the form.
  const [expired, setExpired] = useState(false);
  const stateRef = useRef<GateState>(state);
  useEffect(() => {
    stateRef.current = state;
  }, [state]);

  useEffect(() => {
    let cancelled = false;
    if (stateRef.current === 'checking') {
      api
        .status()
        .then(() => !cancelled && setState('signed-in'))
        .catch((e) => {
          if (cancelled) return;
          // Anything but a 401 (gateway down, 5xx) is the editor's to report:
          // it has the "Connection Error" screen for exactly that.
          setState(String(e).includes('401') ? 'signed-out' : 'signed-in');
        });
    }
    // Only meaningful once signed in: the startup probe's own 401 and calls
    // made from the sign-in page are handled by the state above.
    const offUnauthorized = onUnauthorized(() => {
      if (stateRef.current === 'signed-in') setExpired(true);
    });
    const offSignedOut = onSignedOut(() => {
      setExpired(false);
      setState('signed-out');
    });
    return () => {
      cancelled = true;
      offUnauthorized();
      offSignedOut();
    };
  }, []);

  if (state === 'checking') return <div className="h-screen" style={{ background: 'var(--bg-app)' }} />;
  if (state === 'signed-out') {
    return (
      <div className="h-screen flex items-center justify-center" style={{ background: 'var(--bg-app)', padding: 16 }}>
        <LoginForm onSignedIn={() => setState('signed-in')} />
      </div>
    );
  }
  return (
    <>
      {children}
      {expired && (
        <div
          className="fixed inset-0 flex items-center justify-center"
          style={{ background: 'var(--overlay, rgba(0, 0, 0, 0.55))', zIndex: 1000, padding: 16 }}
        >
          <LoginForm
            expired
            initialUsername={getUsername() ?? ''}
            onSignedIn={() => {
              setExpired(false);
              reportSignedIn();
            }}
          />
        </div>
      )}
    </>
  );
}

const inputStyle = {
  width: '100%',
  padding: '8px 10px',
  borderRadius: 'var(--radius-sm)',
  border: '1px solid var(--border)',
  background: 'var(--surface-input)',
  color: 'var(--text-primary)',
  fontSize: 'var(--text-sm)',
} as const;

const labelStyle = {
  display: 'flex',
  flexDirection: 'column',
  gap: 4,
  fontSize: 'var(--text-xs)',
  color: 'var(--text-secondary)',
} as const;

function LoginForm({
  onSignedIn,
  expired = false,
  initialUsername = '',
}: {
  onSignedIn: () => void;
  expired?: boolean;
  initialUsername?: string;
}) {
  const [username, setUsername] = useState(initialUsername);
  const [password, setPassword] = useState('');
  const [remember, setRemember] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async () => {
    if (busy || username === '' || password === '') return;
    setBusy(true);
    setError(null);
    try {
      if (await verifyCredentials(username, password)) {
        saveCredentials(username, password, remember);
        onSignedIn();
      } else {
        setError('Wrong username or password.');
        setPassword('');
      }
    } catch (e) {
      setError(`Could not reach the gateway: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <form
      aria-label="Sign in"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
      style={{
        width: '100%',
        maxWidth: 340,
        padding: 28,
        borderRadius: 'var(--radius-md)',
        background: 'var(--surface)',
        border: '1px solid var(--border)',
        boxShadow: 'var(--shadow-md)',
        display: 'flex',
        flexDirection: 'column',
        gap: 14,
      }}
    >
      <div className="flex items-center gap-2.5">
        <img
          src="/featherbit-mark.png"
          alt=""
          style={{ height: 28, width: 'auto', filter: 'drop-shadow(var(--glow-violet))' }}
        />
        <div>
          <h1 style={{ margin: 0, fontSize: 'var(--text-md)', fontWeight: 'var(--weight-semibold)' as never, color: 'var(--text-primary)' }}>
            {expired ? 'Sign in again' : 'Sign in to featherbit'}
          </h1>
          <p style={{ margin: 0, fontSize: 'var(--text-2xs)', color: 'var(--text-muted)' }}>
            {expired
              ? 'The gateway no longer accepts your credentials. Unsaved edits are kept; retry your last action after signing in.'
              : 'Admin API credentials (admin.username / admin.password)'}
          </p>
        </div>
      </div>
      <label style={labelStyle}>
        Username
        <input
          value={username}
          onChange={(e) => setUsername(e.target.value)}
          autoComplete="username"
          autoFocus={!expired || initialUsername === ''}
          style={inputStyle}
        />
      </label>
      <label style={labelStyle}>
        Password
        <input
          type="password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          autoComplete="current-password"
          autoFocus={expired && initialUsername !== ''}
          style={inputStyle}
        />
      </label>
      <label className="flex items-center gap-2" style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)' }}>
        <input type="checkbox" checked={remember} onChange={(e) => setRemember(e.target.checked)} />
        Remember me on this browser
      </label>
      {error && (
        <p role="alert" style={{ margin: 0, fontSize: 'var(--text-xs)', color: 'var(--error)' }}>
          {error}
        </p>
      )}
      <button
        type="submit"
        disabled={busy || username === '' || password === ''}
        className="flex items-center justify-center gap-1.5"
        style={{
          padding: '8px 0',
          borderRadius: 'var(--radius-sm)',
          background: 'var(--accent)',
          color: 'var(--text-on-accent)',
          fontSize: 'var(--text-sm)',
          fontWeight: 'var(--weight-medium)' as never,
          opacity: busy || username === '' || password === '' ? 0.6 : 1,
        }}
      >
        <LogIn size={14} />
        {busy ? 'Signing in…' : 'Sign in'}
      </button>
    </form>
  );
}
