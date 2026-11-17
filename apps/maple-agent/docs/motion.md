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
  arrive; nothing waits for an animation to finish. The composer is where
  the user types, so where it sits never animates: it changes place at once
  between a new task and a conversation, and it empties at once on a send.
- **Interruptible.** Every transition that can reverse (hover, reveal,
  switches, chevrons, the composer, the sidebar) is a spring that retargets
  from the value on screen and keeps the speed it has. A second click never
  jumps back to an endpoint, restarts, or turns a corner.
- **Fast in, faster out.** Entrances ease out. Menus, popovers and dialogs
  close at once: dismissal is the user's decision and the next action must
  not wait for it. Reversible state changes run the same curve both ways.
- **Opacity first, small distances.** gpui cannot scale or transform a `div`,
  so content enters with opacity and a short rise (6 px; 8 px for dialog
  cards), drawn as a paint offset: layout stays put, and the hitbox moves
  with the paint. Only SVG icons scale or rotate.
  Height animates only where a box truly changes size: text fields, the
  expanded composer, the sidebar, and a row arriving at the end of the
  thread, which opens so older rows glide up rather than jump.
- **High-frequency work stays still.** Streaming tokens, typed characters,
  keyboard navigation inside menus and lists, and history that loads or
  scrolls into view do not animate. Rows animate once, when they arrive
  live, never again when they scroll back into view.
- **No bounce, no stagger.** Springs are damped so that they do not
  overshoot from rest, and no curve is elastic; items that arrive together
  appear together.
- **Reduced motion.** Settings → General → Reduce motion settles every
  transition at its end state on the next frame and holds looping
  indicators still. Every primitive in `motion.rs` honors it; new motion
  must use those primitives rather than reading time itself.

## Tokens

The tokens are constants in `app/src/ui/motion.rs` and live nowhere else.
Each names one kind of motion and holds its timing, so editing a token
there retunes every animation of that kind.

A token holds two timings, because the app moves in two ways. A one-shot
entrance runs for the token's duration on its curve; nothing interrupts
it. A reversible value is a spring, and the token's settle time is how long
that spring takes to come to rest from a standstill.

| Token | Entrance | Curve | Settle | Used for |
|---|---|---|---|---|
| `HOVER` | 120 ms | smoothstep | 180 ms | hover plates, controls that appear on hover |
| `REVEAL` | 140 ms | ease-out quint | 210 ms | menus, popovers, tooltips, page and pane changes |
| `TOGGLE` | 150 ms | ease-out cubic | 230 ms | switches, checkboxes, chevrons, icon swaps, status changes |
| `ENTER` | 180 ms | ease-out quint | 270 ms | live transcript rows, notices and cards above the composer, dialogs |
| `PANEL` | 200 ms | ease-out cubic | 320 ms | sidebar, composer expand and shrink |
| `resize(distance)` | 120–200 ms | ease-out cubic | 190–320 ms | text fields growing and shrinking with their text, by distance |

`DAMPING_RATIO` sets how every spring is damped. At 1 a spring is the
fastest that does not overshoot from rest. Below 1 it overshoots and comes
back; above 1 it settles more slowly.

The effort slider is the one control with physics of its own. It is dragged
by hand, so it follows the pointer on springs tuned for that and lets the
thumb overshoot a stop as it lands. Those tunings are constants at the top
of `app/src/ui/effort_slider.rs` and `app/src/ui/chat/effort.rs`, and they
stay out of the tokens on purpose: nothing else should move like it.

Press feedback is immediate (`.active()`), and focus rings appear at once.
Looping indicators (spinners, the waiting dots, the running ring) use the
shared 15 fps `ticker` clock and pause in background windows.

## Where motion applies

| Surface | In | Out |
|---|---|---|
| Hover on buttons, rows, menu items | background eases in, `HOVER` | eases back, `HOVER` |
| Menus, dropdowns, popovers | pointer: fade and 4 px slide away from the trigger, `REVEAL`; keyboard: at once | immediate |
| Tooltips | fade, `REVEAL`, after the existing delay | immediate |
| Dialogs, confirmations and the image preview | scrim fades, card fades and rises, `ENTER` | immediate |
| Screen change (Chat ↔ Settings) and Settings sections | new content fades in, `REVEAL` | immediate |
| Empty state ↔ conversation (first message, opening a task, New Task) | the composer changes place at once | at once |
| Switching tasks | transcript fades in when its snapshot lands, `REVEAL` | immediate |
| Live transcript rows at the end of the thread (sent message, reply, thinking, tool card, error, stop notice) | height grows from 0 and the row fades in, `ENTER`, so older rows glide up; scrolled up, a fade and 6 px rise | — |
| Sent message | the composer empties at once and the bubble's slot opens from the height it gave up, so the thread does not drop; the bubble rises in, the real row takes its exact place, and "Sending…" and the waiting dots share one slot under it | — |
| Tool status | spinner leaves at once, the outcome fades in, `TOGGLE` | — |
| Disclosed details (tool output, thinking) | fade in, `REVEAL` | immediate |
| Streaming text | grows in place, no animation | — |
| Waiting dots | fade in, `REVEAL` | immediate |
| Notices, queued messages, cards above the composer | fade and rise, `ENTER` | immediate |
| Files attached to the draft | fade and 6 px rise, `ENTER` | immediate |
| Composer and multi-line fields | height eases to the new line count, `resize(distance)` | same |
| Composer expand / shrink | height eases, `PANEL`; maximize and minimize icons cross-fade and scale, `TOGGLE` | same, reversed |
| A field a choice reveals (Buy credits → Custom amount) | box unfolds to the field's height and the field fades in, `PANEL` | folds the same way, reversed |
| Send ↔ Stop | icons cross-fade and scale, `TOGGLE` | same |
| Switches and checkboxes | knob slides, fill cross-fades, `TOGGLE` | same, reversed |
| Disclosure chevrons | rotate 90°, `TOGGLE` | same, reversed |
| Sidebar collapse | width eases, `PANEL` | same, reversed |
| Task status in the sidebar, project folders | indicators and folder icons cross-fade, `TOGGLE` | same |

Loading spinners for work that usually finishes quickly (opening a task)
appear only after 300 ms, so fast loads never flash a spinner.

## Implementation

- `Tween` is the one reversible value: a spring on gpui's integrator that
  starts from the value and speed on screen and settles at once under
  reduced motion. Views that own a transition (the sidebar, the composer)
  keep a `Tween` in their state, retarget that same `Tween` when the state
  changes again, sample it in `render`, and call
  `window.request_animation_frame()` only while it moves. Use
  `Tween::pixels` for a length, so it stops within a quarter pixel.
- Starting the app with `MAPLE_MOTION=eased` runs reversible values on their
  token's duration and curve instead of a spring, restarting the curve from
  the value on screen. It exists to compare the two in one build.
- `animated(id, target, ...)` keeps a `Tween` in element state for render code
  that owns no state (chevrons, switches, icon swaps). It never animates on
  mount.
- `.hover_bg(color)` gives a control a background that eases in under the
  pointer and back to the fill the control has at rest. It reads that fill
  from the control, so set it first with `.bg(..)` and leave it unset for a
  control with none; there is no second colour to keep in step. Every hover
  background in the app goes through it. Keep native `.hover()` for the
  cursor and text colour, and native `.active()` for press feedback.
  `motion::reveal(element, visible)` fades a control in and out in place.
- `unfold(id, open, content)` opens content in place: its box eases between
  nothing and the content's height while the content fades, so what follows
  glides, and it folds away the same way. Use it for in-flow content a
  choice shows or hides, not for menus or dialogs, which close at once.
- `fade_in`, `rise_in` and `slide_in` are one-shot entrances keyed by element
  id; a new id replays them. `swap_in(element, id, key)` fades content in
  whenever `key` changes but draws its first appearance at once, for
  content that replaces other content in one place (the window's screen, a
  Settings section). At the start of the fade only what is behind the
  content shows, so its parent must paint an opaque background; the
  window's own backdrop is black.
- Fading an ancestor of a cached view (`Entity::cached`, the sidebar) is
  safe: gpui paints a cached view afresh when the opacity it inherits
  changes. A cached view under a fade therefore re-renders on every frame
  of it, which a short fade can afford and a long one should avoid.
- Fades never block input. A fading screen, transcript or entering card
  takes clicks and keys from its first frame. Do not add a hit-blocking
  cover to hide a change.
- Menus open from their trigger: the panel's fade and slide are drawn inside
  the deferred draw, because opacity does not reach into `deferred()`.
  A menu the keyboard opens appears at once, and keyboard highlight moves
  inside a menu settle at once; pointer openings and hover ease.
- Transcript rows sit in a `gpui::list`, whose items lose element state when
  they scroll away. Their entrance is driven by the arrival time kept in view
  state, so a row animates once and never replays. A row that replaces the
  slot under the transcript (the waiting dots, a sent message, a failed
  send) opens from the slot's height, so nothing above it jumps. The sent
  message's row takes over from its bubble by the key the transcript files
  the row under (`timeline_view_key`), which for a real host's rows is not
  the id the event names; test hand-offs with rows that carry a live or
  stored identity.
- The message being sent is drawn with the sent message's own pieces
  (`attachment_row`, `attachment_thumbnail`, `attachment_chip` and the
  same bubble), so the row that takes over changes nothing inside the
  message: pictures stay above the text at the same size, and a message
  of pictures only has no bubble before or after. The row shows the
  draft's picture until the stored one loads, so a thumbnail never gives
  way to a file name. Anything new a sent message shows must be drawn the
  same way in the preview.
- Every animated frame re-renders the window. Keep animations short, request
  frames only while something moves, and never animate inside cached views
  without an element id that is stable across frames.
- Tests that assert settled layout opt into reduced motion
  (`cx.set_reduce_motion(true)`); motion tests sample deterministic time.
