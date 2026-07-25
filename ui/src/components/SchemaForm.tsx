/**
 * Declarative config form for gateway plugin nodes. Given the FieldSchema[]
 * describing a plugin type (from pluginConfig.ts), renders one labelled input
 * per field and emits the node's `config` object exactly as it is serialized
 * into gateway.yaml via the admin API.
 *
 * @module components/SchemaForm
 */
import { Plus, X } from 'lucide-react';
import type { FieldOption, FieldSchema } from '../pluginConfig';

/** Props for SchemaForm. */
interface SchemaFormProps {
  /** Ordered field descriptors for the selected plugin type (see PLUGIN_SCHEMAS in pluginConfig.ts). */
  schema: FieldSchema[];
  /** Current plugin config object; each field reads `value[field.key]`, falling back to `field.default`. */
  value: Record<string, unknown>;
  /** Called with the full replacement config object on every edit (controlled-component style). */
  onChange: (config: Record<string, unknown>) => void;
}

const inputStyle: React.CSSProperties = {
  width: '100%',
  padding: '6px 10px',
  borderRadius: 'var(--radius-sm)',
  fontFamily: 'var(--font-mono)',
  fontSize: 'var(--text-sm)',
  background: 'var(--surface-input)',
  color: 'var(--text-primary)',
  border: '1px solid var(--border)',
};

const labelStyle: React.CSSProperties = {
  display: 'block',
  fontSize: 'var(--text-xs)',
  fontWeight: 500,
  color: 'var(--text-secondary)',
  marginBottom: 4,
};

const hintStyle: React.CSSProperties = {
  fontSize: 'var(--text-2xs)',
  color: 'var(--text-muted)',
  margin: '4px 0 0',
};

/** Expands shorthand string options into `{ value, label }` pairs (label = value). */
function normalizeOptions(options: (string | FieldOption)[] = []): FieldOption[] {
  return options.map((o) => (typeof o === 'string' ? { value: o, label: o } : o));
}

/** Dashed full-width "Add …" button used by list/objects fields to append a row. */
function AddButton({ label, onClick }: { label: string; onClick: () => void }) {
  return (
    <button
      onClick={onClick}
      className="w-full flex items-center justify-center gap-1 transition-colors"
      style={{
        padding: '5px 0',
        borderRadius: 'var(--radius-sm)',
        fontSize: 'var(--text-xs)',
        fontWeight: 500,
        background: 'transparent',
        color: 'var(--accent-hover)',
        border: '1px dashed var(--border-strong)',
      }}
      onMouseEnter={(e) => {
        e.currentTarget.style.borderColor = 'var(--accent-ring)';
        e.currentTarget.style.background = 'var(--accent-soft)';
      }}
      onMouseLeave={(e) => {
        e.currentTarget.style.borderColor = 'var(--border-strong)';
        e.currentTarget.style.background = 'transparent';
      }}
    >
      <Plus size={12} />
      Add {label}
    </button>
  );
}

/** Small "x" button that deletes one row from a list/objects field; `label` is the aria-label. */
function RemoveButton({ onClick, label }: { onClick: () => void; label: string }) {
  return (
    <button
      onClick={onClick}
      className="flex items-center justify-center shrink-0 rounded transition-colors"
      style={{ width: 24, height: 24, color: 'var(--text-muted)' }}
      onMouseEnter={(e) => (e.currentTarget.style.color = 'var(--error)')}
      onMouseLeave={(e) => (e.currentTarget.style.color = 'var(--text-muted)')}
      aria-label={label}
    >
      <X size={13} />
    </button>
  );
}

/** Segmented single-choice control for small enums that should stay visible. */
function RadioGroup({
  options,
  value,
  onChange,
}: {
  options: FieldOption[];
  value: string;
  onChange: (v: string) => void;
}) {
  return (
    <div
      className="flex"
      style={{
        gap: 2,
        padding: 2,
        borderRadius: 'var(--radius-sm)',
        background: 'var(--surface-sunken)',
        border: '1px solid var(--border-subtle)',
      }}
      role="radiogroup"
    >
      {options.map((opt) => {
        const active = value === opt.value;
        return (
          <button
            key={opt.value}
            role="radio"
            aria-checked={active}
            onClick={() => onChange(opt.value)}
            className="flex-1 transition-colors"
            style={{
              padding: '4px 6px',
              borderRadius: 'var(--radius-xs)',
              fontFamily: 'var(--font-mono)',
              fontSize: 'var(--text-2xs)',
              fontWeight: 500,
              background: active ? 'var(--surface-active)' : 'transparent',
              color: active ? 'var(--accent-hover)' : 'var(--text-secondary)',
              boxShadow: active ? 'inset 0 0 0 1px var(--accent-ring)' : 'none',
              whiteSpace: 'nowrap',
            }}
          >
            {opt.label}
          </button>
        );
      })}
    </div>
  );
}

/** Boolean toggle. */
function Switch({
  checked,
  label,
  onChange,
}: {
  checked: boolean;
  label?: string;
  onChange: (v: boolean) => void;
}) {
  return (
    <button
      role="switch"
      aria-checked={checked}
      onClick={() => onChange(!checked)}
      className="flex items-center gap-2"
      style={{ background: 'transparent', border: 'none', padding: 0, cursor: 'pointer' }}
    >
      <span
        style={{
          width: 30,
          height: 17,
          borderRadius: 'var(--radius-pill)',
          background: checked ? 'var(--accent)' : 'var(--surface-input)',
          border: '1px solid var(--border)',
          position: 'relative',
          transition: 'background var(--dur-base) var(--ease-out)',
          flexShrink: 0,
        }}
      >
        <span
          style={{
            position: 'absolute',
            top: 1,
            left: checked ? 14 : 1,
            width: 13,
            height: 13,
            borderRadius: '50%',
            background: '#fff',
            transition: 'left var(--dur-base) var(--ease-out)',
          }}
        />
      </span>
      {label && (
        <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)' }}>{label}</span>
      )}
    </button>
  );
}

/**
 * Renders an editable plugin-config form from a declarative field schema.
 * Fully controlled; keys in `value` that are not in the schema are preserved,
 * so hand-edited or newer config keys survive a round-trip through the form.
 *
 * Per-field rendering and the serialized value each type produces:
 * - `text` — single-line text input; serializes to a string.
 * - `number` — numeric input; serializes to a number, or `undefined` when cleared
 *   (so the key is dropped from the YAML rather than sent as an empty string).
 * - `textarea` — multi-line input (`rows`, default 4) for bodies/templates/scripts;
 *   serializes to a string.
 * - `radio` — segmented button group for small always-visible enums
 *   (e.g. rate-limit `strategy`); serializes to the selected option's string value.
 * - `select` — native dropdown for longer enums (e.g. `content_type`);
 *   serializes to the selected option's string value.
 * - `switch` — boolean toggle with optional inline `switchLabel`; serializes to a boolean.
 * - `list` — add/remove rows of a single scalar input (`field.item.type` picks
 *   text or number); serializes to an array of strings or numbers
 *   (e.g. cors `allow_origins` → `["https://app.example.com"]`). New rows start
 *   as `''` (text) or `0` (number).
 * - `objects` — add/remove cards, each rendering the sub-inputs in `field.fields`;
 *   serializes to an array of records keyed by the sub-field keys
 *   (e.g. proxy-rewrite `add_headers` → `[{ name, value }]`, basic-auth `users`
 *   → `[{ username, password }]`). New cards are pre-filled from each sub-field's
 *   `default`, else `0`/`''` by sub-field type; a numeric sub-field cleared to
 *   empty serializes as `undefined`.
 *
 * Every field shows its `label` above the input and optional `hint` text below.
 *
 * @returns The stacked form fields for the given schema.
 * @remarks Schemas live in ui/src/pluginConfig.ts (the `pluginConfig` map, keyed
 * by the gateway's plugin node types and looked up via getPluginConfigSchema).
 * The emitted config object is what the admin API persists as the node's
 * `config` block in gateway.yaml.
 */
export function SchemaForm({ schema, value, onChange }: SchemaFormProps) {
  const set = (key: string, v: unknown) => onChange({ ...value, [key]: v });

  /** Renders the input control for one field, switching on `field.type` as documented above. */
  const renderField = (field: FieldSchema) => {
    const current = value[field.key];

    switch (field.type) {
      case 'text':
        return (
          <input
            type="text"
            value={(current as string) ?? (field.default as string) ?? ''}
            placeholder={field.placeholder}
            onChange={(e) => set(field.key, e.target.value)}
            style={inputStyle}
          />
        );

      case 'number':
        return (
          <input
            type="number"
            value={(current as number) ?? (field.default as number) ?? ''}
            placeholder={field.placeholder}
            onChange={(e) =>
              set(field.key, e.target.value === '' ? undefined : Number(e.target.value))
            }
            style={inputStyle}
          />
        );

      case 'textarea':
        return (
          <textarea
            value={(current as string) ?? (field.default as string) ?? ''}
            placeholder={field.placeholder}
            rows={field.rows ?? 4}
            onChange={(e) => set(field.key, e.target.value)}
            className="resize-y"
            style={{ ...inputStyle, fontSize: 'var(--text-xs)' }}
          />
        );

      case 'radio':
        return (
          <RadioGroup
            options={normalizeOptions(field.options)}
            value={(current as string) ?? (field.default as string) ?? ''}
            onChange={(v) => set(field.key, v)}
          />
        );

      case 'select':
        return (
          <select
            value={(current as string) ?? (field.default as string) ?? ''}
            onChange={(e) => set(field.key, e.target.value)}
            style={{ ...inputStyle, appearance: 'auto' }}
          >
            {normalizeOptions(field.options).map((opt) => (
              <option key={opt.value} value={opt.value}>
                {opt.label}
              </option>
            ))}
          </select>
        );

      case 'switch':
        return (
          <Switch
            checked={(current as boolean) ?? (field.default as boolean) ?? false}
            label={field.switchLabel}
            onChange={(v) => set(field.key, v)}
          />
        );

      case 'list': {
        const items = Array.isArray(current) ? (current as unknown[]) : [];
        return (
          <div className="space-y-1.5">
            {items.map((item, i) => (
              <div key={i} className="flex items-center gap-1.5">
                <input
                  type={field.item?.type === 'number' ? 'number' : 'text'}
                  value={(item as string | number) ?? ''}
                  placeholder={field.item?.placeholder}
                  onChange={(e) => {
                    const next = [...items];
                    next[i] =
                      field.item?.type === 'number' ? Number(e.target.value) : e.target.value;
                    set(field.key, next);
                  }}
                  style={inputStyle}
                />
                <RemoveButton
                  label={`Remove ${field.addLabel ?? 'item'} ${i + 1}`}
                  onClick={() => set(field.key, items.filter((_, j) => j !== i))}
                />
              </div>
            ))}
            <AddButton
              label={field.addLabel ?? 'item'}
              onClick={() => set(field.key, [...items, field.item?.type === 'number' ? 0 : ''])}
            />
          </div>
        );
      }

      case 'objects': {
        const items = Array.isArray(current)
          ? (current as Record<string, unknown>[])
          : [];
        const blank = () =>
          Object.fromEntries(
            (field.fields ?? []).map((f) => [f.key, f.default ?? (f.type === 'number' ? 0 : '')])
          );
        return (
          <div className="space-y-2">
            {items.map((item, i) => (
              <div
                key={i}
                style={{
                  padding: 10,
                  borderRadius: 'var(--radius-sm)',
                  border: '1px solid var(--border-subtle)',
                  background: 'var(--surface-sunken)',
                }}
              >
                <div className="flex items-center justify-between" style={{ marginBottom: 6 }}>
                  <span className="eyebrow">
                    {field.itemLabel ?? 'Item'} {i + 1}
                  </span>
                  <RemoveButton
                    label={`Remove ${field.itemLabel ?? 'item'} ${i + 1}`}
                    onClick={() => set(field.key, items.filter((_, j) => j !== i))}
                  />
                </div>
                <div className="space-y-2">
                  {(field.fields ?? []).map((sub) => (
                    <div key={sub.key}>
                      <label style={labelStyle}>{sub.label}</label>
                      <input
                        type={sub.type === 'number' ? 'number' : 'text'}
                        value={(item[sub.key] as string | number) ?? ''}
                        placeholder={sub.placeholder}
                        onChange={(e) => {
                          const next = items.map((it, j) =>
                            j === i
                              ? {
                                  ...it,
                                  [sub.key]:
                                    sub.type === 'number'
                                      ? e.target.value === ''
                                        ? undefined
                                        : Number(e.target.value)
                                      : e.target.value,
                                }
                              : it
                          );
                          set(field.key, next);
                        }}
                        style={inputStyle}
                      />
                    </div>
                  ))}
                </div>
              </div>
            ))}
            <AddButton
              label={field.addLabel ?? 'item'}
              onClick={() => set(field.key, [...items, blank()])}
            />
          </div>
        );
      }
    }
  };

  return (
    <div className="space-y-4">
      {schema.map((field) => (
        <div key={field.key}>
          <label style={labelStyle}>{field.label}</label>
          {renderField(field)}
          {field.hint && <p style={hintStyle}>{field.hint}</p>}
        </div>
      ))}
    </div>
  );
}
