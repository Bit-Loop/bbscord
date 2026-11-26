# bbscord TUI Specification (Wave 0, Round 2)

> Owner: `opus-tui` · Adversarial review: `neon-redteam` · Implementation target: `crates/bbs-tui` (Wave 1)
> Status: **Round 2 — pending red-team sign-off.** Every number below is normative unless marked *(informative)*.

Design thesis: **old-school BBS soul, btop polish, zero wasted cells.** One layout engine scales continuously from a
`16×2` HD44780 character LCD to a `640×180` 4K cockpit; one input model works on a 6-key keypad, a legacy VT, a
Zellij pane, and a kitty-keyboard terminal; one graphics pipeline degrades from kitty placeholders to text tags
without moving a single line of the scrollback.

---

## 0. Invariants (testable, enforced by property tests in `bbs-tui`)

| ID | Invariant |
|----|-----------|
| I1 | For every `(cols, rows)` in `1..=700 × 1..=200`, `layout()` never panics, never divides by zero, and every pane rect lies inside the frame. |
| I2 | Sum of pane widths + border/divider columns == `cols` exactly (no gaps, no overflow). |
| I3 | Ultra deck widths are **monotonically non-decreasing** as `cols` grows (widening never shrinks/kills an active Stage). |
| I4 | A 2-cell grapheme is never split by a pane edge, inline-field edge, modal border, or chunk boundary; an orphaned half is overwritten with `' '`. |
| I5 | Resize never mutates the `ScrollAnchor`; only user scroll, message delete, or message edit do. `640×180 → 16×2 → 640×180` returns to the identical anchor line. |
| I6 | No byte from remote content reaches the PTY unless it passed the sanitizer (§4.6). In `Ascii7Bit`, every emitted byte ∈ `0x20..=0x7D \ {0x5C}` plus our own CSI sequences. |
| I7 | Every action is reachable with only `↑ ↓ ← → Enter Esc` (Micro keypad), and every action has a binding that needs neither `CSI u` nor a Ctrl chord. |
| I8 | Image box geometry is tier-independent (except `TextTagOnly` = 1 row), so switching graphics tier never moves the anchor line. |
| I9 | PTY output never exceeds the byte-rate governor budget (§6.6) for more than one frame. |

---

## 1. Terminal capability model

### 1.1 Startup sequence (order is normative)
1. If stdout is not a TTY → refuse TUI (print hint for `--headless`).
2. Read env: `TERM`, `COLORTERM`, `TERM_PROGRAM`, `KITTY_WINDOW_ID`, `TMUX`, `ZELLIJ`, `STY`, `SSH_CONNECTION`/`SSH_TTY`, `NO_COLOR`, `LANG`/`LC_ALL`/`LC_CTYPE`.
3. Enter raw mode (clears `ICRNL IXON ISIG IEXTEN ICANON ECHO`).
4. **Probe** (§1.2) — skipped entirely when `TERM=dumb`, `TERM=linux`, charset forced to `Ascii7Bit`, or `--no-probe`.
5. `EnterAlternateScreen` (`CSI ?1049h`) → `CSI ?7l` (DECAWM off) → `CSI ?2004h` (bracketed paste) → `CSI ?25l` → mouse (`?1000h ?1006h`, only if enabled) → kitty keyboard push `CSI >1u` (only if probed).
6. Teardown is the exact reverse: pop kitty flags `CSI <u`, mouse off, `CSI ?2004l`, **`CSI ?7h` before** `CSI ?1049l`, cursor on, leave raw mode. Teardown runs from a panic hook and on `SIGTERM`/`SIGHUP` too.

DECAWM is a global (not per-buffer) mode in xterm, but ordering it inside the alt-screen bracket is safe on emulators that save/restore modes around `?1049`. It is **re-emitted** on `Ctrl+L` and after `SIGCONT`.

### 1.2 Probe (single batched write, hard-bounded)
Write in one `write(2)`:
```
ESC _Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA ESC \     kitty graphics query
CSI ?2026$p                                     DECRQM synchronized output
CSI ?2027$p                                     DECRQM grapheme-cluster mode
CSI ?u                                          kitty keyboard flags query
CSI 16t                                         cell size in px
CSI 14t                                         text area in px
CSI c                                           DA1 (fence; Ps 4 ⇒ sixel)
```
- Responses are read until the DA1 reply **or** the deadline: `150 ms` local, `300 ms` when `over_ssh`. Whatever has not answered by the fence/deadline is "unsupported". The deadline protects serial/LCD terminals that never answer DA1.
- Leftover late replies are swallowed by the input parser for `500 ms` after the probe (never typed into the composer).
- **Multiplexers:** under `$TMUX`/`$ZELLIJ`/`$STY` the multiplexer answers DA1/DECRQM itself, so the probe reports what the *mux* supports (correct semantics). APC kitty queries are dropped by tmux unless passthrough is on, so kitty is reported unsupported by default. Kitty-through-tmux is enabled only if (a) `tmux show -gv allow-passthrough` returns `on`/`all`, then (b) a DCS-wrapped (`ESC Ptmux; … ESC \`) kitty query is sent with its **own** `200 ms` window independent of the DA1 fence (tmux answers DA1 before the outer terminal answers the wrapped query).
- Pixel metrics precedence: `TIOCGWINSZ` (`ws_xpixel/ws_ypixel`, re-read on every `SIGWINCH`) → `CSI 16t` → `CSI 14t ÷ cells` → assume `8×16`. Any computed dimension is clamped `≥ 1`.

### 1.3 Charset tiers (core delta: add `VtSafe`)
| Tier | When | Frames | Meters/Sparks | Sigils |
|------|------|--------|---------------|--------|
| `Utf8Rich` | UTF-8 locale, not `TERM=linux` | `╔═╦═╗ ║ ╟─╫─╢ ╚═╩═╝` (or single/rounded/heavy) | Braille `⠀..⣿`, eighths `▁..█` | `¹²³⁴⁵⁶ ◉ ▶ ✓ ♪ ↩ ⟪⟫` |
| `VtSafe` | `TERM=linux` (256/512-glyph PSF) or `charset="vt"` | `┌─┬─┐ │ ├─┼─┤` + `═║╔╗╚╝` (present in Lat2/Terminus PSF) | `▌█` and `░▒▓` only | `[1]..[6] * > OK <<RLO>>` |
| `Ascii7Bit` | forced, `TERM=dumb`, non-UTF-8 locale, or Micro on LCD | `+ - = \|` | `[##--]`, spark levels ` ._-=#` | ASCII words only |

`Ascii7Bit` (HD44780 ROM A00): emit only `0x20..=0x7D` **excluding `0x5C`** (`\` renders `¥`; `0x7E` = `→`, `0x7F` = `←`). `lcd_rom = "a02"` unlocks `0x5C`/`0x7E`. Non-ASCII text → transliteration table (`é→e`, `ß→ss`, CJK → `?`), emoji → `:name:`, custom emoji `<:name:id>` → `:name:`. OSC 8, sync output and all graphics are never emitted in this tier.

### 1.4 Color depth
Precedence: `--color`/config → `NO_COLOR` (⇒ `Monochrome`) → `COLORTERM ∈ {truecolor, 24bit}` → `TERM` matches `*-direct|xterm-kitty|xterm-ghostty|foot*|wezterm|alacritty|contour` or `TERM_PROGRAM ∈ {WezTerm, iTerm.app, ghostty, vscode}` ⇒ `TrueColor` (COLORTERM is routinely stripped by `sudo`/`ssh`/tmux) → `*256color` ⇒ `Ansi256` → `linux|vt*|ansi|screen` ⇒ `Ansi16` → else `Ansi16`.
Downsampling: TC→256 = nearest by redmean distance over the 6×6×6 cube **and** the 24-step grey ramp; 256→16 never computed — each theme ships an explicit `[ansi16]` table; Monochrome = bold/underline/reverse + glyph focus markers.

### 1.5 Emoji / width policy
- If DECRQM 2027 reports supported (Ps 1/2/3), emit `CSI ?2027h` and measure by grapheme cluster (`unicode-segmentation` + `unicode-width`, emoji presentation = 2).
- Otherwise (default): strip `U+FE0F` (VS16) from text-default bases, strip Fitzpatrick modifiers `U+1F3FB..=U+1F3FF`, collapse ZWJ sequences to their first base emoji, keycaps `X U+FE0F U+20E3` → `[X]`, regional-indicator pairs kept (width 2 everywhere tested). Config: `emoji_policy = auto|mode2027|strip|shortcode`.

---

## 2. Geometry engine

### 2.1 Profiles & hysteresis
Raw classification is exactly `bbs_core::LayoutProfile::from_dimensions` (Micro `rows ≤ 4 || cols < 24`; Compact `cols < 60 || rows < 12`; Standard `cols < 140`; Wide `cols < 240`; Ultra). bbs-tui adds `classify(geom, prev: Option<LayoutProfile>)`:
- **Downgrade** (raw < prev): immediate — content must fit.
- **Upgrade** (raw > prev): only if every threshold crossed is cleared by `+2` cols / `+1` row (e.g. Standard→Wide at `cols ≥ 142`, Compact→Standard at `cols ≥ 62 && rows ≥ 13`, Micro→Compact at `cols ≥ 26 && rows ≥ 6`).
- Every solver formula below is valid for any width ≥ its profile minimum, so the hysteresis band (e.g. Standard at 141 cols: `S=28, C=110`) is always well-defined.

**Two axes.** The horizontal solver is chosen from `cols` alone (`H-profile`); the vertical chrome from `rows` alone (`V-profile`: `Micro ≤4`, `Short 5..=11`, `Tall ≥12`). The core `LayoutProfile` label is the combination; e.g. `200×8` = Compact label, but **Wide H-solver + Short V-chrome**.

### 2.2 Horizontal solver (C = chat pane inner width)
`outer = (V == Tall) ? 2 : 0` — Tall layouts have a 2-col outer frame; Short layouts are borderless. In both, adjacent panes are separated by exactly 1 divider column. All formulas below (and the Tall reference widths) use `outer`; with `outer = 0` every pane rect shifts left by 1 and `C` gains 2 (I2 holds at e.g. `80×8`, `200×8`, `240×8`, `512×8`).

| H-profile | Formula | Reference widths (Tall) `S / C / Mb / decks` |
|-----------|---------|-------------------------------------|
| Micro `<24` | single pager, `C = cols` | 16 · 20 |
| Compact `24..=59` | single pane, no side borders, `C = cols` | 24 · 40 · 59 |
| Standard `60..=139` | `S = clamp(round(.22c), 18, 28)`, `C = c − outer − 1 − S` | 60: 18/39 · 80: 18/59 · 100: 22/75 · 120: 26/91 · 139: 28/108 |
| Wide `140..=239` | `S = clamp(round(.15c), 24, 32)`, `Mb = clamp(round(.14c), 22, 30)`, `C = c − outer − 2 − S − Mb` | 140: 24/90/22 · 160: 24/110/22 · 200: 30/138/28 · 239: 32/173/30 |
| Ultra `≥240` | greedy deck fill, below | see table |

`round(x) = ⌊x + 0.5⌋`. All arithmetic in `i32`, results clamped `≥ 1` before casting to `u16` (I1).

**Ultra greedy fill** (S = 32, Mb = 30, `DECK_MIN = 56`, `DECK_CAP = 120`, `CHAT_CAP = 138`):
```
avail = cols − outer − 64            // outer + 32 + 1 + 30 + 1   (Tall: cols − 66)
C     = min(CHAT_CAP, avail − 57)    // always reserve deck 1 (56 + divider)
rem   = avail − C − 57; d = [56]
grow(d[0])                           // grow(x): g = min(rem, CAP − x); x += g; rem −= g
for deck in 2..=3: if rem ≥ 57 { rem −= 57; d.push(56); grow(last) }
if d.len() == 3 { distribute rem round-robin over d in priority order (d1 first) }   // all at cap
else            { C += rem }          // spill goes to the chat side-car, never to decks
```
Verified for every `cols ∈ 240..=700` (I2, I3) with a reference solver (ported verbatim as a `bbs-tui` unit test in Wave 1); reference points (Tall, `outer = 2`; Short shifts every threshold 2 cols lower):

| cols | S | C | Mb | d1 | d2 | d3 | note |
|-----:|--:|--:|---:|---:|---:|---:|------|
| 240 | 32 | 117 | 30 | 56 | – | – | chat below cap, Stage present |
| 261 | 32 | 138 | 30 | 56 | – | – | chat reaches cap |
| 269 | 32 | 138 | 30 | 64 | – | – | **inline screenshare unlocks (d1 ≥ 64)** |
| 325 | 32 | 138 | 30 | 120 | – | – | d1 at cap |
| 326..381 | 32 | 139..194 | 30 | 120 | – | – | spill → chat side-car |
| 382 | 32 | 138 | 30 | 120 | 56 | – | d2 appears (side-car absorbs the drop) |
| 446 | 32 | 138 | 30 | 120 | 120 | – | |
| 503 | 32 | 138 | 30 | 120 | 120 | 56 | d3 appears |
| **512** (dual-2K) | 32 | 138 | 30 | 120 | 120 | 65 | |
| 567 | 32 | 138 | 30 | 120 | 120 | 120 | all capped |
| **640** (4K) | 32 | 138 | 30 | 145 | 144 | 144 | round-robin surplus |

Deck priority: **in call** `[Stage (Voice/DAVE/Screenshare), Media/GIF Inspector, Thread/Pins]`; **idle** `[Thread/Pins, Media/GIF Inspector, Mentions inbox]`. Only the *content* of a slot changes on join/leave; slot widths never change because of call state. The only width discontinuity is the Wide→Ultra profile switch (239: C 173 → 240: C 117 + Stage 56), protected by hysteresis (upgrade at 242).

### 2.3 Chat measure
- Gutter mode (IRC style) iff `C ≥ 74` on upgrade / stays while `C ≥ 72` (hysteresis): `G = 21` = `HH:MM␠` (6) + nick right-aligned (12) + `␠│␠` (3). Else header mode: `nick · HH:MM` line, body indented 2, consecutive messages from the same author within 7 min grouped (no repeated header).
- Scrollbar: 1 col (rightmost of chat pane). Text measure `T = min(C − G − 1 − indent, 116)` with `indent = 2` in header mode, `0` in gutter mode.
- Spare `= C − G − 1 − indent − T` (≥ 0 by construction). **Side-car media column** when spare ≥ 25 (upgrade) / ≥ 23 (keep): 1 gap col + `spare − 1` cols, holding thumbnails/link cards of the messages beside them. Else spare is right padding. (Wide 239: `173 − 21 − 1 − 116 = 35` → gap 1 + side-car 34. Ultra 502: spare 56 → side-car 55.)

### 2.4 Vertical budget
**Tall (rows ≥ 12), bordered:**
```
row 0            top border, titles embedded: ╔═[¹guilds]═╦═[²#general · topic…]═══╦═[³members]═╗
rows 1..M        panes
row M+1          ╟─[↩ ali: "quoted…" │ bob, cy typing…]────────╢   (reply/edit target + typing: 0 extra rows)
Cmp rows         composer, Cmp ∈ [1, min(6, ⌊rows/4⌋)], grows with content
last row         bottom border, btop status embedded: ╚═[⣀⣤⣴⣾ 42ms]═[◉ lounge 3 DAVE✓]═[F1 help]═╝
M = rows − 3 − Cmp − B        B = banner rows (below), hard floor M ≥ 2
```
24 rows: `M = 20` at `Cmp = 1`. 12 rows: `Cmp ≤ 3`, `M ≥ 6` (≥ 4 with both banners).
Banners (each 1 row stolen from `M`, only while active, dropped lowest-priority-first if `M` would fall below 2): incoming **call ring** (priority 1), **attachment staging** `📎 2: a.png, b.txt  [x]remove` (priority 2). Sidebars span rows `1..rows−2`; the separator row only spans the chat column (`╟` on the divider, `╢` on the frame).

**Short (rows 5..=11), borderless:**
```
row 0            reverse-video header: pane segments aligned to pane columns, status at right
rows 1..M        panes separated by single │ dividers (no outer frame)
Cmp rows         composer, prefixed "↩ali>" when replying; Cmp ≤ max(1, ⌊(rows−1)/4⌋)
M = rows − 1 − Cmp
```
`24×5` → header 1 + M 3 + composer 1. Short-wide borderless math: `n` panes ⇒ `n − 1` dividers, so e.g. `200×8` Wide-H: `S = 30, Mb = 28, C = 200 − 2 − 30 − 28 = 140`. Pane numbers live in the header segments (`[1]guilds │ #general │ [3]members`); typing indicator goes in the header's right segment; call ring preempts the header row.

**Tall-narrow (cols 24..=59, rows ≥ 12):** Short chrome plus a `─` rule above the composer (the rule carries reply target + typing, like `╟─╢`): `M = rows − 2 − Cmp − B`, `Cmp ≤ min(6, ⌊(rows−1)/4⌋)`, hard floor `M ≥ 2` (banners dropped first). Nav is a full-width overlay drawer `min(cols, 32)` wide.

### 2.5 Modals & overlays
- Sheet mode when `cols < 40 || rows < 12`: borderless, full screen; row 0 = **title + query merged** (`Jump> gen_`), hint row only when `rows ≥ 7`. `24×5` ⇒ 4 list rows.
- Else boxed: `w = clamp(want_w, 24, cols − 4)`, `h = clamp(want_h, 5, rows − 2)`, centered (`⌊(cols − w)/2⌋`). Inner list rows `= h − 2 (border) − 1 (query) − (h ≥ 9 ? 1 hint : 0)`.
- On draw, any 2-cell glyph of the underlying buffer cut by the modal's left/right edge has its orphan half replaced by `' '` (I4).
- Overlays (autocomplete, which-key) anchor to the composer and flip above/below by available space; width `min(want, C)`.

---

## 3. Micro16x2 pager (`rows ≤ 4 || cols < 24`)

All 32 cells of a 16×2 are used: DECAWM is off, so writing `(15,1)` cannot scroll. Focus/selection is **glyph-based** (`>` marker), never SGR-only — LCD terminals have no reverse video. Graphics tier is forced to `TextTagOnly`. Six keys drive everything (`↑ ↓ ← → Enter Esc`); any full keyboard keys are accelerators only.

### 3.1 Row budget
| rows | layout |
|------|--------|
| 1 | content only; active flags as a ≤2-cell prefix |
| 2 | row 0 status, row 1 content (16×2) |
| 3 | status, content, hint/compose line |
| 4 | status, 2 content rows (40 cells/chunk on 20×4), hint/compose line |
| ≥5 (cols < 24, or a Micro hysteresis band such as `24×5`/`25×5`/`30×5` before the `cols ≥ 26 && rows ≥ 6` upgrade) | status, `rows − 2` content rows, hint/compose line |

Floors: `cols == 0 || rows == 0` → draw nothing. `cols < 8` → single glyph: `!` (mention pending), `1..9`/`+` (unread count), `.` (idle). No path panics (I1).

### 3.2 Status row (READ)
Segments, left to right: `author` · `␠` · `flags` (only active ones, 0..=3 cells: `@` mention elsewhere, `*` unread elsewhere, one of `M` muted / `D` deafened / `V` in voice) · `␠` (only if flags non-empty) · `n/m` right-aligned.
`author_w = cols − flags_w − (flags_w > 0) − 1 − chunk_w`. Degrade order when `author_w < 3`: drop `/m` → drop chunk indicator → drop flags separator. Example 16 cols, 125-chunk message, all flags: `ali␠@*M␠125/125` (3+1+3+1+7 = 15, author gets 4).

### 3.3 Content chunking
Word-wrap the rendered message (text + synthesized tails `[+2 img]`, `[emb:Title]`, `[voice 0:45]`, `+1x3 <3x2`) into chunks of `content_rows × cols` display cells. A 2-cell grapheme that would land on the last column moves to the next line (I4). `↑/↓` = previous/next chunk (crosses message boundaries), `←/→` = previous/next message. Auto-read (toggle from the Enter menu, or `Space`): dwell per chunk `= max(1.2 s, 60·words/wpm)`, default `wpm = 250`.

### 3.4 Modes (state machine)
```
READ ──Esc──▶ NAV ──Esc at root──▶ MAIN MENU ──Esc──▶ READ
READ ──Enter──▶ MSG MENU {New msg, Reply, React, Open media, Thread, GIF, Auto-read}
MAIN MENU = {Channels, Compose, Voice, GIF search, Search, Theme, Settings, Quit}
any ──incoming ring──▶ RING (preempts row 0 for ≤30 s; Enter=accept, ←=decline, timeout=ignore)
```
- **NAV**: row 0 breadcrumb `G:BitLoop 2/14`; row 1 item `>#general 12*`. `→` descend, `←` ascend, `Enter` open, `↑↓` move. Root list = `DMs` pseudo-guild (DMs + Group DMs by recency; unnamed GDM shown `&ali,bo,cy`) then guilds sorted unread-first. Any printable key starts a fuzzy filter (`/gen_` on row 0, best match on row 1, `↑↓` through matches).
- **COMPOSE**: row 0 `>ali␠␠␠␠142/2k` (target + counter); row 1 = h-scrolling input. Window rule: if the cursor reaches `cols − 1`, scroll so the cursor sits there (the last cell is reserved for the cursor at EOL); when `h_scroll > 0`, keep ≥ 4 cells of left context. `Enter` sends, `Esc` once = prompt `Esc=drop`, twice = discard.
- **VOICE**: row 0 `VC:lounge 3u M`; row 1 current speaker + ASCII/eighths VU `ali [####  ]`. `Enter` menu {Mute, Deafen, Volume ±, Verify DAVE, Leave}.
- **GIF**: row 0 `GIF:cat 3/25`, row 1 `[cat dancing]`; `←/→` cycle, `Enter` send, `↑` edit query.

---

## 4. Message rendering

### 4.1 Line anatomy
Gutter mode: `14:02 ␠␠␠␠␠bitloop │ text…`; continuation lines indent to the text column. Header mode as §2.3. Replies: `↩ ali: first 40 cells…` dim line above. Edited `(edited)` dim suffix. Mentions of self: `proc_misc` background stripe in the gutter.

### 4.2 Embeds
- Left bar `▌` in the embed's RGB color (downsampled; `|` in ASCII), 1 bar + 1 space. Embed inner width `W = min(T − 2, 100)`.
- Inline fields grid: `n = max(1, min(3, profile.max_inline_embed_cols(), inline_run, ⌊(W + 2)/18⌋))`, field width `fw = ⌊(W − 2(n − 1))/n⌋`, remainder columns to leftmost fields. `W ≤ 15` ⇒ `n = 1`, no division by zero.
- Link-card thumbnail right-aligned when `W ≥ 48`: `tw = min(⌊W/3⌋, 12)` cols, `th = max(1, round(tw · cw / ch))` rows (square in pixels; 12×6 at 8×16 cells). Fields/description beside the thumbnail use `W − tw − 1`; rows below the thumbnail reclaim full `W`.
- Image/video embeds → image box (§6) or `[▶ video title 3:12]` tag; provider/author/footer/timestamp dim lines; poll summary as a meter per option (`▕████░░░░▏ 42%`).

### 4.3 Attachments, reactions, voice messages
- Attachments: image boxes; non-images `📎 report.pdf 214 KB` (`[file] report.pdf 214K`).
- Reactions bar: `👍 3  ❤ 2  :kekw: 5`, wrapped to `T`; self-reacted in `selected_fg`.
- **Voice message**: Discord waveform (≤256 amplitude bytes, base64) resampled to `w = T.saturating_sub(14).min(48)` cells: `▶ ▁▂▅▇█▆▃▁… 0:12/0:45` (Braille 2-samples-per-cell variant in Utf8Rich; `▌█` in VtSafe). When `w < 4` the bar is omitted: `[▶ 0:12/0:45]` (`[> 0:12/0:45]` ASCII). `Enter`/`p` toggles playback via `bbs-voice`; progress recolors played cells.

### 4.4 Image box sizing (tier-independent)
`max_w = min(T, 48)` cols (Ultra/Wide side-car: side-car width), `max_h = min(16, ⌊M/2⌋)` rows. Box = largest cell rect preserving the source aspect using real cell px (`cw/ch`); all dims clamped `≥ 1` (fixes the `bounds.Dy() = 0` class of bug). Placeholder box drawn immediately with `[IMG name W×H]` centered, so nothing shifts on load. Only images within the viewport ± 1 screen are fetched (viewport culling); Discord media-proxy `?width=&height=` requests at box px size.

### 4.5 Width safety
All measurement in display cells over grapheme clusters. Tabs → spaces to next multiple of 4 relative to the text column. I4 is enforced in the shared `put_str_clipped()` used by every widget.

### 4.6 Sanitizer (all remote strings: content, names, embeds, filenames, URLs, topics, paste)
- Remove: C0 except `\n` (and `\t`, expanded), `DEL 0x7F`, C1 `U+0080..=U+009F`, `ESC`.
- Bidi controls rendered **visibly** instead of interpreted: overrides/embeddings `U+202A..=U+202E`, isolates `U+2066..=U+2069`, `U+061C`, shown as `⟪RLO⟫` / `<<RLO>>`. `U+200E/U+200F` dropped.
- **OSC 8 hyperlinks**: only `http://` / `https://`; percent-encode every byte `≤ 0x20`, `0x7F..=0x9F`, `;`, and non-ASCII; reject URLs containing `ESC`/`BEL`/`ST` after decoding; `id=` param is our own hash. Never emitted in `Ascii7Bit`; under tmux only if `terminal-features` includes `hyperlinks`.

---

## 5. Scroll anchoring (core delta)

```rust
pub enum AnchorPart {
    Header,
    Content { grapheme: u32 },            // grapheme index of the line's first cluster
    Embed { index: u16, grapheme: u32 },  // within flattened embed text
    Attachment { index: u16, permille: u16 }, // vertical position inside the image box
    Reactions,
    Footer,
}
pub struct ScrollAnchor { pub message_id: Snowflake, pub part: AnchorPart }
pub enum ScrollState { FollowTail, Anchored(ScrollAnchor) }   // existing enum kept
```
- The anchor is the **bottom-visible line** (chat grows upward). Reflow at any width finds the visual line containing `part` and places it on the bottom row; resize never writes the anchor (I5).
- `MESSAGE_DELETE` of the anchored message → nearest older surviving message's last line (else nearest newer); `MESSAGE_UPDATE` → clamp `grapheme`/`index` to the new lengths.
- Wrapped-line cache: `LRU<(message_id, edit_ts, width, charset, emoji_policy), Vec<LineSpan>>` capped at 4096 entries; reflow cost is O(visible lines) per resize.

---

## 6. Graphics pipeline

### 6.1 Tier selection
Config override → probe result → `TextTagOnly` for Micro / `Ascii7Bit` / `Monochrome`. Defaults per environment:
| Env | Default tier |
|-----|--------------|
| kitty | KittyPlaceholder |
| ghostty | KittyPlaceholder (no kitty animation frames assumed) |
| WezTerm, Konsole | KittyDirect (placeholders unsupported) |
| foot, mlterm, xterm (`-ti vt340`), Windows Terminal ≥1.22, DA1 Ps 4 | Sixel |
| iTerm2 (`TERM_PROGRAM=iTerm.app`) | ITerm2 |
| tmux/zellij/screen (no verified passthrough) | HalfBlock |
| Ansi16 | TextTagOnly (HalfBlock opt-in) |

### 6.2 Tier behavior
1. **KittyPlaceholder**: transmit once (`a=t`, `q=2`, zlib `o=z`), place virtually (`U=1`), draw `U+10EEEE` + row/col diacritics; image id in the fg color → those cells are exempt from theme fg overrides. Survives scrolling, clipping, tmux (with passthrough for the upload).
2. **KittyDirect**, 3. **Sixel**, 4. **ITerm2** (`OSC 1337;File=inline=1;width=Nc;height=Mr;preserveAspectRatio=1:` + base64 PNG): **scroll-proxy** — while the viewport moves, the box shows a HalfBlock proxy; the real image is (re)sent once the viewport has been still for 150 ms; placements scrolled out are deleted (Kitty `a=d,d=i,i=<id>`; sixel cells are simply overwritten). Cells covered by a sixel/iTerm2 image are marked `skip` in the ratatui buffer so text never overwrites them mid-frame.
5. **HalfBlock**: `▀` with fg = top px, bg = bottom px (1×2 px per cell); needs Ansi256/TrueColor.
6. **TextTagOnly**: `[IMG cat.png 1280×720 214K]` (1 row) + OSC 8 link where allowed.

### 6.3 Animation
Inline GIF/GIFV/APNG show frame 0 + `▶GIF` badge; only the focused one (or the Media Inspector's selection) animates. Caps per animation: `≤ 320×240` px, `≤ 60` frames, `≤ 16 MiB` RGBA (whichever is hit first); `≤ 12 fps`.
- **kitty** (detected via `TERM=xterm-kitty` / `KITTY_WINDOW_ID`): frames uploaded once via `a=f` and played with `a=a,s=3` → terminal animates locally, zero ongoing bytes.
- Everyone else (incl. ghostty): client-side frame replacement, `≤ 8 fps`, disabled when `over_ssh` unless opted in, always subject to the governor.

### 6.4 Terminal image memory
Client-side LRU of resident terminal images, budget `64 MiB` decoded RGBA. Eviction sends **`a=d,d=I,i=<id>`** (uppercase `I`: lowercase `d=i` deletes only placements and leaves the pixel data in terminal memory).

### 6.5 Screenshare / video
Default sink `PopoutWindow`. Inline (`InlineTerminal`) only in the Stage deck when **`d1 ≥ 64` cols && `deck_rows ≥ 12`** (⇒ ≥ 8 video rows + ≥ 4 participant/DAVE rows; never true in Short V-chrome), tier ≠ HalfBlock/Sixel unless opted in, and not `over_ssh` unless opted in. Box: `w = d1 − 2`, `h = min(⌈w · (fh/fw) · (cw/ch)⌉, deck_rows − 4)` using the **real stream aspect**; ≥ 4 deck rows always reserved for participants + DAVE status. Start fps 10; governor steps `10 → 5 → 2 → keyframe-only`, then downscale resolution by ½ per step.

### 6.6 PTY byte-rate governor
Every flush is accounted. Budget: `256 KiB/s` local, `48 KiB/s` over SSH (config). Priority when over budget (drop lowest first): video frames → GIF animation → VU meters (10 Hz → 4 Hz → 1 Hz) → sparklines → images re-send (deferred) → **text/UI never dropped**. Keyboard input is read before every frame so a saturated PTY never stalls typing.

---

## 7. btop polish

- **Frames**: `frame_style = double | single | rounded | heavy | ascii` (Utf8Rich), forced `vt`/`ascii` in lower tiers. Outer frame double, inner dividers `div_line`. btop-style superscript titles `╔═[¹guilds]═` → digit keys focus panes in NORMAL.
- **Gradients**: static, positional, **vertical** per pane border: `t = row/(rows−1)`, `color(t) = t < .5 ? lerp(s, m, 2t) : lerp(m, e, 2t − 1)` in OKLab, quantized to 16 steps ⇒ every horizontal border run is a single SGR and the left/right borders change color at most 16 times. No animated borders ever. Meters get horizontal 3-stop gradients (≤ 16 cells).
- **Braille**: cell = `U+2800 | bits`; bottom-up fill for a column of height `h ∈ 0..=4`: left column bits `[6, 2, 1, 0]`, right column bits `[7, 5, 4, 3]` (first `h` of each list). Sparkline: 8 cells = 16 samples. VU (horizontal): `−60..0 dBFS` mapped to `2w` half-columns, full `⣿` / half `⡇`; gradient stops at −18 dB and −6 dB. VtSafe: `▌█`; ASCII `[####----]`.
- **Latency**: gateway heartbeat ACK RTT arrives every ~41.25 s, so the status sparkline is labelled as history; voice-gateway heartbeat (~13.75 s) and REST RTT samples fill between ACKs. Gradient `temp_*` (good → bad).
- **Status segments** in the bottom border (dropped right-to-left when space runs out): latency spark+ms · voice (`◉ lounge 3 DAVE✓ M`) · unread/mention counts · hotkey hints (`hi_fg` letters).
- **BBS soul**: ANSI-art splash (CP437 `.ans` converted to UTF-8 at build time, ASCII variant for 7-bit), NORMAL-mode menu bar `[G]uilds [C]hannels [V]oice [T]heme [?]help [Q]uit` in the bottom border when `C ≥ 60`.

---

## 8. Themes

### 8.1 Switcher
`F2`, NORMAL `T`, or `Ctrl+T` (NORMAL only). Hovering a theme live-repaints the whole UI; `Enter` commits to config; `Esc` reverts. Rescans sources on open; `r` rescans while open.

### 8.2 Built-ins (bg / fg / accent `hi_fg` / border gradient start→mid→end)
| Theme | bg | fg | hi | gradient |
|-------|----|----|----|----------|
| ANSI-BBS Royal | `#0000AA` | `#AAAAAA` | `#55FFFF` (title `#FFFF55`) | `#5555FF → #55FFFF → #FFFFFF` |
| Amber Phosphor | `#0D0700` | `#FFB000` | `#FFD27F` | `#7A4A00 → #FFB000 → #FFE6A8` |
| P1 Green CRT | `#000A00` | `#33FF33` | `#B6FFB6` | `#0B5D0B → #33FF33 → #B6FFB6` |
| tty | indexed 0 | indexed 7 | indexed 14 | indexed 4 → 6 → 15 (no RGB: follows the terminal palette) |
| Gruvbox | `#282828` | `#EBDBB2` | `#FE8019` | `#B8BB26 → #FABD2F → #FB4934` |
| Tokyo Night | `#1A1B26` | `#C0CAF5` | `#7DCFFF` | `#7AA2F7 → #BB9AF7 → #F7768E` |
| Catppuccin Mocha | `#1E1E2E` | `#CDD6F4` | `#F5C2E7` | `#89B4FA → #CBA6F7 → #F38BA8` |
| Dracula | `#282A36` | `#F8F8F2` | `#FF79C6` | `#BD93F9 → #8BE9FD → #50FA7B` |
| Nord | `#2E3440` | `#D8DEE9` | `#88C0D0` | `#5E81AC → #88C0D0 → #8FBCBB` |
| Cyberpunk BBS | `#0A0014` | `#E0E0FF` | `#F3E600` | `#FF00A0 → #8000FF → #00F0FF` |

### 8.3 Native TOML (`~/.config/bbscord/themes/<name>.theme`)
```toml
[meta]
name = "Amber Phosphor"
[colors]           # semantic keys == btop key names, so both formats share one table
main_bg = "#0D0700"
main_fg = "#FFB000"
hi_fg   = "#FFD27F"
# … any btop key …
[gradients]        # 1–3 stops; omitted stops follow the btop empty rules
border  = ["#7A4A00", "#FFB000", "#FFE6A8"]
[ansi16]           # required for Ansi16 depth; indices 0..=15
main_bg = 0
main_fg = 3
```

### 8.4 btop importer
Sources: `~/.config/bbscord/themes/*.theme`, `$XDG_CONFIG_HOME/btop/themes`, `/usr/share/btop/themes`, `/usr/local/share/btop/themes` (shown as `btop/<name>`). Format sniff: first non-blank, non-`#` line matching `^theme\[` ⇒ btop parser, else TOML.
Line grammar: `^\s*theme\[([a-z_]+)\]\s*=\s*"([^"]*)"` — capture the **first quoted value**, ignore everything after the closing quote (host files `matcha-dark-sea.theme`, `monokai.theme` carry trailing `#…"` garbage; others carry trailing whitespace). Unknown keys ignored; `#` comment lines skipped.
Value grammar: `#RRGGBB` · `#GG` (grey `GG,GG,GG`) · `R G B` decimal (0–255) · `""`.
Empty rules (evaluated in this order, **btop-faithful**: btop's `generateGradients` only interpolates when `*_end` is defined, otherwise fills the whole gradient with `*_start`): `main_bg = ""` ⇒ `Color::Reset` (transparent terminal bg); gradient `end == ""` ⇒ flat `start` (mid ignored → `ColorGradient::solid(start)`); else `mid == ""` ⇒ 2-stop `start → end` (`ColorGradient::new_2stop`). None of the 41 host themes has `mid` set with `end` empty, so this only matters for hand-written files, but it must match btop (see §12 delta 6); missing optional keys: `graph_text → main_fg`, `meter_bg → inactive_fg`, `process_* → selected_bg` gradient, `followed_*`/`proc_banner_*`/`proc_pause_bg`/`proc_follow_bg` ignored.
`[ansi16]` for imported themes: auto-derived by nearest of the xterm default 16 (redmean).

### 8.5 Semantic mapping
| btop key | bbscord role |
|----------|--------------|
| main_bg / main_fg | base bg / text |
| title | pane titles |
| hi_fg | BBS hotkey letters `[G]`, unread markers (same role as btop's shortcut highlight) |
| selected_bg / selected_fg | selection, own reactions |
| inactive_fg | timestamps, read channels, dim lines |
| graph_text | sparkline labels |
| meter_bg | empty meter cells |
| proc_misc | mention stripe / mention badges |
| cpu_box / mem_box / net_box / proc_box | sidebar / chat / voice-Stage / members+decks borders |
| div_line | inner dividers, `╟─╢` |
| temp_* | latency gradient |
| cpu_* | border gradient |
| download_* / upload_* | remote speakers VU / mic VU |
| used_* | unread heat in guild list |
| free_* / cached_* / available_* | fallback nick palette (hashed) when roles have no color |
| process_* | selection gradient in lists |

---

## 9. Input

### 9.1 Legacy facts we rely on
Raw mode: Enter = `CR 0x0D`; **`Ctrl+J` = `LF 0x0A` is distinct** (ICRNL off) → newline; Backspace accepted as both `0x7F` and `0x08`; `Ctrl+S/Q/Z/C/O/Y/V` arrive as keys (IXON/ISIG/IEXTEN off). Never bound as distinct keys: `Ctrl+I`(=Tab) `Ctrl+M`(=CR) `Ctrl+[`(=Esc) `Ctrl+H`(=BS) `Ctrl+digit` `Ctrl+Shift+*` `Super`. `Shift+Tab` = `CSI Z` is legacy-safe. ESC disambiguation timeout: `50 ms` local, `100 ms` over SSH, `0` with kitty keyboard. (tmux users: `escape-time` ≤ 10.)

### 9.2 Router (strict precedence)
1. **Recovery**: `Ctrl+L` (full repaint, re-emit modes, re-place images), `Ctrl+Z` (suspend), `Ctrl+C` (cancel; double within 1 s on empty composer ⇒ quit prompt).
2. **Active modal**: consumes every key; `Esc` closes. Global shortcuts never fire behind a modal.
3. **INSERT (composer)**: readline set intact — `Ctrl+A E B F D H K U W Y T`, `Alt+B/F/D`, `Ctrl+X Ctrl+E` (edit in `$EDITOR`). App chords allowed in INSERT: `Ctrl+J`/`Alt+Enter` newline (`Shift+Enter` only with kitty keyboard flag 1), `Ctrl+G` GIF, `Ctrl+O` mute, `Ctrl+N/P` next/prev unread, `Ctrl+R` reply-to-last, `Ctrl+K` **DWIM** (below), F-keys, `Esc` → NORMAL, `Tab` autocomplete.
4. **NORMAL (BBS hotkeys)**: single letters, the guaranteed path (I7).

**Ctrl+K DWIM** (fought for, kept): Discord muscle memory says quick switcher; readline says kill-to-EOL. At **end of buffer** kill-to-EOL is a no-op, so `Ctrl+K` there (or with an empty composer, or in NORMAL) opens the switcher; anywhere else it kills to EOL. `composer.ctrl_k = "dwim" | "kill" | "switcher"`.

### 9.3 Keymap
| Action | NORMAL (guaranteed) | F-key | Ctrl accelerator | Micro path |
|--------|--------------------|-------|------------------|------------|
| Quick switcher | `k` | – | `Ctrl+K` (DWIM) | NAV |
| Compose / focus composer | `i`, `Enter` | – | – | MAIN→Compose |
| Reply / react / thread / pins | `r` / `e` / `t` / `p` | – | `Ctrl+R` | MSG MENU |
| Search | `/` | – | – | MAIN→Search |
| Theme switcher | `T` | `F2` | `Ctrl+T` (NORMAL only) | MAIN→Theme |
| GIF picker | `G` | `F5` | `Ctrl+G` | MAIN→GIF |
| Next/prev unread | `]` / `[` | – | `Ctrl+N` / `Ctrl+P` | NAV unread-first |
| Mute / deafen | `m` / `d` | `F7` / `F8` | `Ctrl+O` (mute) | VOICE menu |
| Join voice / call / hang up | `v` / `a` / `h` | `F9` (answer/hang) | – | VOICE / RING |
| Screenshare | `s` | – | – | VOICE menu |
| Members / voice deck | `M` / `V` | `F3` / `F4` | – | – |
| Focus pane | `1`..`6`, `Tab`/`Shift+Tab` | `F6` | – | – |
| Help / which-key | `?` | `F1` | – | MAIN |
| Quit | `q` (confirm) | `F10` | `Ctrl+Q` (NORMAL) | MAIN→Quit |

Zellij's default `Ctrl+T/G/O/N/P/S/Q` and tmux's `Ctrl+B` may swallow accelerators; every row has a NORMAL letter and most an F-key, so nobody is locked out. GNOME Terminal grabs `F10`/`F11` by default → `q` remains.

**Push-to-talk:** legacy terminals send no key-release events ⇒ toggle-to-talk only. True hold-PTT when kitty keyboard flag 2 (event types) is negotiated, or system-wide via the XDG GlobalShortcuts portal (`ashpd`).

### 9.4 Paste & suspend
- Bracketed paste always on: pasted text is inserted verbatim (newlines become composer newlines, never sends), sanitized (§4.6). Paste `> 2000` chars (or `> 4000` with Nitro) ⇒ prompt "send as `message.txt` attachment?". Terminals that ignore `?2004h` cannot be detected, so a fallback heuristic also applies: a single `read()` containing ≥ 2 CRs mixed with printable text ⇒ treated as paste (inserted, never sent).
- `Ctrl+Z`: run teardown, `raise(SIGTSTP)`; on `SIGCONT`: raw mode, alt screen, `?7l`, `?2004h`, mouse, kitty kbd flags, re-probe pixel size, full repaint, re-place/re-send images.

---

## 10. Voice, DAVE, calls, Group DMs

- **Stage deck / voice overlay** rows: header `◉ lounge · 3 · DAVE✓ · 34 ms`; per participant `◉ ali  ⣿⣿⣷⣄⠀ −12 dB  100%  M D ✓` (speaking dot in `download_*`/`upload_*` gradient, volume `0..=200%`). Selected participant: `-`/`+` ±10 %, `0` reset to 100 %, `x` local mute.
- **DAVE E2EE**: `DAVE✓` (verified), `DAVE~` (E2EE active, unverified), `DAVE!` (downgrade/transport-only — red, plus a 1-row banner). `v` opens the verification modal: voice privacy code in 5-digit groups and per-participant codes; mark verified locally. Epoch changes re-flag affected participants as `~`.
- **Calls** (1:1 & Group DM): ring banner (§2.4) `☎ ali is calling — [a]nswer [h]ang up`; `F9` answers/hangs up; Micro RING mode.
- **Group DMs**: in the DM list, `n` new GDM (multi-select friends, max 9 others = 10 members), on a GDM: `+` add recipient, `-` remove (owner only, greyed otherwise), `R` rename, `L` leave (confirm). Same actions via MSG/MAIN menus in Micro.
- **GIF picker**: modal (or the Media Inspector deck in Ultra/Wide). Tile `tw = 20`, `th = min(8, inner_h − 2)`; grid cols `= ⌊(inner_w + 1)/(tw + 1)⌋`. Auto-switches to a 1-row-per-result text list when `inner_h < 8 || inner_w < 20 || tier = TextTagOnly`. Only the selected tile animates.

---

## 11. Render loop & bandwidth

- Event-driven; idle = 0 fps. Scroll/resize ≤ 30 fps; VU 10 Hz only while visible; sparkline 1 Hz.
- On `Event::Resize`: drain every queued event (`poll(0)` loop), keep the last size, re-read `TIOCGWINSZ` pixels, render once. Image re-encode debounced 120 ms after the last resize; placeholder boxes meanwhile.
- Frames wrapped in synchronized output (`CSI ?2026h … CSI ?2026l`) when DECRQM reported support.
- ratatui diffing + SGR run coalescing. *(Informative)* Full repaint at `640×180` ≈ 115 k cells ≈ 200–300 KB (theme switch / `Ctrl+L` only); steady state in a call with 10 speakers at 10 Hz ≈ 20–40 KB/s, under the governor.
- `--low-bandwidth` (auto when `over_ssh`): VU 4 Hz, gradients off, Ansi256 cap, animations off, inline video off.

---

## 12. Core contract deltas (for `core-arch` after sign-off)
1. `CharsetMode`/`ResolvedCharset`: add `VtSafe`; rename `Utf8` → `Utf8Rich` (or keep `Utf8` meaning rich).
2. `ScrollAnchor { message_id, part: AnchorPart }` replaces `wrapped_line_offset` (§5).
3. `LayoutProfile::classify(geom, prev)` with hysteresis (§2.1); keep `from_dimensions` as the raw classifier.
4. `TerminalCapabilities`: add `grapheme_mode_2027: bool`, `kitty_animation: bool`, `tmux_passthrough: bool`, `cell_px: Option<(u16, u16)>`, `esc_timeout_ms: u16`.
5. `VideoSinkKind::InlineTerminal` gated by the Stage-deck rule (§6.5).
6. `theme.rs::parse_btop_theme` → `parse_gradient`: when `end` is `None`, drop `mid` (`let mid = if end.is_some() { mid } else { None };`) so a btop `mid`-set/`end`-empty gradient is flat `start` like btop, not `start → mid`. `ColorGradient::sample`'s `(Some(mid), None)` arm stays for API completeness but is unreachable from both importers (native TOML 2-element arrays map to `new_2stop`).
7. `[ansi16]` placement: add `ansi16: Option<Ansi16Palette>` to **`ThemePalette` in `bbs-core`** (`Ansi16Palette` = the same semantic keys with `u8` indices `0..=15`), so it is serialisable and shared by the TOML loader and the btop importer. When it is `None`, `bbs-tui` derives indices with `RgbColor::to_ansi16()`. The built-in `tty` theme sets only `ansi16` (RGB fields are placeholders), and `bbs-tui` emits SGR `30–37/90–97` for it at every depth, so it follows the terminal's own palette.

## 13. Test plan (Wave 1)
- Property tests: I1–I3 over the full grid; I4 with a CJK/emoji corpus at odd widths; I5 by scripted resize storms `640×180 → 16×2 → 24×5 → 200×8 → 640×180` asserting the anchor line text is identical; I6 by fuzzing the sanitizer with C0/C1/ESC/bidi/OSC payloads and asserting the byte set in Ascii7Bit.
- Golden snapshots (ratatui `TestBackend`) at `16×2, 20×4, 24×5, 40×12, 80×24, 139×40, 140×40, 200×8, 239×60, 240×60, 512×144, 640×180` × charset tiers.
- btop importer: parse all 41 host themes in CI fixture copies (no network), including the trailing-comment and empty-mid cases.
