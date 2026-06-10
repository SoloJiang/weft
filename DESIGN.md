# Design

Visual system for weft. Dark-primary, high-density, calm. Built on a
violet-tinted near-black architecture with an **indigo** brand (the weft mark's
three work lines) and an **orange** convergence accent (the single outcome dot).
All color in OKLCH. Component base: **shadcn/ui (Radix + Tailwind)**, retuned to
these tokens — never the default shadcn look.

> Status today: the M1 shell uses plain CSS as a functional placeholder. This
> document is the target system; M2+ product UI is built to it, and the M1
> shell is migrated to it in a polish pass.

## Theme

**Dark + light, toggleable.** Default follows the OS (`prefers-color-scheme`),
the choice persists (localStorage `weft-theme`), applied via `data-theme` on
`<html>` (an inline script in `index.html` sets it before first paint — no
flash). Dark mood: a control room at night — violet-tinted near-black, the
**indigo** brand glowing like the work lines of the weft mark, **orange** marking
convergence. Light mood: violet-tinted paper, the same brand deepened for
contrast. Both are designed palettes, not inverts.

Mechanism: Tailwind `@theme` colors reference per-theme `--c-*` vars; dark is the
`:root` default, light overrides under `:root[data-theme="light"]`. Add a color
only as a `--c-*` pair (dark + light).

Color strategy: **committed**. The brand is sourced from the weft mark
(`public/weft-*.svg`): INDIGO `#4F46E5` = the three parallel work lines
(structure/primary); ORANGE `#F2683C` = the converged outcome (accent). Surfaces
are violet-tinted near-black anchored to the mark's `#1C1B22` base. The brand
carries identity; the surface never competes.

## Color (OKLCH)

### Surface architecture (violet-tinted near-black, anchored to #1C1B22)

| Token | OKLCH | Use |
|---|---|---|
| `--bg` | `oklch(0.165 0.012 292)` | app background, the deepest layer |
| `--surface` | `oklch(0.205 0.013 292)` | panels, bars, cards |
| `--raised` | `oklch(0.245 0.014 292)` | popovers, menus, elevated rows |
| `--border` | `oklch(0.30 0.014 292)` | hairline separators, control borders |
| `--border-strong` | `oklch(0.37 0.016 292)` | focused/active borders |

Surfaces are differentiated by **lightness steps, not shadows**. Shadows are
reserved for genuinely floating layers (popover/modal/toast) at ≤ 8px blur.

### Ink (text) — all verified ≥ AA on `--surface`

| Token | OKLCH | Use |
|---|---|---|
| `--ink` | `oklch(0.96 0.005 292)` | primary text |
| `--ink-muted` | `oklch(0.76 0.010 292)` | secondary text, still ≥ 4.5:1 |
| `--ink-faint` | `oklch(0.62 0.012 292)` | labels, meta, placeholders (≥ 4.5:1) |

No text dimmer than `--ink-faint` for anything readable. Disabled-only may go
lower.

### Brand (indigo) + Accent (orange) — from the weft mark

| Token | OKLCH | Source | Use |
|---|---|---|---|
| `--brand` | `oklch(0.55 0.22 277)` | `#4F46E5` indigo | primary actions, active selection, mark, focus ring |
| `--brand-press` | `oklch(0.49 0.21 277)` | | pressed/active brand |
| `--brand-ink` | `oklch(0.99 0.005 277)` | white | text on an indigo fill |
| `--brand-ghost` | `oklch(0.55 0.22 277 / 0.16)` | | brand-tinted hover/selected backgrounds |
| `--accent` | `oklch(0.70 0.18 38)` | `#F2683C` orange | convergence/outcome moments, distinctive highlights |
| `--accent-ghost` | `oklch(0.70 0.18 38 / 0.16)` | | accent-tinted backgrounds |

Indigo is the brand; orange is the convergence accent (the mark's single outcome
dot). Both used sparingly. Note the brand is **decoupled from status** — "running"
is its own green (below), not the brand, so structure and liveness read distinctly.

### Status semantics (the only other saturated colors; always paired with icon + label)

| State | Token | OKLCH | Glyph |
|---|---|---|---|
| running / active | `--status-running` | `oklch(0.73 0.16 150)` emerald | ● pulse |
| waiting-input | `--status-waiting` | `oklch(0.80 0.13 80)` amber | ◐ |
| waiting-approval | `--status-approval` | `oklch(0.74 0.17 45)` orange | ⚠ |
| injecting (program) | `--status-inject` | `oklch(0.72 0.12 215)` cyan | ↳ |
| paused / idle | `--status-idle` | `oklch(0.64 0.015 292)` slate | ○ |
| error / exited | `--status-error` | `oklch(0.64 0.20 25)` red | ✕ |

The tables above are the **dark** palette (`:root`). The **light** palette
(`:root[data-theme="light"]`) keeps the same hues, flipped for a near-white
violet paper: `--c-bg` `oklch(0.975 0.004 292)`, `--c-surface` `oklch(0.995 0.002 292)`,
`--c-raised` white, `--c-ink` `oklch(0.26 0.02 292)`, brand deepened to the true
`#4F46E5` `oklch(0.51 0.23 277)`, status colors darkened (~L 0.55–0.62) for AA on
white. Exact values live in `src/index.css`; both tables stay in lockstep.

Color never stands alone — the glyph and a text label always accompany it
(see Accessibility in PRODUCT.md).

## Typography

Three families, contrast-paired (UI sans + mono; no second sans):

- **UI / display**: `Geist` (or `Inter` fallback). All headings, labels,
  body. Display = same family at larger size + tighter tracking.
- **Mono**: `Geist Mono` (or `JetBrains Mono`). Code, file paths, branch
  names, session ids, diffs, and other session metadata.

Scale (dense; base 13px). Ratio ≥ 1.25 between steps via size + weight.

| Role | Size / line-height | Weight | Tracking |
|---|---|---|---|
| display | 22px / 1.2 | 600 | -0.02em |
| h2 | 17px / 1.3 | 600 | -0.01em |
| h3 | 14px / 1.4 | 600 | 0 |
| body | 13px / 1.5 | 400 | 0 |
| label / meta | 12px / 1.4 | 500 | 0 |
| mono | 12–13px / 1.5 | 400 | 0 |

`text-wrap: balance` on headings. No all-caps body; uppercase only for ≤ 2-word
chips. Display ceiling well under the 6rem cap — this is a dense tool, not a
landing page.

## Layout & density

- 4px spacing base. Compact rhythm: 8 / 12 / 16 / 24 for most gaps.
- App shell: left nav (workspace ▸ issue ▸ direction), main session region,
  optional right rail (diff / thread bus). Flexbox for the shell, Grid only for
  true 2D (session grids, diff columns).
- Multi-session layouts: `repeat(auto-fit, minmax(360px, 1fr))` so panels reflow
  without breakpoint thrash.
- **Radius**: 8px cards/panels, 6px inputs/buttons, full pill for chips/tags.
  Never the over-rounded 24px+ look.
- Borders are 1px hairlines at `--border`; pair a border *or* a one-step surface
  lift, not both plus a shadow.
- Semantic z-index scale: `--z-nav: 10`, `--z-sticky: 20`, `--z-backdrop: 30`,
  `--z-modal: 40`, `--z-toast: 50`, `--z-tooltip: 60`. No magic 9999.

## Components

Base on **shadcn/ui** (Radix primitives + Tailwind), retokenized to the OKLCH
variables above via CSS custom properties. Tailwind config maps `bg`, `surface`,
`ink`, `brand`, `border`, status roles to these tokens. Key components and their
weft treatment:

- **Status chip**: pill, glyph + color + label, used in nav, session headers,
  lists. The single most-repeated atom — must be perfect.
- **Session panel**: the weft-owned chat timeline, framed with a header (tool,
  cwd, branch, status chip) and the §4.3 interaction layer (composer,
  Ask-Bridge approval bar, queued-message indicator).
- **Nav tree**: workspace → issue → direction, with active = `--brand-ghost`
  fill + `--brand` left-edge indicator (a 2px indicator, NOT a decorative
  side-stripe border).
- **Command palette** (⌘K): Radix dialog, keyboard-first, the primary
  navigation/action surface.
- **Diff view**: mono, per-repo, restrained add/remove tinting from the status
  ramp (green add, red remove) at low chroma so it stays calm.
- **Approval bar**: appears when a tool raises a permission ask (Ask Bridge);
  Allow / Always / Full / Deny buttons send the decision back to the blocked
  tool — a structured passthrough of the tool's own prompt, never a new gate.
- **Home (lead conversation)**: the default surface. A focused chat with the
  issue's lead — task in, scope/brief/decisions out — flanked by the board and
  a session region. Reads like a control tower, not a chatbot: structured cards
  (proposed scope, dispatched directions, escalations) inline in the stream, not
  walls of prose.
- **Board card (trust dashboard)**: the kanban atom. Carries a title, lifecycle
  column, the tool(s) in play, and **acceptance signals** (tests x/y, contract
  match, review-agent verdict) with expandable provenance. Green = trust at a
  glance; red / escalated draws the eye and opens on click. Never a flat
  icon-card grid — the signals are the content.
- **Two-level board**: Workspace board (cards = issues) zooms into the Issue board
  (cards = directions). `Needs you` is a pinned exception lane aggregated at both
  levels. Cards flow themselves; the human acts (Approve / Answer / Open /
  Review / Merge), it does not drag.
- **Scope confirm (write trigger)**: the post-decompose step — each direction
  shows which repos it will *write* (a per-repo on/off toggle the human
  corrects), then a single "create" materializes worktrees. Reads are
  unmanaged. This is the one human gate; everything else is automation +
  the tools' own permission prompts. The visible face of the core wow.

## Motion

Motion is part of the build, not a coat of paint. It exists to **explain a
change of state** and to make switching feel instant, never to perform.

- **Curves**: ease-out-expo / quint for entrances and movement. No bounce, no
  elastic, ever.
- **Durations**: 120–180ms for UI transitions (focus, hover, chip changes,
  panel switch); up to 240ms for larger layout reveals. Fast, because the tool
  is fast.
- **Library**: use `motion` (Framer Motion) for orchestrated panel/list
  transitions and shared-layout session switching; CSS transitions for simple
  hover/focus.
- **Signature moments (interaction & transition craft):**
  - *Status change*: the chip crossfades color + glyph; running pulses subtly
    (opacity 0.6↔1, 1.6s, reduced-motion = static).
  - *Session switch*: shared-layout transition so the active panel border and
    header animate to the new selection — continuity, not a hard cut.
  - *Injection queue*: a queued program message slides a banner down; on flush
    it briefly highlights the injected turn (cyan `--status-inject` wash that
    fades) so the human sees what the coordinator sent.
  - *Approval arrival*: the panel border eases to `--status-approval` and the
    approval bar slides up — a calm alert, not a flash.
  - *List reveals*: issue/session lists stagger entrance (24ms step) on first
    mount only; never re-stagger on every render.
- **Reduced motion**: every animation has a `prefers-reduced-motion: reduce`
  path — pulses stop, slides become instant, crossfades shorten to ~80ms.
- Don't animate layout props (width/height/top) where transform/opacity can do
  it; blur/clip-path only when they materially improve a moment and stay smooth.

## Slop guardrails (weft-specific)

Never ship: pastel/purple gradients, gradient text, glassmorphism as default,
decorative side-stripe borders (the nav indicator is a 2px functional marker,
not a 4px accent stripe), over-rounded cards, emoji as UI iconography in
production, neon-terminal styling, identical icon-card grids, per-section
uppercase eyebrows. If a screen could be guessed as "AI dashboard" from a
thumbnail, it has failed principle 1.
