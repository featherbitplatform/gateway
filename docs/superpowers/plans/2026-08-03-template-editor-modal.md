# Template Editor Modal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A larger, centered modal editor for templated fields — suggestions above a big input, scrollable, wrap-not-truncate — opened from the VarInput popover ("Expand editor" / Ctrl+Space).

**Architecture:** Extract VarInput's token machinery (regex, caret detection, filtering, insertion incl. `insertEnvOnly`, brace consumption, `justInserted`) into a shared `useTemplateToken` hook; the modal and VarInput both consume it so behavior can't drift. Modal is Dialog-based with explicit Apply/Cancel.

**Tech Stack:** React 19 (ui/), Playwright. No new deps. UI-only.

**Spec:** `docs/superpowers/specs/2026-08-03-template-editor-modal-design.md`. Branch `feature/template-editor-modal` stacks on `feature/universal-templates` (PR targets develop after PR #11 merges).

## Global Constraints

- Conventional Commits, **no Co-Authored-By trailer**. No new npm/Rust dependencies.
- Task 1 is a **behavior-preserving refactor**: VarInput's observable behavior must be byte-identical (the existing e2e specs `templates.spec.ts` + `var-suggestions.spec.ts` are the parity net at the final gate).
- Insertion semantics are shared and unchanged: `{{path}}` inserts; `${NAME}` in `templateMode==='env-only'`; trailing `}}`/`}` consumption per the typed token's opening delimiter; `justInserted` one-shot suppresses reopen; `$` trigger only when `legacyDollar`.
- Modal commit semantics: Apply writes back via the field's normal onChange; Esc/Cancel discards; no implicit writes.
- Every task: `cd ui && npm run build && npm run lint` green at commit; paste actual output tails.

---

### Task 1: Extract `useTemplateToken` (behavior-preserving)

**Files:**
- Create: `ui/src/templateToken.ts`
- Modify: `ui/src/components/VarInput.tsx` (consume the hook; JSX/styling unchanged)

**Interfaces:**
- Produces (consumed by Task 2):

```ts
export interface TemplateTokenArgs {
  value: string;
  onChange: (v: string) => void;
  elRef: React.RefObject<HTMLInputElement | HTMLTextAreaElement | null>;
  suggestions: Suggestion[];
  templateMode: 'full' | 'env-only';
  legacyDollar: boolean;
}
export interface TemplateTokenState {
  open: boolean;
  filtered: Suggestion[];        // trigger- and mode-filtered, ready to render
  activeIndex: number;
  setActiveIndex: (i: number) => void;
  onKeyDown: (e: React.KeyboardEvent) => boolean; // true = handled (caller stops)
  onValueEvent: () => void;      // call from onChange/onSelect/keyup paths (token re-detection)
  insert: (s: Suggestion) => void;
  close: () => void;
}
export function useTemplateToken(args: TemplateTokenArgs): TemplateTokenState;
```

- [ ] **Step 1: Read `ui/src/components/VarInput.tsx` fully.** Inventory every piece of token logic: `TOKEN_RE`, `syncTokenFromCaret`, trigger tagging (`activeTrigger`), filtering (trigger + templateMode + substring), `insertSuggestion` (tokenStart/spliceEnd, `isDollarBraceToken`/`isDoubleBraceToken` brace consumption, `insertEnvOnly` selection, `pendingCaret`, no-op guard), the `[value]`-keyed caret effect, `justInserted`, blur timeout. List them in your report as the extraction checklist.
- [ ] **Step 2: Create `ui/src/templateToken.ts`** moving that logic verbatim into `useTemplateToken` (state + effects + handlers), exporting the interface above. The hook owns: open/filter/active state, `pendingCaret`/`justInserted` refs, the caret effect. It does NOT own: popover rendering, availability, footer — those stay in VarInput.
- [ ] **Step 3: Rewire VarInput** to consume the hook. The rendered DOM (`data-testid="var-popover"`, rows, footer, styles) must not change — diff the JSX before/after to confirm only logic moved.
- [ ] **Step 4: Verify** — `cd ui && npm run build && npm run lint`; manually hand-trace two flows in the report (type `{{re` → Enter inserts and closes without reopen; `$ur` in a legacyDollar field completes) confirming identical code paths now route through the hook.
- [ ] **Step 5: Commit** — `refactor(ui): extract shared template token hook from VarInput`

---

### Task 2: `TemplateEditorModal` + VarInput integration

**Files:**
- Create: `ui/src/components/TemplateEditorModal.tsx`
- Modify: `ui/src/components/VarInput.tsx`

**Interfaces:**
- Consumes: `useTemplateToken` (Task 1), `Dialog`/`DialogButton` (`ui/src/components/Dialog.tsx` — read it; reuse, don't fork), `Suggestion`/`Availability`/`previewValue` from `ui/src/varSuggestions.ts`.
- Produces: `TemplateEditorModalProps { open: boolean; onClose: () => void; value: string; onApply: (v: string) => void; suggestions: Suggestion[]; availability: Availability; templateMode: 'full' | 'env-only'; legacyDollar: boolean; onOpenLegend: () => void; }`.

- [ ] **Step 1: Build the modal.** Dialog ~720px (`width={720}`), title "Template editor". Content order per the spec: (a) scrollable suggestion panel FIRST (maxHeight '40vh', overflowY auto, `data-testid="template-editor-suggestions"`), grouped with the same eyebrow labels as the popover; row = mono path (no truncation — `overflowWrap: 'anywhere'`), value preview dimmed right-aligned, wrapping to a second full-width dimmed line when long (flex-wrap layout); `<redacted>`/`(empty)`/note styling copied from VarInput's row rendering (import/reuse its `ValueCell` if exported, else extract it to a tiny shared component in this task); (b) a `<textarea rows={4}>` holding the DRAFT value (local state initialized from `props.value` each time `open` flips true), mono, full width, `data-testid="template-editor-input"`; (c) availability footer line + "Context vars reference" link (`onOpenLegend`); (d) Dialog footer: Cancel (ghost) + Apply (primary).
- Wire `useTemplateToken` on the draft: the suggestion panel shows `filtered` when a token is active, else ALL suggestions (browsing mode — the panel is always populated so the user can inspect; clicking a row with no active token inserts at the caret as if the token were empty: pass through `insert` with tokenless behavior — extend the hook ONLY IF this needs a flag `insertAtCaretWhenNoToken: true`; document the choice). Keyboard nav works while the textarea is focused (`onKeyDown` delegating to the hook first).
- Apply → `onApply(draft)`; Esc and Cancel → `onClose()` without applying (Dialog's existing Esc handling — verify it calls onClose).
- Modal stays open after insertion.
- [ ] **Step 2: VarInput integration.** Footer of the popover gains an "Expand editor" button BEFORE the legend link (visible always when the popover is open; `aria-label="Expand template editor"`); `Ctrl+Space` (onKeyDown, when not handled by the hook) opens the modal even when the popover is closed. VarInput holds `modalOpen` state and renders `<TemplateEditorModal open={modalOpen} value={value} onApply={(v) => { onChange(v); setModalOpen(false); }} onClose={() => setModalOpen(false)} ...pass-through props />`. Opening the modal closes the popover.
- [ ] **Step 3: Verify** — build + lint; hand-trace in the report: open modal via button and via Ctrl+Space; type `{{re` in the modal textarea → panel filters → Enter inserts `{{request.method}}` and panel returns to browse mode; Apply writes the draft to the field; Esc discards.
- [ ] **Step 4: Commit** — `feat(ui): template editor modal with expandable suggestion panel`

---

### Task 3: E2E + testbook

**Files:**
- Modify: `e2e/tests/templates.spec.ts` (add scenario), `e2e/E2E_TESTBOOK.md`

- [ ] **Step 1: Scenario E2E-TPL-05** (reuse the spec file's existing setup idioms — read it first): seed the traced request with a long header (`upgrade-insecure-requests: 1` is sent by Chromium automatically; safer: send an explicit long custom header `x-a-very-long-custom-header-name-for-modal: value-123`). In the inspector on a templated field: type `{{` → popover visible → click `button[aria-label="Expand template editor"]` → `[data-testid="template-editor-modal"]`... (the Dialog needs a testid — add `data-testid="template-editor-modal"` to the modal Dialog content in Task 2 if not already; verify) → assert the suggestion row for `request.headers.x-a-very-long-custom-header-name-for-modal` renders its FULL text (locator by exact full string, and assert its value `value-123` visible); scroll the panel (`data-testid="template-editor-suggestions"`); type `{{request.headers.x-a-very` in the modal input → row filters → Enter → input contains the full `{{request.headers...}}`; click Apply → field input in the inspector contains the same; reopen modal, append text, press Escape → field unchanged.
- [ ] **Step 2: Run** — `cargo build --release` (UI embedded), `npx playwright test templates.spec.ts`, then FULL `npm test`. Paste summaries.
- [ ] **Step 3: Testbook** entry E2E-TPL-05; commit — `test(e2e): template editor modal scenario`

---

### Task 4: Final verification + reviews

- [ ] `cd ui && npm run build && npm run lint`; full `cargo test` (should be untouched — UI-only branch); `cargo build --release && cd e2e && npm test` FULL (the parity net for Task 1's refactor).
- [ ] Manual: narrow inspector → `{{` → Expand → long names/values readable, panel scrolls, Apply/Esc semantics.
- [ ] superpowers:requesting-code-review (final whole-branch), then superpowers:finishing-a-development-branch (PR → develop once PR #11 is merged; note the stack).
