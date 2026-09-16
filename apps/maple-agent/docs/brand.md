# Maple brand guide and its gpui mapping

The visual values in `app/src/ui/theme.rs`, the bundled fonts, and the
shared widgets follow the Maple brand kit, the Figma file "Maple Brand"
(https://www.figma.com/design/kY49eTc1Pa2yVEQK9jFeal/Maple-Brand). The
first part of this document records the kit itself so the app has one
in-tree reference; the second part says how each value lands in gpui.

`docs/maple-theme-spec.md` records the earlier Tauri web app measurements
that the first gpui theme was built from. It is kept for layout numbers
(column widths, paddings) that the brand kit does not define; where the two
disagree on colour, radius, or type, the brand kit wins.

## Part 1: the brand kit

### Colour scales

Five scales, ten steps each (Neutral has twelve). The 500 step is the
"brand" value of each scale.

**Maple (primary), "primary brand energy"**

| 50 | 100 | 200 | 300 | 400 | 500 | 600 | 700 | 800 | 900 |
|---|---|---|---|---|---|---|---|---|---|
| `#fff4f0` | `#ffe8e0` | `#ffd1c1` | `#ffbaa2` | `#ffa88a` | **`#ff9771`** | `#f67d57` | `#e8633d` | `#d04926` | `#a83515` |

**Pebble (secondary), "ethereal balance"**

| 50 | 100 | 200 | 300 | 400 | 500 | 600 | 700 | 800 | 900 |
|---|---|---|---|---|---|---|---|---|---|
| `#f7f7f9` | `#e8e8ed` | `#d1d2dc` | `#babccb` | `#9c9dab` | **`#8a8b9a`** | `#757689` | `#5e5f6e` | `#474854` | `#30313a` |

**Bark (tertiary), "grounded structure"**

| 50 | 100 | 200 | 300 | 400 | 500 | 600 | 700 | 800 | 900 |
|---|---|---|---|---|---|---|---|---|---|
| `#f8f5f4` | `#eaded9` | `#d4bcaf` | `#c29a8d` | `#b0877c` | **`#9e7469`** | `#8a6055` | `#704d43` | `#583a32` | `#3d2821` |

**Grove (tertiary), "organic calming"**

| 50 | 100 | 200 | 300 | 400 | 500 | 600 | 700 | 800 | 900 |
|---|---|---|---|---|---|---|---|---|---|
| `#f7f6f0` | `#e8e4d4` | `#d3ccb0` | `#beb48c` | `#aea375` | **`#9e925e`** | `#8a7f4c` | `#726b3c` | `#5a542d` | `#3f3b1f` |

**Neutral, "focus and clarity"**

| 0 | 50 | 100 | 200 | 300 | 400 | 500 | 600 | 700 | 800 | 900 | 950 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `#ffffff` | `#fafafa` | `#f5f5f5` | `#e5e5e5` | `#d4d4d4` | `#a3a3a3` | `#737373` | `#525252` | `#404040` | `#262626` | `#171717` | `#0a0a0a` |

### Brand balance: 60 / 30 / 10

> We follow a balanced approach to color distribution to ensure our
> interface remains clean and functional while maintaining brand character.

| Share | Role | Palette | Guidance |
|---|---|---|---|
| 60 % | Core atmosphere | Neutrals and surfaces | Large backgrounds and workspace areas use the neutral palette to provide a clean canvas for content. |
| 30 % | Brand identity | Pebble and Bark | Navigation, sidebars, and secondary UI elements establish the Maple AI environment. |
| 10 % | Action and focus | Maple primary | Reserved for key actions, focus states, and moments of high importance. Use sparingly to maintain impact. |

### Semantic roles

| Role | Value | Scale ref |
|---|---|---|
| Primary | `#ff9771` | Maple 500 |
| On primary | `#f7f7f9` | Pebble 50 (the kit's swatch shows white text on the coral tile) |
| Primary container | `#ffe8e0` | Maple 100 |
| Secondary | `#8a8b9a` | Pebble 500 |
| On secondary | `#f7f7f9` | |
| Secondary container | `#e8e8ed` | Pebble 100 |
| Tertiary | `#9e7469` | Bark 500 |
| On tertiary | `#f7f7f9` | |
| Tertiary container | `#eaded9` | Bark 100 |
| Success | `#7b8f4a` | |
| Warning | `#d4a35a` | |
| Error | `#d05e41` | |
| Info | `#7e8da1` | |

Surfaces in the kit: white page, `#fafafa` cards, `#474854` (Pebble 800)
dark tiles for the logo, hairlines at 10 % black. Body text is Neutral
900, secondary text Neutral 600, muted text Neutral 400. Display and
headline samples are set in Pebble 800; the kit's own section headings
are Pebble 300.

### Type

| Role | Family | Weights |
|---|---|---|
| Display | Array (Fontshare, Indian Type Foundry) | Regular |
| Body | Manrope | Regular 400, Medium 500, SemiBold 600, Bold 700 |
| Code | Geist Mono | Regular 400, Medium 500 |

| Style | Family | Size / line height | Weight |
|---|---|---|---|
| Display large | Array | 86.4 / 78 px | 400 |
| Display medium | Array | 76.8 / 73.6 px | 400 |
| Display small | Array | 64 / 68 px | 400 |
| Headline large | Array | 48 / 56 px | 400 |
| Title large | Manrope | 24 / 32 px | 600 |
| Title medium | Manrope | 20 / 28 px | 500 |
| Body large | Manrope | 18 / 28 px | 400 |
| Body medium | Manrope | 16 / 24 px | 400 |
| Label large | Manrope | 14 / 20 px | 500 |
| Mono medium | Geist Mono | 16 / 24 px | 400 |
| Eyebrow | Geist Mono | 12 / 16 px, tracking 1.2 px, uppercase | 400 |

### Spacing and radius

| Space | Value | Usage |
|---|---|---|
| xs | 8 px | |
| sm | 12 px | Tight component spacing |
| md | 20 px | Button groups, form elements |
| lg | 36 px | Navigation items spacing |
| xl | 56 px | Section gaps, hero spacing |
| 2xl | 88 px | |

| Radius | Value | Kit name |
|---|---|---|
| sm | 8 px | SM |
| md | 12 px | MD |
| lg | 16 px | LG |
| xl | 24 px | XL |
| card | 44 px | Card |
| full | 999 px | Full (pills) |

### Buttons: the "glassy button system"

Pill shape, translucent fill `linear-gradient(180deg,
rgba(238,226,222,0.5), rgba(238,226,222,0.25))`, label in Manrope Medium
`#4d4e52`.

| Size | Font | Padding (y × x) | Height | Use |
|---|---|---|---|---|
| Small | 14 px | 8 × 20 | 32 px | Contextual actions |
| Medium | 16 px | 8 × 20 | 36 px | Standard operations |
| Large | 18 px | 8 × 28 | 44 px | Primary focal points |

> Our buttons utilize a physical interaction model. Hovering increases
> scale and brightness to invite engagement, while the active state
> compresses the element, providing tactile digital feedback.

Hover: scale 1.05, shadow `0 4px 6px rgba(0,0,0,.1), 0 2px 4px
rgba(0,0,0,.1)`. Active: scale 0.95. Disabled: 40 % opacity. Transition
200 ms ease-out.

### Marks

- Wordmark "MAPLE" (247 × 48) and abbreviated "MPL" (100 × 32), white
  `#f7f7f9` on Pebble 800.
- Avatars: circle with a radial gradient `#ff9771` → `#ce9a8e` (50 %) →
  `#9c9dab` at 90 % opacity, white glyph centred; sizes 120, 80, 56, 40,
  32 px (the 32 px size uses the single "M" chevron).
- Favicon 16 px (radius 4) and icon 32 px (radius 8) are a white "M" on
  Pebble 800; the 64 px app icon (radius 14) uses the avatar gradient.

## Part 2: how the app applies it

### Colour tokens

The light palette is the kit's; the dark palette is not in the kit. It
keeps the coral and status hues, uses Pebble steps for text and chrome so
the app reads as the same brand, and tints the coral container towards
the dark background.

| Token | Light | Dark | Kit source |
|---|---|---|---|
| `bg_app` | `#ffffff` | `#18181c` | page white; dark well is lifted off near-black so type does not bloom |
| `bg_sidebar` | `#f7f7f9` Pebble 50 | `#1e1e24` | Pebble for sidebars |
| `bg_elevated`, `bg_user_bubble`, `bg_sidebar_card` | `#ffffff` / `#e8e8ed` | `#27272e` | cards sit a step above the well |
| `bg_tool_card`, `bg_sidebar_chrome` | `#fafafa` / `#ffffff` | `#1e1e24` | |
| `bg_input` | `#ffffff` | `#1c1c22` | distinct from the well |
| `bg_code_block` | `#f7f7f9` | `#141416` | recessed vs the well |
| `bg_sidebar_pill`, row hover | `#e8e8ed` Pebble 100 | `#32323a` | |
| row selected | `#d1d2dc` Pebble 200 | `#3a3a44` | |
| `border` | `#e8e8ed` Pebble 100 | `#3a3a44` | hairline |
| `border_subtle` | `#f1f1f4` | `#2c2c34` | |
| `text_primary` | `#171717` Neutral 900 | `#d1d2dc` Pebble 200 | body; dark is grey, not Pebble 50, ~12:1 on the well |
| `text_heading` | `#171717` | `#e8e8ed` Pebble 100 | markdown headings, one step brighter than body |
| `text_secondary` | `#525252` Neutral 600 | `#babccb` Pebble 300 | |
| `text_muted` | `#a3a3a3` Neutral 400 | `#9c9dab` Pebble 400 | AA on well and elevated |
| `accent` | `#ff9771` Maple 500 | same | primary |
| `accent_hover` | `#f67d57` Maple 600 | `#ffa88a` Maple 400 | |
| `on_accent` | `#f7f7f9` | same | on primary (stays bright so coral stays readable) |
| `accent_container`, `permission_fill` | `#ffe8e0` Maple 100 | `#3a2118` / `#2a1a14` | primary container |
| `link` | `#9e7469` Bark 500 | `#c29a8d` Bark 300 | tertiary |
| `status_success` | `#7b8f4a` | `#8fa35a` | success |
| `status_warning` | `#d4a35a` | same | warning |
| `status_error` | `#d05e41` | `#e07052` | error |
| `display_text` | `#474854` Pebble 800 | `#babccb` Pebble 300 | Array titles; large type can sit dimmer than body |

The 60 / 30 / 10 rule maps to: workspace and transcript on `bg_app`;
sidebar, settings navigation, popups, and hover fills on Pebble; coral only
on the send button, the composer border, focus rings, the new-task row,
selected menu items, and primary buttons.

### Type

| Role | Family | Constant |
|---|---|---|
| Display headings (settings pane titles) | Array Regular | `assets::FONT_DISPLAY` |
| Empty-state hero | Array Regular | `assets::FONT_DISPLAY` |
| Chrome (sidebar, buttons, settings) | Manrope 14 px | `assets::FONT_BODY` |
| Chat and composer (default) | System UI, 14 px, line-height 1.65 | `assets::FONT_SYSTEM` |
| Chat option: Maple | Manrope | `assets::FONT_BODY` |
| Chat option: Geist | Geist Sans (static Regular–Bold) | `assets::FONT_GEIST` |
| Chat option: Serif | Georgia | `assets::FONT_SERIF` |
| Code, diffs, tool output, keycaps | Geist Mono (static Regular / Medium / SemiBold) | `assets::FONT_MONO` |

Fonts are bundled in `app/assets/fonts` and registered at startup, so
code and optional chat faces render the same on every platform. Array is
licensed under the ITF Free Font License, which allows embedding in the
app but not redistributing the font file on its own, and forbids
renaming; the Wide cut therefore cannot be selected (gpui has no stretch
and the file's family name is `Array`). Manrope and Geist are OFL. The
system and serif faces are not bundled. gpui/font-kit does not apply
variable `wght` axes, so Geist and Geist Mono ship as static instances.

Chat family and size are user settings (`chat_font_family`,
`chat_font_size`, 13–18 px, default 14). Retired `"sf-pro"` values map to
System; `.SystemUIFont` already is SF Pro on macOS. Markdown emphasis and
headings use SemiBold (600), not Bold (700). Heading sizes are ems of the
live chat size, at 1.25 line-height. The hero heading is 36 / 48 px in
Array Regular; settings section titles are 26 / 32 px in Array Regular.

### Shape

| Constant | Value | Used for |
|---|---|---|
| `theme::RADIUS_SM` | 8 px | icon buttons, menu rows, chips, badges |
| `theme::RADIUS_MD` | 12 px | popups, inputs, message bubbles, small cards |
| `theme::RADIUS_LG` | 16 px | cards, image frames |
| `theme::RADIUS_XL` | 24 px | composer, dialogs, the login card |
| `rounded_full` | pill | every text button, avatars, toggles |

The kit's 44 px card radius is not used; the app's cards are too small
for it.

### Buttons

`ui::widgets` provides `primary_button` (coral pill, `on_accent` label),
`secondary_button` (Pebble 100 pill), and `ghost_button` (text only, hover
overlay). All share 8 px vertical and 20 px horizontal padding
(`theme::SPACE_MD`, the kit's medium button) and a Manrope Medium label.

The coral fill replaces the kit's translucent "glass" gradient, which is
designed to sit on a Pebble 300 panel and washes out on the app's white
and near-black surfaces. The hover and press scale motion is not applied:
gpui has no transition primitive, and a stepped scale change on hover
reads as a flicker.

### Spacing

gpui's 4 px helpers cover the kit's 8 and 12 px steps. `theme::SPACE_MD`
(20 px) is the pill padding. The composer toolbar icons take
`accent_container` on hover, the kit's primary container.
