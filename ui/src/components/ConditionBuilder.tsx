/**
 * Visual builder for the triple-array condition-expression dialect defined
 * in `ui/src/conditions.ts` (the TypeScript counterpart of the Rust `Expr`
 * parser in `src/vars/mod.rs`). Renders an editable tree of AND/OR/NOT
 * groups and leaf rules (subject/op/value), with a raw-JSON fallback for
 * expressions the builder model cannot represent.
 *
 * Two shapes are supported (see {@link ConditionBuilderProps.shape}):
 * - `'expr'` — a single top-level (implicit AND) expression, used by
 *   `request-validation`'s `conditions` field.
 * - `'or-of-exprs'` — an OR-of-ANDed-expressions list (fault-injection-style
 *   `vars`); the root is a fixed OR and its direct children are fixed AND
 *   "condition sets", both non-negatable and non-retoggleable because
 *   {@link toVarsList} does not serialize logic/negate at those two levels.
 *
 * State model: `mode` toggles between `'builder'` (edits a parsed
 * {@link ConditionGroup} tree, emitting `toExpr`/`toVarsList` on every
 * change) and `'raw'` (edits JSON text directly, entered automatically
 * whenever the incoming value fails to parse). An empty root serializes to
 * `onChange(undefined)` rather than `[]`, since the Rust side treats an
 * absent/empty conditions list as vacuously true and the field is optional.
 *
 * @module components/ConditionBuilder
 */
import { useEffect, useRef, useState } from 'react';
import { Plus } from 'lucide-react';
import {
  type ConditionGroup,
  type ConditionNode,
  type ConditionRule,
  type SubjectKind,
  type ValueType,
  LIST_OPS,
  UNARY_OPS,
  emptyExpr,
  emptyVarsList,
  fromExpr,
  fromVarsList,
  opsFor,
  toExpr,
  toVarsList,
} from '../conditions';
import { AddButton, RadioGroup, RemoveButton, type VarContext } from './SchemaForm';
import { VarInput } from './VarInput';

/** Mirrors SchemaForm's `inputStyle` (not exported there — see its fast-refresh lint note). */
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

/** Props for {@link ConditionBuilder}. */
interface ConditionBuilderProps {
  /** Triple-array (shape `'expr'`) or list of them (shape `'or-of-exprs'`). */
  value: unknown;
  shape: 'expr' | 'or-of-exprs';
  onChange: (v: unknown) => void;
  /**
   * Live suggestions for the "Context var" rule row's name field, threaded
   * through from SchemaForm exactly like every other templated field (see
   * {@link VarContext}). When omitted (e.g. PluginConfigPanel, which has no
   * node selected to derive suggestions from), that field falls back to a
   * plain input with no autocomplete popover.
   */
  varContext?: VarContext;
}

/** Subject-kind options for the rule row's subject select, with a per-kind name placeholder. */
const SUBJECT_OPTIONS: { value: SubjectKind; label: string; placeholder: string }[] = [
  { value: 'header', label: 'Header', placeholder: 'authorization' },
  { value: 'query', label: 'Query param', placeholder: 'page' },
  { value: 'cookie', label: 'Cookie', placeholder: 'session' },
  { value: 'var', label: 'Context var', placeholder: 'remote_addr' },
  { value: 'jsonpath-request', label: 'JSONPath (request body)', placeholder: '$.user.name' },
  { value: 'jsonpath-response', label: 'JSONPath (response body)', placeholder: '$.data.id' },
];

/** Ops (within the non-unary, non-list scalar branch) that also expose a value-type select. */
const TYPED_SCALAR_OPS = ['==', 'has', 'contains'];

function blankRule(): ConditionRule {
  return {
    kind: 'rule',
    subject: 'header',
    name: '',
    negate: false,
    op: '==',
    value: '',
    valueType: 'string',
    values: [],
  };
}

function blankGroup(): ConditionGroup {
  return { kind: 'group', logic: 'AND', negate: false, children: [] };
}

/** Replaces, deletes (when `fn` returns null), or recurses into the child at `path`. */
function updateChildren(
  children: ConditionNode[],
  path: number[],
  fn: (node: ConditionNode) => ConditionNode | null
): ConditionNode[] {
  const [head, ...rest] = path;
  return children.flatMap((child, i) => {
    if (i !== head) return [child];
    if (rest.length === 0) {
      const updated = fn(child);
      return updated === null ? [] : [updated];
    }
    if (child.kind !== 'group') return [child];
    return [{ ...child, children: updateChildren(child.children, rest, fn) }];
  });
}

/** Appends `child` to the group at `path` (`[]` = root). */
function addChildAt(root: ConditionGroup, path: number[], child: ConditionNode): ConditionGroup {
  if (path.length === 0) return { ...root, children: [...root.children, child] };
  return {
    ...root,
    children: updateChildren(root.children, path, (node) =>
      node.kind === 'group' ? { ...node, children: [...node.children, child] } : node
    ),
  };
}

/** Removes the node at `path` (never called with `path: []` — root is not removable). */
function removeAt(root: ConditionGroup, path: number[]): ConditionGroup {
  if (path.length === 0) return root;
  return { ...root, children: updateChildren(root.children, path, () => null) };
}

/** Applies `fn` to the node at `path` (`[]` = root). */
function updateAt(
  root: ConditionGroup,
  path: number[],
  fn: (node: ConditionNode) => ConditionNode
): ConditionGroup {
  if (path.length === 0) return fn(root) as ConditionGroup;
  return { ...root, children: updateChildren(root.children, path, fn) };
}

function notButtonStyle(active: boolean): React.CSSProperties {
  return {
    padding: '4px 8px',
    borderRadius: 'var(--radius-sm)',
    fontFamily: 'var(--font-mono)',
    fontSize: 'var(--text-2xs)',
    fontWeight: 700,
    lineHeight: 1,
    background: active ? 'var(--surface-active)' : 'transparent',
    color: active ? 'var(--accent-hover)' : 'var(--text-secondary)',
    border: active ? '1px solid var(--accent-ring)' : '1px solid var(--border-subtle)',
    boxShadow: active ? 'inset 0 0 0 1px var(--accent-ring)' : 'none',
    flexShrink: 0,
  };
}

/** Compact add/remove editor for `in`/`ipmatch`'s string-array operand. */
function ListValueEditor({
  values,
  onChange,
}: {
  values: string[];
  onChange: (v: string[]) => void;
}) {
  return (
    <div className="flex items-center flex-wrap" style={{ gap: 4 }}>
      {values.map((v, i) => (
        <div key={i} className="flex items-center" style={{ gap: 2 }}>
          <input
            type="text"
            aria-label="Condition value"
            value={v}
            onChange={(e) => {
              const next = [...values];
              next[i] = e.target.value;
              onChange(next);
            }}
            style={{ ...inputStyle, width: 100 }}
          />
          <RemoveButton
            label={`Remove value ${i + 1}`}
            onClick={() => onChange(values.filter((_, j) => j !== i))}
          />
        </div>
      ))}
      <button
        onClick={() => onChange([...values, ''])}
        aria-label="Add value"
        className="flex items-center justify-center"
        style={{
          width: 24,
          height: 24,
          borderRadius: 'var(--radius-sm)',
          border: '1px dashed var(--border-strong)',
          color: 'var(--accent-hover)',
          background: 'transparent',
        }}
      >
        <Plus size={12} />
      </button>
    </div>
  );
}

/** One leaf rule row: subject, name, NOT, operator, value (shape depends on op), remove. */
function RuleRow({
  rule,
  path,
  onRemove,
  onUpdate,
  varContext,
}: {
  rule: ConditionRule;
  path: number[];
  onRemove: (path: number[]) => void;
  onUpdate: (path: number[], fn: (node: ConditionNode) => ConditionNode) => void;
  varContext?: VarContext;
}) {
  const patch = (partial: Partial<ConditionRule>) =>
    onUpdate(path, (node) => ({ ...(node as ConditionRule), ...partial }));

  const ops = opsFor(rule.subject);
  const isUnary = UNARY_OPS.includes(rule.op);
  const isList = LIST_OPS.includes(rule.op);
  const isJsonPath = rule.subject === 'jsonpath-request' || rule.subject === 'jsonpath-response';
  const showValueType = !isUnary && !isList && isJsonPath && TYPED_SCALAR_OPS.includes(rule.op);
  const subjectMeta = SUBJECT_OPTIONS.find((o) => o.value === rule.subject);

  const handleSubjectChange = (subject: SubjectKind) => {
    const validOps = opsFor(subject);
    const stillJsonPath = subject === 'jsonpath-request' || subject === 'jsonpath-response';
    patch({
      subject,
      op: validOps.includes(rule.op) ? rule.op : '==',
      // valueType only has a visible control for jsonpath-* subjects; reset it so a
      // stale 'number'/'boolean' doesn't silently coerce a flat-subject value with
      // no control left to fix it (e.g. ["http_foo", "==", 42] with no way to retype it).
      ...(stillJsonPath ? {} : { valueType: 'string' as const }),
    });
  };

  return (
    <div
      className="flex items-center flex-wrap"
      style={{ gap: 6 }}
      data-testid="condition-node"
      data-depth={path.length}
    >
      <select
        aria-label="Condition subject"
        value={rule.subject}
        onChange={(e) => handleSubjectChange(e.target.value as SubjectKind)}
        style={{ ...inputStyle, width: 'auto', appearance: 'auto' }}
      >
        {SUBJECT_OPTIONS.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </select>
      {rule.subject === 'var' && varContext ? (
        <VarInput
          value={rule.name}
          onChange={(v) => patch({ name: v })}
          placeholder={subjectMeta?.placeholder}
          style={{ ...inputStyle, width: 140 }}
          templateMode="full"
          legacyDollar
          ariaLabel="Condition name"
          {...varContext}
        />
      ) : (
        <input
          type="text"
          aria-label="Condition name"
          value={rule.name}
          placeholder={subjectMeta?.placeholder}
          onChange={(e) => patch({ name: e.target.value })}
          style={{ ...inputStyle, width: 140 }}
        />
      )}
      <button
        onClick={() => patch({ negate: !rule.negate })}
        aria-label="Negate rule"
        aria-pressed={rule.negate}
        style={notButtonStyle(rule.negate)}
      >
        !
      </button>
      <select
        aria-label="Condition operator"
        value={rule.op}
        onChange={(e) => patch({ op: e.target.value })}
        style={{ ...inputStyle, width: 'auto', appearance: 'auto' }}
      >
        {ops.map((op) => (
          <option key={op} value={op}>
            {op}
          </option>
        ))}
      </select>
      {!isUnary && isList && (
        <ListValueEditor values={rule.values} onChange={(values) => patch({ values })} />
      )}
      {!isUnary && !isList && (
        <>
          <input
            type="text"
            aria-label="Condition value"
            value={rule.value}
            onChange={(e) => patch({ value: e.target.value })}
            style={{ ...inputStyle, width: 140 }}
          />
          {showValueType && (
            <select
              aria-label="Condition value type"
              value={rule.valueType}
              onChange={(e) => patch({ valueType: e.target.value as ValueType })}
              style={{ ...inputStyle, width: 'auto', appearance: 'auto' }}
            >
              <option value="string">str</option>
              <option value="number">num</option>
              <option value="boolean">bool</option>
            </select>
          )}
        </>
      )}
      <RemoveButton label="Remove rule" onClick={() => onRemove(path)} />
    </div>
  );
}

/** One group card (AND/OR/NOT), recursively rendering its rule/group children. */
function GroupCard({
  group,
  path,
  depth,
  shape,
  indexInParent,
  onAddRule,
  onAddGroup,
  onRemove,
  onUpdate,
  varContext,
}: {
  group: ConditionGroup;
  path: number[];
  depth: number;
  shape: 'expr' | 'or-of-exprs';
  indexInParent: number;
  onAddRule: (path: number[]) => void;
  onAddGroup: (path: number[]) => void;
  onRemove: (path: number[]) => void;
  onUpdate: (path: number[], fn: (node: ConditionNode) => ConditionNode) => void;
  varContext?: VarContext;
}) {
  const isRoot = depth === 0;
  // Root (both shapes) and, for or-of-exprs, its direct "condition set" children
  // are fixed by toVarsList/toExpr — their logic/negate flags are either implicit
  // (expr root) or silently dropped by the serializer (or-of-exprs root + depth 1),
  // so no control is offered to set them there.
  const fixedLogic = shape === 'or-of-exprs' && depth <= 1;
  const showLogicToggle = !fixedLogic && group.children.length > 0;
  const showConditionSetLabel = shape === 'or-of-exprs' && depth === 1;
  // NOT mirrors fixedLogic: toVarsList reads only `.children` at these two levels
  // (root and its direct "condition set" children), so a negate toggle there would
  // be a dead control — active styling with zero effect on the serialized value.
  const hideNot = isRoot || fixedLogic;

  return (
    <div
      style={{
        padding: 10,
        borderRadius: 'var(--radius-sm)',
        border: '1px solid var(--border-subtle)',
        background: 'var(--surface-sunken)',
      }}
      data-testid="condition-node"
      data-depth={depth}
    >
      <div className="flex items-center justify-between" style={{ marginBottom: 8, gap: 8 }}>
        <div className="flex items-center" style={{ gap: 8 }}>
          {showConditionSetLabel && (
            <span className="eyebrow">Condition set {indexInParent + 1}</span>
          )}
          {showLogicToggle && (
            <RadioGroup
              ariaLabel="Group logic"
              options={[
                { value: 'AND', label: 'AND' },
                { value: 'OR', label: 'OR' },
              ]}
              value={group.logic}
              onChange={(v) =>
                onUpdate(path, (node) => ({
                  ...(node as ConditionGroup),
                  logic: v as 'AND' | 'OR',
                }))
              }
            />
          )}
          {!hideNot && (
            <button
              onClick={() =>
                onUpdate(path, (node) => ({
                  ...(node as ConditionGroup),
                  negate: !(node as ConditionGroup).negate,
                }))
              }
              aria-label="Negate group"
              aria-pressed={group.negate}
              style={notButtonStyle(group.negate)}
            >
              !
            </button>
          )}
        </div>
        {!isRoot && <RemoveButton label="Remove group" onClick={() => onRemove(path)} />}
      </div>

      <div className="space-y-2">
        {group.children.map((child, i) => {
          const childPath = [...path, i];
          return child.kind === 'group' ? (
            <GroupCard
              key={i}
              group={child}
              path={childPath}
              depth={depth + 1}
              shape={shape}
              indexInParent={i}
              onAddRule={onAddRule}
              onAddGroup={onAddGroup}
              onRemove={onRemove}
              onUpdate={onUpdate}
              varContext={varContext}
            />
          ) : (
            <RuleRow
              key={i}
              rule={child}
              path={childPath}
              onRemove={onRemove}
              onUpdate={onUpdate}
              varContext={varContext}
            />
          );
        })}
      </div>

      <div className="flex" style={{ gap: 8, marginTop: 8 }}>
        <div style={{ flex: 1 }}>
          <AddButton label="rule" onClick={() => onAddRule(path)} />
        </div>
        <div style={{ flex: 1 }}>
          <AddButton label="group" onClick={() => onAddGroup(path)} />
        </div>
      </div>
    </div>
  );
}

/**
 * Renders a builder UI (falling back to raw JSON) for one condition-expression
 * field. Fully controlled: `value` is the field's current config value (the
 * triple-array/list, or `undefined` when unset); `onChange` is called with
 * the re-serialized value (or `undefined` for an empty root) on every edit.
 *
 * @remarks
 * Holds parsed-model/raw-text state internally (unlike SchemaForm's other
 * field types, which are stateless), so it tracks the incoming `value` via a
 * "last value we ourselves emitted" ref and resyncs mode/model whenever
 * `value` changes for a reason other than its own `onChange` call (e.g. the
 * inspector switching to a different node, since SchemaForm's fields are not
 * remounted per node).
 */
export function ConditionBuilder({ value, shape, onChange, varContext }: ConditionBuilderProps) {
  const parseModel = (v: unknown): ConditionGroup | null =>
    shape === 'or-of-exprs' ? fromVarsList(v) : fromExpr(v);
  const emptyModel = (): ConditionGroup => (shape === 'or-of-exprs' ? emptyVarsList() : emptyExpr());
  const serializeModel = (m: ConditionGroup): unknown[] =>
    shape === 'or-of-exprs' ? toVarsList(m) : toExpr(m);

  const computeInitial = (v: unknown): { mode: 'builder' | 'raw'; model: ConditionGroup } => {
    if (v === undefined) return { mode: 'builder', model: emptyModel() };
    const parsed = parseModel(v);
    return parsed ? { mode: 'builder', model: parsed } : { mode: 'raw', model: emptyModel() };
  };

  const [mode, setMode] = useState<'builder' | 'raw'>(() => computeInitial(value).mode);
  const [model, setModel] = useState<ConditionGroup>(() => computeInitial(value).model);
  const [rawText, setRawText] = useState(() => JSON.stringify(value ?? [], null, 2));
  const [rawError, setRawError] = useState('');
  const lastEmitted = useRef<string | undefined>(JSON.stringify(value));

  useEffect(() => {
    const serialized = JSON.stringify(value);
    if (serialized === lastEmitted.current) return;
    lastEmitted.current = serialized;
    const initial = computeInitial(value);
    setMode(initial.mode);
    setModel(initial.model);
    setRawText(JSON.stringify(value ?? [], null, 2));
    setRawError('');
    // computeInitial/parseModel/etc. close over `shape`, which is included below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [value, shape]);

  const emit = (newModel: ConditionGroup) => {
    setModel(newModel);
    const out = newModel.children.length > 0 ? serializeModel(newModel) : undefined;
    lastEmitted.current = JSON.stringify(out);
    onChange(out);
  };

  const canGoVisual = parseModel(value) !== null;

  const toggleMode = () => {
    if (mode === 'builder') {
      setRawText(JSON.stringify(value ?? [], null, 2));
      setRawError('');
      setMode('raw');
      return;
    }
    const parsed = parseModel(value);
    if (parsed) {
      setModel(parsed);
      setMode('builder');
    }
  };

  const handleApplyRaw = () => {
    let parsed: unknown;
    try {
      parsed = JSON.parse(rawText);
    } catch {
      setRawError('Invalid JSON');
      return;
    }
    if (!Array.isArray(parsed)) {
      setRawError('Invalid JSON');
      return;
    }
    setRawError('');
    const out = parsed.length > 0 ? parsed : undefined;
    lastEmitted.current = JSON.stringify(out);
    onChange(out);
    const asModel = parseModel(parsed);
    if (asModel) {
      setModel(asModel);
      setMode('builder');
    }
  };

  const modeToggleDisabled = mode === 'raw' && !canGoVisual;

  return (
    <div>
      <button
        onClick={toggleMode}
        disabled={modeToggleDisabled}
        style={{
          background: 'transparent',
          border: 'none',
          padding: 0,
          marginBottom: 6,
          fontSize: 'var(--text-2xs)',
          fontWeight: 500,
          color: modeToggleDisabled ? 'var(--text-muted)' : 'var(--accent-hover)',
          cursor: modeToggleDisabled ? 'not-allowed' : 'pointer',
        }}
      >
        {mode === 'builder' ? 'Edit as JSON' : 'Edit visually'}
      </button>
      {modeToggleDisabled && (
        <p style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', margin: '0 0 6px' }}>
          expression not representable in the builder
        </p>
      )}

      {mode === 'builder' ? (
        <GroupCard
          group={model}
          path={[]}
          depth={0}
          shape={shape}
          indexInParent={0}
          onAddRule={(path) => emit(addChildAt(model, path, blankRule()))}
          onAddGroup={(path) => emit(addChildAt(model, path, blankGroup()))}
          onRemove={(path) => emit(removeAt(model, path))}
          onUpdate={(path, fn) => emit(updateAt(model, path, fn))}
          varContext={varContext}
        />
      ) : (
        <div>
          <textarea
            value={rawText}
            onChange={(e) => setRawText(e.target.value)}
            rows={8}
            className="w-full resize-y"
            style={{
              ...inputStyle,
              fontSize: 'var(--text-xs)',
              border: `1px solid ${rawError ? 'var(--error)' : 'var(--border)'}`,
            }}
          />
          {rawError && (
            <p style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-xs)', color: 'var(--error)', margin: '4px 0 0' }}>
              {rawError}
            </p>
          )}
          <button
            onClick={handleApplyRaw}
            className="mt-2 w-full transition-colors"
            style={{
              padding: '7px 0',
              borderRadius: 'var(--radius-sm)',
              fontSize: 'var(--text-sm)',
              fontWeight: 500,
              background: 'var(--accent)',
              color: 'var(--text-on-accent)',
            }}
          >
            Apply
          </button>
        </div>
      )}
    </div>
  );
}
