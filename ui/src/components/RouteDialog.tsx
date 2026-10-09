/**
 * The create / edit route dialog: route name (fixed when editing), match
 * path, an optional hosts list, and a method chip set. The match rule it
 * saves is built by `routeMatch.ts`; everything here is presentation.
 */
import { useState } from "react";
import { Dialog, DialogButton, DialogField } from "./Dialog";
import {
  ALL_METHODS,
  buildMatchRule,
  matchRuleToForm,
  type MatchForm,
} from "../routeMatch";
import type { MatchRule } from "../types";

interface RouteDialogProps {
  open: boolean;
  /** `create` asks for a name and shows "Create route"; `edit` fixes the name and shows "Save route". */
  mode: "create" | "edit";
  /** The route being edited (edit mode); its name and match prefill the form. */
  route?: { name: string; match: MatchRule };
  onClose: () => void;
  /** Called with the trimmed name and the built match rule. */
  onSubmit: (name: string, match: MatchRule) => void;
}

const EMPTY_FORM: MatchForm = {
  path: "/*",
  hosts: "",
  methods: [...ALL_METHODS],
};

/**
 * Mounts the form only while open, so its state is initialised fresh from
 * `route` on every open (and discarded on close) without an effect.
 */
export function RouteDialog(props: RouteDialogProps) {
  if (!props.open) return null;
  return <RouteDialogBody {...props} />;
}

function RouteDialogBody({
  open,
  mode,
  route,
  onClose,
  onSubmit,
}: RouteDialogProps) {
  const [name, setName] = useState(() => route?.name ?? "");
  const [form, setForm] = useState<MatchForm>(() =>
    route ? matchRuleToForm(route.match) : EMPTY_FORM,
  );

  const toggleMethod = (method: string) => {
    setForm((f) => ({
      ...f,
      methods: f.methods.includes(method)
        ? f.methods.filter((m) => m !== method)
        : [...f.methods, method],
    }));
  };

  const trimmedName = name.trim();
  const canSubmit = trimmedName.length > 0 && form.methods.length > 0;

  const submit = () => {
    if (!canSubmit) return;
    onSubmit(trimmedName, buildMatchRule(form, route?.match));
  };

  return (
    <Dialog
      open={open}
      title={mode === "create" ? "New route" : "Edit route"}
      onClose={onClose}
      width={420}
      footer={
        <>
          <DialogButton variant="ghost" onClick={onClose}>
            Cancel
          </DialogButton>
          <DialogButton onClick={submit} disabled={!canSubmit}>
            {mode === "create" ? "Create route" : "Save route"}
          </DialogButton>
        </>
      }
    >
      {mode === "create" ? (
        <DialogField
          label="Route name"
          value={name}
          onChange={setName}
          placeholder="echo-api"
          autoFocus
        />
      ) : (
        <div style={{ marginBottom: 12 }}>
          <div style={fieldLabel}>Route name</div>
          <div
            style={{
              fontFamily: "var(--font-mono)",
              fontSize: "var(--text-sm)",
              color: "var(--text-primary)",
              padding: "7px 0",
            }}
          >
            {name}
          </div>
        </div>
      )}
      <DialogField
        label="Match path"
        value={form.path}
        onChange={(path) => setForm((f) => ({ ...f, path }))}
        placeholder="/api/*"
        mono
        autoFocus={mode === "edit"}
      />
      <div style={{ marginBottom: 12 }}>
        <label htmlFor="route-hosts" style={fieldLabel}>
          Hosts
        </label>
        <input
          id="route-hosts"
          type="text"
          value={form.hosts}
          placeholder="api.example.com, *.example.org"
          onChange={(e) => setForm((f) => ({ ...f, hosts: e.target.value }))}
          className="rg-field w-full"
          style={inputStyle}
        />
        <div style={hint}>
          Optional. Comma-separated; <code>*.</code> matches one label. The
          request port is ignored. Empty matches every host.
        </div>
      </div>
      <div>
        <div style={fieldLabel}>Methods</div>
        <div className="flex flex-wrap" style={{ gap: 6 }}>
          {ALL_METHODS.map((method) => {
            const on = form.methods.includes(method);
            return (
              <label
                key={method}
                style={{
                  display: "inline-flex",
                  alignItems: "center",
                  gap: 5,
                  padding: "3px 9px",
                  borderRadius: 999,
                  fontFamily: "var(--font-mono)",
                  fontSize: "var(--text-xs)",
                  cursor: "pointer",
                  userSelect: "none",
                  background: on
                    ? "var(--accent-soft)"
                    : "transparent",
                  color: on ? "var(--text-primary)" : "var(--text-muted)",
                  border: `1px solid ${on ? "var(--accent-border)" : "var(--border)"}`,
                }}
              >
                <input
                  type="checkbox"
                  checked={on}
                  onChange={() => toggleMethod(method)}
                  aria-label={method}
                  style={{ width: 11, height: 11, margin: 0 }}
                />
                {method}
              </label>
            );
          })}
        </div>
        <div style={hint}>
          {form.methods.length === ALL_METHODS.length
            ? "All methods selected: the route accepts any method."
            : form.methods.length === 0
              ? "Select at least one method."
              : "Requests with any other method are not routed."}
        </div>
      </div>
    </Dialog>
  );
}

const fieldLabel: React.CSSProperties = {
  display: "block",
  fontSize: "var(--text-xs)",
  fontWeight: 500,
  color: "var(--text-secondary)",
  marginBottom: 4,
};

const inputStyle: React.CSSProperties = {
  outline: 'none',
  padding: "7px 10px",
  borderRadius: "var(--radius-sm)",
  fontFamily: "var(--font-mono)",
  fontSize: "var(--text-sm)",
  color: "var(--text-primary)",
};

const hint: React.CSSProperties = {
  marginTop: 5,
  fontSize: "var(--text-2xs)",
  color: "var(--text-muted)",
  lineHeight: 1.4,
};
