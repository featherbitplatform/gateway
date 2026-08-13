# Template Editor Modal — Design Spec

Date: 2026-08-03
Status: approved (brainstormed and validated with Francesco)

## Context

Composing `{{...}}` references inside the node inspector's fixed-width rail is cramped: the suggestion popover truncates long paths (`request.headers.upgrade-insecure-req…`) and value previews exactly where they need to be read. This feature adds a **larger, centered modal editor** for templated fields, opened on demand from the popover, with room to read and scroll every suggestion.

Decisions made during brainstorming:

- **Entry**: "Expand editor" button in the inline popover footer + `Ctrl+Space` from any templated field. The inline popover keeps working unchanged for quick picks; the modal never opens uninvited.
- **Layout** (per Francesco's sketch): suggestions **above** a large multiline input; the list scrollable (~40vh); rows full-width with the path in mono and the live value beside/beneath it, **wrapping instead of truncating** (long values may take a second dimmed line).
- **Editing model**: the modal input holds the whole field value; token detection, filtering, keyboard navigation, insertion rules (including `${NAME}` in env-only fields, trailing-brace consumption, no reopen after insert) are **shared code** with the popover — extracted into a common hook so the two cannot drift.
- **Commit semantics**: explicit **Apply** (writes back to the field via the normal onChange and closes) / **Cancel & Esc** (discard modal edits). No implicit writes.
- **Scope**: UI-only. Branch stacks on `feature/universal-templates` (depends on its VarInput machinery); PR targets develop after PR #11 merges.

## Design

### Components

- **`ui/src/components/TemplateEditorModal.tsx`** (new): Dialog-based (~720px), props `{ open, onClose, value, templateMode, legacyDollar, suggestions, availability, onOpenLegend, onApply(value) }`. Internal state: draft value + caret; suggestion list rendered above the input; group eyebrows; availability footer + legend link (legend opens over/after the modal). Insertion keeps the modal open. Apply → `onApply(draft)`, close. Esc/Cancel → close, no write. `data-testid="template-editor-modal"`.
- **Shared token hook** (`ui/src/templateToken.ts`, new): extract from `VarInput` the token regex, caret-token detection, filtering, insertion (incl. `insertEnvOnly` selection, brace consumption, `justInserted` one-shot) into `useTemplateToken({ value, setValue, elRef, suggestions, templateMode, legacyDollar })` returning `{ open, filtered, activeIndex, keyHandlers, insert, closePopover, activeTokenPrefix }`. `VarInput` and the modal both consume it.
- **`VarInput.tsx`**: popover footer gains a prominent "Expand editor" button (always visible in the footer, before the legend link); `Ctrl+Space` in the field opens the modal directly. VarInput owns the modal open state and passes its own props through; on Apply it fires its `onChange` with the new value.

### Row layout in the modal list

Single row: `path` (mono, no truncation, wraps if needed) — value preview right-aligned dimmed; if the value exceeds the remaining width, it moves to a second dimmed full-width line under the path. `<redacted>`/`(empty)`/notes styled as in the popover. Env rows show name only.

### Testing

- Build/lint gates.
- E2E (extends `templates.spec.ts` or new spec): in the inspector, type `{{` in a templated field → popover shows the Expand button → click → modal opens; assert a long suggestion (`request.headers.upgrade-insecure-requests` seeded via a request header) is fully visible (no `…` in its row) together with its value; pick it; Apply; assert the field value contains the inserted path. Esc path: reopen, edit, Esc → field unchanged.

## Critical files

`ui/src/components/TemplateEditorModal.tsx` (new), `ui/src/templateToken.ts` (new), `ui/src/components/VarInput.tsx`, e2e spec + testbook.

## Verification

- `cd ui && npm run build && npm run lint`
- `cargo build --release && cd e2e && npm test` incl. the new scenario
- Manual: narrow inspector field → `{{` → Expand → long header paths and values fully readable and scrollable
