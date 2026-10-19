---
name: design-canvas
description: "Design questions: when to show visual options on the canvas vs answer in text, and how to compose each canvas screen."
---

# Design canvas

The canvas shows one screen at a time in the user's browser: a question, an
optional mockup, and clickable options. Their pick comes back as a user
message. Use it when seeing beats reading.

## When to open the canvas (routing)

Decide per question, not per session. A question *about* UI is not
automatically a visual question: "what kind of wizard flow do you want?" is a
text question; "which of these two wizard layouts feels right?" is visual.

- **Pure text decisions** — "should we do X", "which first", "which tech" —
  answer inline in the terminal. Do NOT open the canvas; a text list is
  faster and cheaper. (v1 has no text-picker screen for these.)
- **Appearance questions** — "what should it look like", layout / styling /
  side-by-side comparison — open the canvas and show the candidates.
- **When unsure, open the canvas.** The cost is asymmetric: opening by
  mistake costs one screen of HTML and a glance; staying in text by mistake
  costs two or three rounds of trying to describe a picture in words — each
  round a full screen — plus the user's time.
  Not opening is the more expensive mistake.
- The user can switch medium at any moment ("用图给我看" / "就文字说"). Honor
  the switch and continue from what you already have; never make them
  restate the question.

## When to push a screen

- One screen = one decision point. Push when you have concrete candidates the
  user can judge side by side.
- Don't push for progress, status, or to re-show what they just picked — the
  terminal transcript already has that.
- Never push a question that is already on the screen or that they just
  answered — not as a variant, not "with more context". `canvas_push`
  replaces the screen, wiping the pick they just made. Extra detail and
  refinements go in terminal text.
- One live question at a time. When a pick settles the screen's question,
  deliver the outcome (a text summary, or a display-only screen of the chosen
  design) before asking anything else — a chain of question screens reads as
  an interrogation. A "continue working" nudge means add substance to what
  you owe; never re-ask a question that was already answered.
- After each turn the host replaces the screen with a "继续在终端讨论…"
  waiting screen (summary of decided / pending). You never craft that screen;
  your next `canvas_push` simply overwrites it.
- When the design question is settled, call `canvas_close`. Open tabs then
  show a farewell screen and the user can close the tab.

## What goes on a screen

`canvas_push` takes one envelope: a question, a picture of the candidates,
and 2–4 clickable picks.

- `title` — the question in one line, e.g. `大屏顶部导航，哪个布局更专业？`.
- `subtitle` — optional one-line context under it.
- `options` — 2–4 candidates: stable ASCII `id` (`a`, `b`, `c`), short
  `label`, optional one-line `note` naming what differs. Empty `options` = a
  display-only screen (no pick expected).
- `body_html` — a small HTML **fragment** for the visual half (mockups,
  comparison cards). The host wraps it in the page: never write `<html>`,
  `<head>`, or a full document.
- `multiselect: true` when several independent picks make sense (adds
  confirm / skip). Default is single-pick.

Build `body_html` from the component classes below. Write all user-visible
copy (title, labels, notes, mockup text) in the user's language.

- `.cards` / `.card` / `.card-body` / `.card-image` — side-by-side candidates
- `.split` with `.mock-nav` / `.mock-sidebar` / `.mock-content` — page layouts
- `.mockup` / `.mockup-header` / `.mockup-body` — a wireframe frame; inside it
  `.mock-button` / `.mock-input` for controls
- `.placeholder` — image or region slot
- `.pros-cons` / `.pros` / `.cons` — tradeoff lists
- `.notice` / `.note` / `.label` / `.section` — callouts and grouping

The options panel is generated from the `options` array — do not hand-write
`.options` / `.option` markup in `body_html`.

Match fidelity to the question: layout questions need wireframe-level
structure; styling questions need the actual colors, type, and texture —
without them the user is judging blind.

## While the canvas is open

- The browser opens only after the user consents (a one-time prompt). Accept
  the tool's return text — `awaiting consent` or `browser launched` — and
  keep working; never retry `canvas_open` to force a browser open.
- A pick arrives as a user message starting with `(画布选择)`. Treat it as
  the user's answer to that screen's question and continue from it — don't
  re-ask what they just answered.
