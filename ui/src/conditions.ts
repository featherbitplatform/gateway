/**
 * Condition-expression model and triple-array serialization for the policy
 * builder's condition editor (e.g. `if`/branch nodes and fault-injection
 * `vars`).
 *
 * This is the TypeScript counterpart of the Rust condition parser in
 * `src/vars/mod.rs` (`Expr::parse`): the triple-array dialect where a rule is
 * `[subject, op, value]`, the negated `[subject, "!", op, value]`, or a unary
 * `[subject, "present"|"absent"|"is_null"]`; groups are `["AND"|"OR", ...]`
 * or `["NOT", child]` (heads case-insensitive). Subjects are either flat var
 * names (`http_*`, `arg_*`, `cookie_*`, or a bare var name) or JSONPath
 * queries over a request/response JSON body (`$...`, `request_body:$...`,
 * `response_body:$...`).
 *
 * {@link ConditionNode} is the builder-friendly model the UI edits;
 * {@link toExpr}/{@link fromExpr} convert it to/from the triple-array form
 * used in policy YAML. {@link toVarsList}/{@link fromVarsList} handle the
 * fault-injection-style `vars` list, which is an OR-of-ANDed-expressions
 * rather than a single top-level AND.
 *
 * Round-trip fidelity: `fromExpr(toExpr(m))` deep-equals `m` for any model
 * built by this module's own constructors, and `toExpr(fromExpr(e))` equals
 * `e` for any expression this module can represent. Anything the builder
 * model cannot represent (non-scalar operands, malformed groups, ...) makes
 * `fromExpr`/`fromVarsList` return `null` rather than a mangled model. Two
 * narrow, deliberate exceptions to byte-identical round-tripping: `!=` is
 * normalized to its `~=` alias at parse time (identical Rust-side semantics),
 * and a `var`-subject name typed with a legacy `$`/`${...}` wrapper (the
 * `VarInput` catalog-suggestion convention) has that wrapper stripped at
 * serialize time (see {@link stripLegacyDollar}) -- neither changes what the
 * expression means.
 *
 * Pure module: no React imports, no side effects.
 *
 * @module conditions
 */

/** Where a rule's subject comes from. */
export type SubjectKind =
  | 'header'
  | 'query'
  | 'cookie'
  | 'var'
  | 'jsonpath-request'
  | 'jsonpath-response';

/** How a scalar rule's `value` string serializes in the triple-array form. */
export type ValueType = 'string' | 'number' | 'boolean';

/** A single condition rule (leaf node). */
export interface ConditionRule {
  kind: 'rule';
  subject: SubjectKind;
  /** header/query/cookie/var name, or the JSONPath (`$...`). */
  name: string;
  negate: boolean;
  op: string;
  /** Scalar-op operand, as typed by the user. */
  value: string;
  /** How `value` serializes for JSONPath subjects (and scalar ops generally). */
  valueType: ValueType;
  /** Operand for `in` / `ipmatch`. */
  values: string[];
}

/** A logical group of rules/groups (AND/OR, optionally negated). */
export interface ConditionGroup {
  kind: 'group';
  logic: 'AND' | 'OR';
  negate: boolean;
  children: ConditionNode[];
}

export type ConditionNode = ConditionRule | ConditionGroup;

/** Unary operators: no operand, just `[subject, op]` (or negated). */
export const UNARY_OPS: readonly string[] = ['present', 'absent', 'is_null'];

/** List operators: operand is an array, `[subject, op, [...]]`. */
export const LIST_OPS: readonly string[] = ['in', 'ipmatch'];

/** All scalar/unary/list operators supported by the Rust `Expr` evaluator. */
const ALL_OPS: readonly string[] = [
  '==',
  '~=',
  '>',
  '>=',
  '<',
  '<=',
  '~~',
  '~*',
  'in',
  'has',
  'ipmatch',
  'present',
  'absent',
  'is_null',
  'contains',
];

/** Operators valid for the subject kind (`is_null` only for jsonpath-*). */
export function opsFor(subject: SubjectKind): string[] {
  const isJsonPath = subject === 'jsonpath-request' || subject === 'jsonpath-response';
  return isJsonPath ? [...ALL_OPS] : ALL_OPS.filter((op) => op !== 'is_null');
}

/**
 * Strips a legacy `$name`/`${name}` wrapper, for the `var` subject's name
 * field: it's rendered as a `VarInput` (see ConditionBuilder's `RuleRow`)
 * whose catalog suggestions insert `$`-prefixed text (the same convention
 * every other `$var` field in the app uses), but the wire form of a `var`
 * subject is the bare name with no `$` -- a leading `$` there would instead
 * be sniffed as a JSONPath subject by {@link parseSubject}. A name typed
 * without the `$` passes through unchanged.
 */
function stripLegacyDollar(name: string): string {
  if (name.startsWith('${') && name.endsWith('}')) return name.slice(2, -1);
  if (name.startsWith('$')) return name.slice(1);
  return name;
}

/** Serializes a rule's (subject, name) pair into its wire-form subject string. */
function serializeSubject(subject: SubjectKind, name: string): string {
  switch (subject) {
    case 'header':
      return `http_${name.toLowerCase().replace(/-/g, '_')}`;
    case 'query':
      return `arg_${name}`;
    case 'cookie':
      return `cookie_${name}`;
    case 'var':
      return stripLegacyDollar(name);
    case 'jsonpath-request':
      return name;
    case 'jsonpath-response':
      return `response_body:${name}`;
  }
}

/** Parses a wire-form subject string into its (subject kind, name) pair. */
function parseSubject(raw: string): { subject: SubjectKind; name: string } {
  if (raw.startsWith('response_body:')) {
    return { subject: 'jsonpath-response', name: raw.slice('response_body:'.length) };
  }
  if (raw.startsWith('request_body:')) {
    return { subject: 'jsonpath-request', name: raw.slice('request_body:'.length) };
  }
  if (raw.startsWith('$')) {
    return { subject: 'jsonpath-request', name: raw };
  }
  if (raw.startsWith('http_')) {
    return { subject: 'header', name: raw.slice('http_'.length).replace(/_/g, '-') };
  }
  if (raw.startsWith('arg_')) {
    return { subject: 'query', name: raw.slice('arg_'.length) };
  }
  if (raw.startsWith('cookie_')) {
    return { subject: 'cookie', name: raw.slice('cookie_'.length) };
  }
  return { subject: 'var', name: raw };
}

/** Coerces a rule's string `value` into its serialized (typed) form. */
function coerceScalar(value: string, valueType: ValueType): unknown {
  if (valueType === 'number') {
    const n = Number(value);
    return Number.isNaN(n) ? value : n;
  }
  if (valueType === 'boolean') {
    return value === 'true';
  }
  return value;
}

/** Parses a scalar operand back into (value, valueType); null if non-scalar. */
function parseScalarOperand(operand: unknown): { value: string; valueType: ValueType } | null {
  if (typeof operand === 'number') return { value: String(operand), valueType: 'number' };
  if (typeof operand === 'boolean') return { value: String(operand), valueType: 'boolean' };
  if (typeof operand === 'string') return { value: operand, valueType: 'string' };
  return null;
}

/**
 * Converts a list-op (`in`/`ipmatch`) operand array into string values; null
 * if any element isn't already a string.
 *
 * Numbers/bools are deliberately rejected rather than stringified: the
 * builder's list editor only ever produces string items, and stringifying a
 * numeric/bool item on parse would round-trip `["$.user.age","in",[30,40]]`
 * into `["30","40"]`, which breaks native JSONPath equality (age `30` would
 * never match the string `"30"` again). Minimum-fidelity fix: such lists stay
 * unrepresentable in the builder (-> raw-JSON mode) rather than being
 * silently mangled; typed in-list authoring is a deferred follow-up.
 */
function toListValues(arr: unknown[]): string[] | null {
  const out: string[] = [];
  for (const el of arr) {
    if (typeof el === 'string') out.push(el);
    else return null;
  }
  return out;
}

/** Serializes a single rule into its triple-array form. */
function serializeRule(rule: ConditionRule): unknown[] {
  const arr: unknown[] = [serializeSubject(rule.subject, rule.name)];
  if (rule.negate) arr.push('!');
  arr.push(rule.op);
  if (UNARY_OPS.includes(rule.op)) return arr;
  if (LIST_OPS.includes(rule.op)) {
    arr.push([...rule.values]);
    return arr;
  }
  arr.push(coerceScalar(rule.value, rule.valueType));
  return arr;
}

/** Parses a triple-array (non-group) entry into a rule; null if unrepresentable. */
function parseRule(arr: unknown[]): ConditionRule | null {
  if (arr.length < 2) return null;
  const subjectRaw = arr[0];
  if (typeof subjectRaw !== 'string') return null;

  let idx = 1;
  let negate = false;
  if (arr[idx] === '!') {
    negate = true;
    idx++;
  }
  const opRaw = arr[idx];
  if (typeof opRaw !== 'string') return null;
  idx++;
  // `!=` is the Rust engine's alias for `~=` (identical semantics) but isn't
  // offered in the builder's operator dropdown -- normalize it at parse time
  // so the select doesn't silently display `==` while `~=`/`!=` executes.
  // This is a byte-level spelling change on round-trip, accepted per the
  // controller ruling (finding 3a).
  const op = opRaw === '!=' ? '~=' : opRaw;
  // Any operator outside the builder's dialect (ALL_OPS) makes the rule
  // unparseable -> the caller falls back to raw-JSON mode instead of
  // rendering a dropdown that doesn't actually contain the op in play
  // (finding 3b).
  if (!ALL_OPS.includes(op)) return null;
  const remaining = arr.slice(idx);
  const { subject, name } = parseSubject(subjectRaw);

  if (UNARY_OPS.includes(op)) {
    if (remaining.length !== 0) return null;
    return { kind: 'rule', subject, name, negate, op, value: '', valueType: 'string', values: [] };
  }
  if (LIST_OPS.includes(op)) {
    if (remaining.length !== 1 || !Array.isArray(remaining[0])) return null;
    const values = toListValues(remaining[0] as unknown[]);
    if (values === null) return null;
    return { kind: 'rule', subject, name, negate, op, value: '', valueType: 'string', values };
  }
  if (remaining.length !== 1) return null;
  const scalar = parseScalarOperand(remaining[0]);
  if (scalar === null) return null;
  return {
    kind: 'rule',
    subject,
    name,
    negate,
    op,
    value: scalar.value,
    valueType: scalar.valueType,
    values: [],
  };
}

/** Serializes a node (rule or group) into its triple-array form. */
function serializeNode(node: ConditionNode): unknown[] {
  return node.kind === 'rule' ? serializeRule(node) : serializeGroup(node);
}

/** Serializes a group into its triple-array form, handling the NOT-wrap case. */
function serializeGroup(group: ConditionGroup): unknown[] {
  const childrenArr = group.children.map(serializeNode);
  const base = [group.logic, ...childrenArr];
  if (!group.negate) return base;
  if (group.logic === 'AND' && group.children.length === 1) {
    return ['NOT', childrenArr[0]];
  }
  return ['NOT', base];
}

/** Parses a single triple-array entry into a rule or group node; null if unrepresentable. */
function parseNode(v: unknown): ConditionNode | null {
  if (!Array.isArray(v) || v.length === 0) return null;
  const head = v[0];
  if (typeof head === 'string') {
    const upper = head.toUpperCase();
    if (upper === 'AND' || upper === 'OR') {
      const children: ConditionNode[] = [];
      for (const c of v.slice(1)) {
        const parsed = parseNode(c);
        if (parsed === null) return null;
        children.push(parsed);
      }
      return { kind: 'group', logic: upper as 'AND' | 'OR', negate: false, children };
    }
    if (upper === 'NOT') {
      const rest = v.slice(1);
      if (rest.length !== 1) return null;
      const child = parseNode(rest[0]);
      if (child === null) return null;
      if (child.kind === 'rule') {
        return { kind: 'group', logic: 'AND', negate: true, children: [child] };
      }
      // Child is a group. If it's already negated, flattening (`{...child,
      // negate: true}` is a no-op negate: true -> negate: true) would
      // silently drop this NOT and its child's NOT collapses into a single
      // negation -- inverting the predicate on round-trip (double negation
      // is identity, not negation). Wrap instead so both NOTs survive.
      // A non-negated child group still flattens as before: NOT of a plain
      // AND/OR group is exactly that group with negate flipped on.
      if (child.negate) {
        return { kind: 'group', logic: 'AND', negate: true, children: [child] };
      }
      return { ...child, negate: true };
    }
  }
  return parseRule(v);
}

/** Model -> triple-array. Root group serializes as the top-level rule list. */
export function toExpr(root: ConditionGroup): unknown[] {
  if (root.logic === 'AND' && !root.negate) {
    return root.children.map(serializeNode);
  }
  return [serializeGroup(root)];
}

/** Triple-array -> model; null when not representable in the builder. */
export function fromExpr(v: unknown): ConditionGroup | null {
  if (!Array.isArray(v)) return null;
  const children: ConditionNode[] = [];
  for (const el of v) {
    const parsed = parseNode(el);
    if (parsed === null) return null;
    children.push(parsed);
  }
  return { kind: 'group', logic: 'AND', negate: false, children };
}

/** For fault-injection-style `vars`: OR-of-expressions -> root OR group of AND groups. */
export function toVarsList(root: ConditionGroup): unknown[] {
  return root.children.map((child) => {
    if (child.kind === 'group') {
      return child.children.map(serializeNode);
    }
    return [serializeNode(child)];
  });
}

/** OR root of AND groups -> fault-injection-style `vars` list; null if unrepresentable. */
export function fromVarsList(v: unknown): ConditionGroup | null {
  if (!Array.isArray(v)) return null;
  const children: ConditionNode[] = [];
  for (const exprArr of v) {
    if (!Array.isArray(exprArr)) return null;
    const andChildren: ConditionNode[] = [];
    for (const ruleArr of exprArr) {
      const parsed = parseNode(ruleArr);
      if (parsed === null) return null;
      andChildren.push(parsed);
    }
    children.push({ kind: 'group', logic: 'AND', negate: false, children: andChildren });
  }
  return { kind: 'group', logic: 'OR', negate: false, children };
}

/** Fresh empty condition model (implicit top-level AND). */
export function emptyExpr(): ConditionGroup {
  return { kind: 'group', logic: 'AND', negate: false, children: [] };
}

/** Fresh empty vars-list model (OR of AND groups). */
export function emptyVarsList(): ConditionGroup {
  return { kind: 'group', logic: 'OR', negate: false, children: [] };
}
