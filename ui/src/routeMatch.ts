/**
 * Pure helpers behind the route dialog's match-rule form: parsing the
 * hosts field, building a {@link MatchRule} from the form, and the reverse
 * for the edit flow. Kept free of React so they are unit-testable.
 */
import type { MatchRule } from "./types";

/** Every method the form offers; selecting all of them means "any method". */
export const ALL_METHODS = [
  "GET",
  "POST",
  "PUT",
  "PATCH",
  "DELETE",
  "HEAD",
  "OPTIONS",
] as const;

/** The editable subset of a match rule. `hosts` is the raw text of the hosts field. */
export interface MatchForm {
  path: string;
  hosts: string;
  methods: string[];
}

/**
 * Splits the hosts field on commas, whitespace, and newlines; lower-cases
 * (host matching is case-insensitive anyway, so the stored form is tidy)
 * and drops blanks and duplicates while keeping first-seen order.
 */
export function parseHostList(text: string): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const raw of text.split(/[\s,]+/)) {
    const host = raw.trim().toLowerCase();
    if (!host || seen.has(host)) continue;
    seen.add(host);
    out.push(host);
  }
  return out;
}

function sameMethods(a: readonly string[], b: readonly string[]): boolean {
  if (a.length !== b.length) return false;
  const set = new Set(a);
  return b.every((m) => set.has(m));
}

/**
 * Builds the match rule to save. Unconstrained fields are omitted rather
 * than written as empty values: an empty hosts field or every method
 * selected both mean "match anything". Hosts are always written to `hosts`;
 * the legacy single `host` key is folded in by {@link matchRuleToForm} and
 * never written back. Any other constraint on `previous` (today: `headers`)
 * is carried over untouched because the form does not edit it.
 */
export function buildMatchRule(
  form: MatchForm,
  previous?: MatchRule,
): MatchRule {
  const rule: MatchRule = {};
  const path = form.path.trim();
  if (path) rule.path = path;
  const hosts = parseHostList(form.hosts);
  if (hosts.length > 0) rule.hosts = hosts;
  const methods = form.methods.map((m) => m.toUpperCase());
  if (methods.length > 0 && !sameMethods(methods, ALL_METHODS))
    rule.methods = methods;
  if (previous?.headers && Object.keys(previous.headers).length > 0) {
    rule.headers = previous.headers;
  }
  return rule;
}

/** All host patterns of a rule, legacy `host` first. */
export function allHosts(rule: MatchRule): string[] {
  return [...(rule.host ? [rule.host] : []), ...(rule.hosts ?? [])];
}

/** The form state for editing an existing rule. */
export function matchRuleToForm(rule: MatchRule): MatchForm {
  const methods =
    rule.methods && rule.methods.length > 0
      ? rule.methods.map((m) => m.toUpperCase())
      : [...ALL_METHODS];
  return { path: rule.path ?? "", hosts: allHosts(rule).join(", "), methods };
}

/**
 * One-line summary for the sidebar row: the path, prefixed by the first
 * host and a `+N` count when more are configured.
 */
export function describeMatch(rule: MatchRule): string {
  const path = rule.path || "/";
  const hosts = allHosts(rule);
  if (hosts.length === 0) return path;
  const extra = hosts.length > 1 ? ` +${hosts.length - 1}` : "";
  return `${hosts[0]}${extra}${path}`;
}
