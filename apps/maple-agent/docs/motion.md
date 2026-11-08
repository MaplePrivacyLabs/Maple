# Motion

Maple Work moves only to explain a change: where something came from, what
state it is in now, or that the app heard the user. Motion never stands
between the user and the next action. This page records the shared decisions;
`app/src/ui/motion.rs` implements them.

## Rules

- **Purpose first.** Each motion shows feedback, a change of state, or where
  something went. If removing it loses nothing, remove it.
- **Never delay input.** Content is clickable and typeable the moment it is
  drawn. Sends, keyboard shortcuts, Escape and Stop act on the frame they
  arrive; nothing waits for an animation to finish.
- **Interruptible.** Every transition that can reverse (hover, reveal,
  switches, chevrons, the composer, the sidebar) retargets from the value on
  screen. A second click never jumps back to an endpoint or restarts.
- **Fast in, faster out.** Entrances ease out. Menus, popovers and dialogs
  close at once: dismissal is the user's decision and the next action must
  not wait for it. Reversible state changes run the same curve both ways.
- **Opacity first, small distances.** gpui cannot scale or transform a `div`,
  so content enters with opacity and at most a 6 px rise, drawn as a
  relative offset that does not change layout. Only SVG icons scale or
  rotate. Height animates only where a box truly changes size (text fields,
  the expanded composer, the sidebar).
- **High-frequency work stays still.** Streaming tokens, typed characters,
  keyboard navigation inside menus and lists, and history that loads or
  scrolls into view do not animate. Rows animate once, when they arrive
  live, never again when they scroll back into view.
- **No bounce, no stagger.** No overshoot or elastic curves anywhere; items
  that arrive together appear together.
- **Reduced motion.** Settings → General → Reduce motion settles every
  transition at its end state on the next frame and holds looping
  indicators still. Every primitive in `motion.rs` honors it; new motion
  must use those primitives rather than reading time itself.

## Tokens

| Token | Duration | Curve | Used for |
|---|---|---|---|
| `HOVER` | 120 ms | smoothstep | hover plates, controls that appear on hover |
| `REVEAL` | 140 ms | ease-out quint | menus, popovers, tooltips, notices, page and pane changes |
| `TOGGLE` | 150 ms | ease-out cubic | switches, checkboxes, chevrons, icon swaps, status changes |
| `ENTER` | 180 ms | ease-out quint | live transcript rows, cards above the composer, dialogs |
| `PANEL` | 200 ms | ease-out cubic | sidebar, composer expand and shrink |
| `resize_duration` | 120–200 ms | ease-out cubic | text fields growing and shrinking with their text, by distance |

Press feedback is immediate (`.active()`), and focus rings appear at once.
Looping indicators (spinners, the waiting dots, the running ring) use the
shared 15 fps `ticker` clock and pause in background windows.

## Where motion applies

| Surface | In | Out |
|---|---|---|
| Hover on buttons, rows, menu items | background eases in, `HOVER` | eases back, `HOVER` |
| Menus, dropdowns, popovers | fade and 4 px slide away from the trigger, `REVEAL` | immediate |
| Tooltips | fade, `REVEAL`, after the existing delay | immediate |
| Dialogs and confirmations | scrim fades, card fades and rises, `ENTER` | immediate |
| Screen change (Chat ↔ Settings) and Settings sections | new content fades in, `REVEAL` | immediate |
| Switching tasks | transcript fades in when its snapshot lands, `REVEAL` | immediate |
| Live transcript rows (sent message, reply, tool card, error, stop notice) | fade and 6 px rise, `ENTER` | — |
| Streaming text | grows in place, no animation | — |
| Cards above the composer, queued messages, send preview | fade and rise, `ENTER` | immediate |
| Composer and multi-line fields | height eases to the new line count, `resize_duration` | same |
| Composer expand / shrink | height eases, `PANEL`; maximize and minimize icons cross-fade and scale, `TOGGLE` | same, reversed |
| Send ↔ Stop | icons cross-fade and scale, `TOGGLE` | same |
| Switches and checkboxes | knob slides, fill cross-fades, `TOGGLE` | same, reversed |
| Disclosure chevrons | rotate 90°, `TOGGLE` | same, reversed |
| Sidebar collapse | width eases, `PANEL` | same, reversed |
| Task status in the sidebar | indicators cross-fade, `TOGGLE` | same |

Loading spinners for work that usually finishes quickly (opening a task)
appear only after 300 ms, so fast loads never flash a spinner.

## Implementation

- `Tween` is the one retargetable value: it starts from the value on screen
  and settles under reduced motion. Views that own a transition (the sidebar,
  the composer) keep a `Tween` in their state, sample it in `render`, and call
  `window.request_animation_frame()` only while it moves.
- `animated(id, target, ...)` keeps a `Tween` in element state for render code
  that owns no state (chevrons, switches, icon swaps). It does not animate on
  mount unless asked with `enter_from`.
- `fade_in` and `rise_in` are one-shot entrances keyed by element id. A new
  id replays them; use that for content swaps (a Settings section, a task).
- Transcript rows sit in a `gpui::list`, whose items lose element state when
  they scroll away. Their entrance is driven by the arrival time kept in view
  state, so a row animates once and never replays.
- Every animated frame re-renders the window. Keep animations short, request
  frames only while something moves, and never animate inside cached views
  without an element id that is stable across frames.
- Tests that assert settled layout opt into reduced motion
  (`cx.set_reduce_motion(true)`); motion tests sample deterministic time.
