# bbscord

**Old-school BBS soul, `btop` polish, zero wasted cells.**

`bbscord` is a production-ready, memory-safe Rust Discord terminal client built around a single continuous adaptive layout engine spanning **`16×2` HD44780 character LCDs** all the way to **`512×144` dual-2K and `640×180` 4K ultrawide cockpits**—complete with mandatory **DAVE MLS E2EE voice**, **Group DMs (`type = 3`) & DM calls**, **GStreamer WebRTC Go Live screensharing**, **animated GIF search**, **universal terminal graphics**, and **live `btop`-compatible themes**.

---

## Architecture

`bbscord` is organized as a modular 7-crate Rust workspace (`edition = "2024"`, `#![forbid(unsafe_code)]` across application, protocol, voice, video, and TUI crates):

```text
crates/
├── bbs-core          Domain models (Snowflake, GroupDm=3, Embeds, Voice/DAVE, Go Live),
│                     viewport/hysteresis contracts, btop theme parser, zeroized secrets,
│                     async service traits, and 100% offline deterministic mocks.
├── bbs-protocol      Chrome-fingerprinted TLS/HTTP2 REST v10 (wreq), Gateway v10 (zlib-stream),
│                     Group DM CRUD, DM/Group DM call signaling, QR Remote-Auth v2,
│                     signed CDN attachment URL refresh, and Discord/Klipy/Giphy GIF providers.
├── bbs-runtime-term  Hard-bounded terminal probe (DA1, DECRQM 2026/2027, Kitty, XTWINOPS),
│                     panic- & SIGTSTP/SIGCONT-safe TtyGuard, multi-tier graphics renderer
│                     (Kitty Placeholder/Direct, Sixel, iTerm2, HalfBlock, TextTag),
│                     100 ms SIGWINCH debouncer, and PTY byte-rate governor.
├── bbs-voice         Voice Gateway v8, UDP/RTP AEAD (XChaCha20/AES-GCM), mandatory DAVE MLS
│                     E2EE (davey), 48 kHz Opus capture/mixing (audiopus + cpal/PipeWire),
│                     VAD/PTT, per-user 0–200% gain, and Ogg/Opus voice-message playback.
├── bbs-video         Go Live Stream RTC signaling, Wayland/X11 XDG Desktop Portal + PipeWire
│                     capture (ashpd), hardware/software H.264/VP8/VP9 pipelines (GStreamer),
│                     DAVE video frame transforms, mpv pop-out & inline Stage terminal sinks.
├── bbs-tui           Pure Ratatui UI engine implementing docs/TUI_SPEC.md (I1–I9 invariants),
│                     2-axis H×V layout solver, content-addressed ScrollAnchor, pulldown-cmark +
│                     syntect Markdown, Braille VU/latency meters, and 10 built-in + btop themes.
└── bbscord           Integrated CLI binary, config/secrets loader, QR v2 login screen,
                      --mock-demo offline showcase, and --headless-smoke E2E verifier.
```

Full normative UI physics, cell budgets, and invariants (`I1`–`I9`) are documented in [docs/TUI_SPEC.md](docs/TUI_SPEC.md).

---

## Continuous Dynamic Layouts (`16×2` LCD → 4K Cockpit)

The geometry solver evaluates horizontal (`HProfile`) and vertical (`VProfile`) axes independently with asymmetric hysteresis (`+2` cols / `+1` row on upgrade; immediate downgrade) and content-addressed `ScrollAnchor` pinning (`640×180 → 16×2 → 640×180` returns to the exact bottom-visible line):

| Viewport | Profile | Layout & Ergonomics |
| :--- | :--- | :--- |
| **`16×2` / `20×4`** (`rows ≤ 4 \|\| cols < 24`) | `Micro16x2` | 6-key hardware pager (`↑ ↓ ← → Enter Esc`), `DECAWM` off (`CSI ?7l`) so all 32 cells are usable, glyph focus (`>`), `MAIN MENU` / `MSG MENU`, HD44780 ROM A00 7-bit ASCII (`0x20..=0x7D \ {0x5C}`) & optional `--lcd-a02`. |
| **`24×5` – `59×24`** (`cols < 60 \|\| rows < 12`) | `Compact` / `Short` | Borderless single-pane / short-wide multi-pane (`n − 1` dividers), reverse-video header, full-screen sheet modals (`4` list rows even on `24×5`). |
| **`80×24` – `139×40`** | `StandardBbs` | Classic CP437/ANSI-BBS double-line frame (`╔═╦═╗` / `╟─╢`), embedded superscript titles, 2-column inline embed grids, Braille latency & voice status bar. |
| **`140×40` – `239×60`** | `Wide` | 3-pane desktop (`Sidebar` + `Chat` + `Members`), 3-column inline embed grids, link-card thumbnails, automatic side-car media column when spare width `≥ 25` cols. |
| **`240×60` – `512×144` (Dual-2K) / `640×180` (4K)** | `UltraWideCockpit` | Clamped sidebars (`32`/`30`), capped readable chat measure (`138` cols), greedy monotonic multi-deck right column (up to 3 simultaneous decks: **Voice/DAVE/Screenshare Stage**, **Media/GIF Inspector**, **Thread/Pins**). |

---

## Themes & `btop` Compatibility

Press **`F2`**, **`T`** (NORMAL), or **`Ctrl+T`** (NORMAL) to open the live-repainting theme switcher (`Enter` persists to `~/.config/bbscord/config.toml`, `Esc` reverts):

- **10 Built-in Presets:** `ANSI-BBS Royal`, `Amber Phosphor`, `P1 Green CRT`, `tty` (strict SGR 16-color indexed palette following your terminal theme), `Gruvbox`, `Tokyo Night`, `Catppuccin Mocha`, `Dracula`, `Nord`, `Cyberpunk BBS`.
- **Direct `btop` `.theme` Import:** Automatically discovers `~/.config/bbscord/themes/*.theme`, `$XDG_CONFIG_HOME/btop/themes/*.theme`, `/usr/share/btop/themes/*.theme`, and `/usr/local/share/btop/themes/*.theme`. Supports `#RRGGBB`, `#GG` grayscale shorthands, `"R G B"` decimals, transparent `main_bg=""`, trailing inline comments, and native TOML `[meta]/[colors]/[gradients]/[ansi16]`.
- **OKLab 3-Stop Gradients & Braille Telemetry:** 16-step quantized vertical border gradients, Braille (`⠀..⣿`) voice VU meters (`-60..0 dBFS`) and Gateway latency sparklines, with `VtSafe` (`TERM=linux`) and 7-bit ASCII (`[####----]`) fallbacks.

---

## Voice, DAVE E2EE, Group DMs & Go Live Screensharing

- **Mandatory DAVE MLS E2EE:** Full `davey` MLS handshake, pairwise 45-digit SAS verification codes (`v` modal), truthful `DAVE✓` (all verified) / `DAVE~` (encrypted, unverified) / `DAVE!` (red downgrade banner) status indicators, and epoch-change verification reset.
- **Guild Voice, Stage & DM/Group DM Calls:** Join guild voice channels or ring/answer 1:1 DM and Group DM (`type = 3`) calls (`a` answer / `h` hang up / `F9`), adjust per-participant gain (`0..=200%` via `-`/`+`/`0`), toggle VAD/PTT and 80 Hz high-pass filtering, and play Ogg/Opus voice messages inline with waveform visualization.
- **Group DM Management:** Create (`n`), add recipients (`+`), remove recipients (`-`), rename (`R`), and leave (`L`) Group DMs directly from the DM deck.
- **Go Live WebRTC Screensharing:**
  - **Watch:** Defaults to a hardware-accelerated borderless **`mpv` pop-out window** (`autovideosink` fallback); press `W` in the Stage deck to toggle **inline terminal playback** (gated to Stage width `≥ 64` cols and height `≥ 12` rows with `≥ 4` rows reserved for participants & DAVE status).
  - **Broadcast:** Uses Wayland/X11 XDG Desktop Portal (`ashpd`) + PipeWire (`pipewiresrc`) with dynamic hardware/software encoding (`vah264enc`/`nvh264enc` → `x264enc` → `vp8enc`), DAVE frame encryption, and RTCP PLI/FIR keyframe recovery.

---

## Universal Terminal & Graphics Support

| Environment | Default Graphics Tier | Charset / Color | Notes |
| :--- | :--- | :--- | :--- |
| **Kitty** | `KittyPlaceholder` (`U+10EEEE`) | `Utf8` / `TrueColor` | Native `a=f`/`a=a` animation, `a=d,d=I` VRAM eviction (`64 MiB` LRU). |
| **Ghostty / Rio** | `KittyPlaceholder` | `Utf8` / `TrueColor` | Client-side paced GIF frames (`≤ 8 fps`), Mode 2026 sync output. |
| **WezTerm / Konsole** | `KittyDirect` | `Utf8` / `TrueColor` | Scroll-proxy HalfBlock during scroll; re-sends after `150 ms` settle. |
| **foot / xterm (`vt340`) / Windows Terminal** | `Sixel` | `Utf8` / `TrueColor` | Governed PTY byte budget (`256 KiB/s` local, `48 KiB/s` SSH). |
| **iTerm2** | `ITerm2` (`OSC 1337`) | `Utf8` / `TrueColor` | Base64 inline PNG with aspect preservation. |
| **`tmux` / `zellij` / `screen`** | `HalfBlock` (or `--tmux-passthrough`) | `Utf8` / `TrueColor` | DCS passthrough opt-in; zero Ctrl-chord lockouts in Zellij. |
| **Linux Console (`TERM=linux`)** | `TextTagOnly` | `VtSafe` / `Ansi16` | PSF-safe box drawing & block meters; skips graphics probes. |
| **Serial / `16×2` HD44780 LCD / `TERM=dumb`** | `TextTagOnly` | `Ascii7Bit` / `Monochrome` | Strict `0x20..=0x7D \ {0x5C}` byte set; `150 ms` hard probe cap. |

---

## Keybinds

`bbscord` uses a strict 4-tier input router (**Recovery** → **Active Modal** → **INSERT Readline** → **NORMAL BBS**). Every action has a guaranteed single-letter NORMAL key (never blocked by `zellij`/`tmux` or missing `CSI u`) and a 6-key Micro keypad path:

| Action | NORMAL (Universal) | F-Key | Ctrl Accelerator |
| :--- | :--- | :--- | :--- |
| Quick switcher | `k` | — | `Ctrl+K` (DWIM: switcher at EOL/empty, kill-to-EOL mid-line) |
| Focus composer / newline | `i`, `Enter` | — | `Ctrl+J` / `Alt+Enter` (newline in INSERT), `Ctrl+X Ctrl+E` (`$EDITOR`) |
| Reply / React / Thread / Pins | `r` / `e` / `t` / `p` | — | `Ctrl+R` (reply) |
| Theme switcher (live preview) | `T` | `F2` | `Ctrl+T` (NORMAL) |
| Animated GIF picker | `G` | `F5` | `Ctrl+G` |
| Next / Prev unread | `]` / `[` | — | `Ctrl+N` / `Ctrl+P` |
| Voice join / Call answer / Hang up | `v` / `a` / `h` | `F9` | — |
| Mute / Deafen / Screenshare | `m` / `d` / `s` | `F7` / `F8` | `Ctrl+O` (mute) |
| Focus pane / Members / Voice deck | `1`–`6`, `Tab` / `M` / `V` | `F6` / `F3` / `F4` | — |
| Copy message / Open URL | `Alt+Y` / `Alt+O` | — | `wl-copy` / `xclip` / OSC 52 & `xdg-open` |
| Redraw / Suspend / Help / Quit | `Ctrl+L` / `Ctrl+Z` / `?` / `q` | `F1` / `F10` | `Ctrl+Q` (NORMAL) |

---

## Quickstart

### Build from Source

Install native audio/video dev headers (Arch: `pacman -S opus alsa-lib gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad`; Ubuntu/Debian: `apt install libopus-dev libasound2-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev pkg-config cmake libclang-dev`), then build:

```bash
cargo build --release -p bbscord
```

### Offline Demo & Verification (Zero Account / Zero Network)

```bash
# Interactive showcase with guilds, Group DM calls, DAVE E2EE, inline embeds, animated GIFs & SMPTE video
./target/release/bbscord --mock-demo

# Test 16x2 HD44780 LCD layout in your terminal
./target/release/bbscord --mock-demo --geometry 16x2 --charset ascii

# Run 10-frame headless E2E smoke test across 16x2, 80x24, and 512x144
./target/release/bbscord --headless-smoke
```

### Live Mode & Authentication

```bash
# First run opens the QR Remote-Auth v2 / Masked Token login screen and stores credentials in your OS keyring
./target/release/bbscord

# Opt-in read-only RAM import of an existing discordo OS keyring entry (never written to disk)
./target/release/bbscord --import-discordo-keyring
```

### Configuration & Optional GIF API Keys

- **`~/.config/bbscord/config.toml`**: Persists theme choice, layout preferences, voice VAD/PTT/gain settings, and `prefer_inline_video` (never stores tokens).
- **`~/.config/bbscord/secrets.toml`** *(optional, must be `chmod 600`)*: Optional `klipy_api_key` / `giphy_api_key` fallback keys if you want external GIF catalogs in addition to Discord's first-party `/gifs` endpoint.

---

## Security Model

- All authentication tokens are wrapped in `SecretToken(Zeroizing<String>)` with redacted `Debug`, no `Display`, and no `Serialize`.
- OS credentials are stored exclusively in the native OS keyring (`service="bbscord", user="token"`); `--import-discordo-keyring` performs a read-only lookup into RAM only.
- All remote Discord strings, display names, URLs, and pasted text pass through a C0/C1/DEL/bidi-isolate/OSC-8 sanitizer before reaching the PTY.
- Repository commits and pushes are protected by `scripts/secret-scan.sh` (`.git/hooks/pre-commit` & `.git/hooks/pre-push`) and CI full-history scanning.

## License

Dual-licensed under **MIT** ([LICENSE](LICENSE)) or **Apache-2.0**, at your option.
