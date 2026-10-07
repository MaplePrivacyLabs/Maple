# Maple Theme Spec for the gpui Agent Chat UI

This spec gives the exact values that the gpui rewrite must use for the agent
chat UI. All values come from the Maple frontend sources:

- `frontend/tailwind.config.cjs` (token names, radius scale)
- `frontend/src/index.css` (HSL tokens for light and dark)
- `frontend/src/chat.css` (chat typography and markdown sizes)
- `frontend/src/components/UnifiedChat.tsx`, `chat/ChatTurn.tsx`,
  `AgentMode.tsx`, `Sidebar.tsx` (class names that bind tokens to elements)

Maple defines the tokens as HSL. This document converts them to hex. Where
the web app draws a translucent color over the page, the "composited" column
gives the flat hex to paint in gpui (dark theme, over `#0A0A0A`).

The dark theme is the primary target. The light theme values are listed in
the same tables.

## 1. Neutral scale

Both themes use one neutral scale.

| Token | Hex |
|---|---|
| neutral-50 | `#FAFAFA` |
| neutral-100 | `#F5F5F5` |
| neutral-200 | `#E5E5E5` |
| neutral-300 | `#D4D4D4` |
| neutral-400 | `#A3A3A3` |
| neutral-500 | `#737373` |
| neutral-600 | `#525252` |
| neutral-700 | `#404040` |
| neutral-800 | `#262626` |
| neutral-900 | `#171717` |
| neutral-950 | `#0A0A0A` |

## 2. Background colors

| Surface | Dark | Light | Source |
|---|---|---|---|
| App background (`--background`) | `#0A0A0A` | `#FAFAFA` | `bg-background` on the chat column and composer area |
| Card / popover (`--card`) | `#171717` | `#FFFFFF` | |
| Muted (`--muted`) | `#262626` | `#F5F5F5` | |
| Sidebar (`--sidebar`) | `#262626` | `#F5F5F5` | `Sidebar.tsx`: `bg-muted dark:bg-[hsl(var(--sidebar))]` |
| Sidebar chrome (`--sidebar-chrome`) | `#404040` | `#FFFFFF` | header and footer strips in the sidebar |
| Sidebar chrome hover | `#525252` | `#F5F5F5` | |
| Sidebar row hover | `#404040` | `#FAFAFA` | |
| Sidebar row selected | `#525252` | `#E5E5E5` | |
| Sidebar row selected + hover | `#737373` | `#D4D4D4` | |
| Sidebar scrollbar thumb | `#525252` | `#A6A6A6` | hover: `#737373` / `#999999` |
| Composer background | `#0A0A0A` | `#FAFAFA` | `bg-background` inside a bordered `rounded-3xl` container |
| User message bubble | `#171717` (`--card`) | `#F5F5F5` (`--muted`) | `ChatTurn.tsx`: `bg-muted dark:bg-card` |
| Assistant message | transparent | transparent | Assistant turns have no bubble. Text sits on the app background. |
| Tool card (`bg-muted/20`) | `#101010` (composited) | `#F8F8F8` (composited) | `rounded-3xl border` collapsible card |
| Inline tool row (`bg-muted/30`) | `#121212` | `#F7F7F7` | web search / small status rows |
| Attachment chip (`bg-muted/50`) | `#181818` | `#F7F7F7` | |
| Tool card, error state (`bg-destructive/5`) | `#140E0D` | `#F8ECE9` | |
| Input requested row (`maple-primary/0.06`) | `#191210` | `#FAF4F1` | the MCP elicitation row |
| Maple surface (`--maple-surface`) | `#171717` | `#FAFAFA` | |
| Maple surface dim | `#262626` | `#BABCCB` | |

## 3. Text colors

| Role | Dark | Light | Source |
|---|---|---|---|
| Primary (`--foreground`) | `#FAFAFA` | `#262626` | message text, tool titles |
| Secondary (`text-foreground/80`) | `#CACACA` (composited) | `#515151` | tool output body |
| Muted (`--muted-foreground`) | `#A3A3A3` | `#737373` | timestamps, status labels, spinner |
| Placeholder (`muted-foreground/60`) | `#666666` | `#A8A8A8` | composer placeholder |
| Link (`foreground/0.72`) | `#B7B7B7` | `#616161` | markdown links, no underline until hover |
| Typing dots (`bg-foreground/60`) | `#9A9A9A` | `#7A7A7A` | three 8 px dots, pulse animation |
| Product label ("Maple") | `#FAFAFA` | `#262626` | 14 px, weight 600 |
| On sidebar chrome | `#FAFAFA` | `#262626` | |
| Primary button text (`--primary-foreground`) | `#0A0A0A` | `#FAFAFA` | on `--primary` fill `#FAFAFA` / `#171717` |

## 4. Accent colors

| Token | Dark | Light | Use |
|---|---|---|---|
| Maple primary (coral) | `#FF9771` | `#FF9771` | send button, focus ring, caret, link accents, permission shield icon, web search icon |
| Maple primary strong | `#E77040` | `#E77040` | bottom stop of the send button gradient |
| On primary | `#0A0A0A` | `#FFFFFF` | icon inside the send button |
| Primary container | `#40261C` | `#FFE8E0` | hover fill on composer icon buttons |
| Ring (`--ring`) | `#FF9771` | `#FF9771` | keyboard focus outline |
| Maple secondary (pebble) | `#6E6F81` | `#8A8B9A` | |
| Maple secondary 700 | `#9F9FAD` | `#5A5B6A` | composer icon buttons (attach, mic) |
| Maple secondary container | `#2F2F37` | `#E8E8ED` | composer border at rest |
| Maple tertiary (bark) | `#8C645A` | `#9E7469` | |
| Blue | `#3FDBFF` | `#3FDBFF` | "new" dot ring in sidebar |
| Purple | `#9469F8` | `#9469F8` | |
| Bitcoin | `#F7931A` | `#F7931A` | |

Send button: 32 x 32 px circle. Vertical gradient from `#FF9771` (top) to
`#E77040` (bottom). Icon color is On primary at 90 % opacity.

## 5. Border colors

| Border | Dark | Light | Source |
|---|---|---|---|
| Default (`--border`) | `#262626` | `#E5E5E5` | user bubble border, dividers |
| Input (`--input`) | `#262626` | `#E5E5E5` | |
| Composer at rest | `#2F2F37` | `#E8E8ED` | `border-[hsl(var(--maple-secondary-container))]` |
| Composer focused | `#FF9771` | `#FF9771` | `focus-within:border-[hsl(var(--maple-primary))]` |
| Tool card (`border-muted/40`) | `#151515` (composited) | `#F8F8F8` | almost invisible; the card reads as a soft fill |
| Tool card error (`border-destructive/35`) | `#4F271D` | `#EFC9BF` | |
| Input requested row (`maple-primary/0.45`) | `#784A38` | `#FDC8B8` | |
| Sidebar right edge (`border-border/20`) | `#262626` | `#EEEEEE` | 1 px |
| Markdown blockquote left bar | `#262626` (4 px, 0.25 em) | `#E5E5E5` | |

Border width is 1 px everywhere unless stated.

## 6. Radius scale

`--radius` is `0.5rem` (8 px).

| Name | Px | Use |
|---|---|---|
| sm | 4 | |
| md | 6 | |
| lg | 8 | |
| xl | 12 | small icon buttons, image thumbnails, code blocks |
| 2xl | 16 | user bubble, inline tool row, attachment chip |
| 3xl | 24 | composer, tool card |
| full | 9999 | send button, typing dots |

## 7. Spacing scale used in the chat

Tailwind unit = 4 px.

| Element | Value |
|---|---|
| Transcript column max width | 896 px (`max-w-4xl`), centered |
| Transcript padding | 16 px (24 px on desktop) |
| User turn vertical padding | 16 px top and bottom |
| User turn, stacked (consecutive user turns) | 4 px top, 0 bottom |
| User bubble max width | min(100 %, 672 px) |
| User bubble padding | 16 px horizontal, 12 px vertical |
| Assistant turn vertical padding | 16 px (desktop: 16 px all sides) |
| Assistant turn: gap between avatar column and content | 12 px (desktop), 8 px (narrow) |
| Assistant avatar | 32 x 32 px |
| Assistant content: gap between blocks | 8 px |
| Gap between turns | 0 extra; the turn padding gives 32 px between two message bodies |
| Tool card padding | 16 px horizontal, 12 px vertical |
| Tool card bottom margin | 8 px |
| Tool card: gap icon to title | 8 px |
| Tool card body indent | 24 px left, 8 px top |
| Inline tool row padding | 12 px horizontal, 8 px vertical |
| Input requested row padding | 16 px horizontal, 12 px vertical |
| Composer outer padding | 16 px horizontal, inside the 896 px column |
| Composer text area | 16 px left, 32 px right padding |
| Composer bottom toolbar | icon buttons 32 x 32 px (36 px on desktop), 8 px gap |
| Action row (copy etc.) gap | 4 px |
| Code block padding | 16 px top, 16 px sides, 8 px bottom |
| Markdown paragraph bottom margin | 10 px |
| Markdown heading top margin | 24 px, bottom 16 px |

## 8. Typography

Font family: **Manrope**, sans-serif fallback. Code: `ui-monospace`,
SF Mono, Menlo, Consolas.

| Text | Size | Line height | Weight |
|---|---|---|---|
| App body (chrome) | 14 px | normal | 400 |
| Chat message text (`--chat-font-size`) | 15 px | 1.65 (~25 px) | 400 |
| Chat message letter spacing | 0.1 px | | |
| Chat "sm" tier (tool cards, status) | 14 px | 20 px | 400 |
| Tool title | 13 px | 20 px | 500 |
| Tool meta / status label | 11 px | 20 px | 400 |
| Chat "xs" tier | 12 px | 16 px | 400 |
| Product label ("Maple") | 14 px | 1 | 600 |
| Composer text | 16 px | 24 px | 400 |
| Markdown h1 | 2 em | | 600 |
| Markdown h2 | 1.5 em | | 600 |
| Markdown h3 | 1.25 em | | 600 |
| Markdown h4 | 1 em | | 600 |
| Bold | | | 600 |
| Code block | 85 % (~12.75 px) | 1.45 | 400 |
| Inline code | 85 % | | 400 |
| Input requested row title | 14 px | 20 px | 500 |
| Input requested row detail | 12 px | 16 px | 400, muted |

## 9. Status colors

| State | Dark | Light | Where |
|---|---|---|---|
| Running tool | spinner in muted `#A3A3A3` / `#737373`; title text in muted | | `Loader2` 16 px, spin |
| Success | `#87A253` | `#7B8F4A` | check icon 16 px (`text-maple-success`) |
| Error (`--destructive`, both themes) | `#D05E41` | `#D05E41` | X icon, status label, card border/fill (see sections 2 and 5) |
| Error (`--maple-error`) | `#CC5233` | `#D05E41` | inline search rows, inline error text |
| Warning / incomplete | `#CE994B` | `#D4A35A` | X icon and label for interrupted tools; 6 px dot in "waiting" pill |
| Info | `#6C7E93` | `#7E8DA1` | |
| Input requested row (pending) | icon `#FF9771`; border `#784A38`; fill `#191210` | icon `#FF9771`; border `#FDC8B8`; fill `#FAF4F1` | `ShieldCheck` 16 px. Resolved state returns to the plain tool card |
| Typing indicator | three 8 px dots `#9A9A9A`, pulse with 0 / 75 / 150 ms delay | `#7A7A7A` | |

## 10. Syntax highlighting (dark)

| Token | HSL | Hex |
|---|---|---|
| comment | 225 27% 43% | `#505F8B` |
| keyword | 199 95% 74% | `#7ED4FC` |
| string | 93 75% 67% | `#B0EA6C` |
| number | 22 100% 69% | `#FFA361` |
| attribute | 270 75% 78% | `#CAA1F1` |
| selector tag | 158 64% 65% | `#6CDEB1` |
| addition | 211 93% 75% | `#84BEFB` |
| deletion | 349 86% 72% | `#F57A93` |
| punctuation | 225 27% 70% | `#9EA9C7` |

## 11. Notes for the gpui implementation

- The web app paints many surfaces with alpha. gpui can paint the composited
  hex values given above instead. This avoids blend order errors.
- The user bubble in dark mode is `#171717` on `#0A0A0A` with a `#262626`
  border. The contrast is low by design.
- The composer border is the only element that changes color on focus
  (`#2F2F37` to `#FF9771`). Buttons show focus with a 2 px `#FF9771` ring.
- The coral `#FF9771` is the same in both themes. Only the "on primary" text
  changes (`#0A0A0A` dark, `#FFFFFF` light).
