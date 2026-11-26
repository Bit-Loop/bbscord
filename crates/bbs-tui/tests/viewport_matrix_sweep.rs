//! Viewport matrix sweep: renders the populated mock world across the full geometry range
//! (`1×1` → `700×220`), all charset tiers, modals, and randomized SIGWINCH walks, asserting the
//! TUI_SPEC invariants I1–I9 and writing inspectable frame dumps to
//! `$CARGO_TARGET_TMPDIR/frame-dumps/`.

mod common;

use bbs_core::{ResolvedCharset, ScrollState, UiAction, mock_ids};
use bbs_tui::{Effect, geometry};
use common::{check_a00, check_wide_cells, demo_app, render, write_dump};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const MATRIX: &[(u16, u16)] = &[
    (1, 1),
    (8, 2),
    (16, 2),
    (20, 4),
    (24, 5),
    (48, 14),
    (80, 8),
    (80, 24),
    (160, 50),
    (269, 45),
    (512, 144),
    (640, 180),
    (700, 220),
];

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

fn actions(effects: &[Effect]) -> Vec<&UiAction> {
    effects
        .iter()
        .filter_map(|e| {
            if let Effect::Action(a) = e {
                Some(a)
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn matrix_all_tiers_render_cleanly() {
    for tier in [
        ResolvedCharset::Utf8,
        ResolvedCharset::VtSafe,
        ResolvedCharset::Ascii7Bit,
    ] {
        let mut app = demo_app(tier, true);
        for &(c, r) in MATRIX {
            let buf = render(&mut app, c, r);
            check_wide_cells(&buf).unwrap_or_else(|e| panic!("{tier:?} {c}x{r}: {e}"));
            if tier == ResolvedCharset::Ascii7Bit {
                check_a00(&buf).unwrap_or_else(|e| panic!("{c}x{r}: {e}"));
            }
            let l = app.layout().expect("layout");
            if !l.micro {
                let sum: u16 = l.panes.iter().map(|p| p.rect.width).sum::<u16>()
                    + u16::try_from(l.dividers.len()).unwrap_or(0)
                    + l.outer;
                assert_eq!(sum, c, "I2 at {c}x{r}");
            }
            write_dump(&format!("{tier:?}-{c}x{r}"), &buf);
        }
    }
}

#[test]
fn micro_16x2_reads_and_uses_every_cell() {
    let mut app = demo_app(ResolvedCharset::Ascii7Bit, false);
    let buf = render(&mut app, 16, 2);
    let dump = common::dump(&buf);
    let lines: Vec<&str> = dump.lines().collect();
    assert_eq!(lines.len(), 2);
    // Status row ends with the chunk counter n/m.
    assert!(
        lines[0]
            .trim_end()
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_digit()),
        "{dump}"
    );
    assert!(
        lines[1].trim().len() > 4,
        "content row must carry text: {dump}"
    );
    write_dump("micro-16x2-read", &buf);
}

#[test]
fn micro_six_keys_reach_compose_voice_gif_quit() {
    let mut app = demo_app(ResolvedCharset::Ascii7Bit, true);
    let _ = render(&mut app, 16, 2);
    // Compose & send via READ → Enter (MSG MENU) → "New msg".
    let _ = app.handle_key(key(KeyCode::Enter));
    let _ = app.handle_key(key(KeyCode::Enter));
    for c in "hi".chars() {
        let _ = app.handle_key(ch(c));
    }
    let buf = render(&mut app, 16, 2);
    write_dump("micro-16x2-compose", &buf);
    let fx = app.handle_key(key(KeyCode::Enter));
    assert!(
        actions(&fx)
            .iter()
            .any(|a| matches!(a, UiAction::SendMessage { content, .. } if content == "hi")),
        "{fx:?}"
    );
    // Esc → NAV, Esc → MAIN MENU, Down×2 → Voice.
    let _ = app.handle_key(key(KeyCode::Esc));
    let _ = app.handle_key(key(KeyCode::Esc));
    let buf = render(&mut app, 16, 2);
    assert!(
        common::dump(&buf).contains("MAIN MENU"),
        "{}",
        common::dump(&buf)
    );
    let _ = app.handle_key(key(KeyCode::Down));
    let _ = app.handle_key(key(KeyCode::Down));
    let _ = app.handle_key(key(KeyCode::Enter));
    let buf = render(&mut app, 16, 2);
    write_dump("micro-16x2-voice", &buf);
    assert!(
        common::dump(&buf).starts_with("VC:"),
        "{}",
        common::dump(&buf)
    );
    // Voice actions: Enter → Mute.
    let _ = app.handle_key(key(KeyCode::Enter));
    let fx = app.handle_key(key(KeyCode::Enter));
    assert!(
        actions(&fx)
            .iter()
            .any(|a| matches!(a, UiAction::SetSelfMute(true))),
        "{fx:?}"
    );
    // Back to READ, MAIN MENU → GIF search → send first trending result.
    let _ = app.handle_key(key(KeyCode::Esc));
    let _ = app.handle_key(key(KeyCode::Esc));
    let _ = app.handle_key(key(KeyCode::Esc));
    for _ in 0..3 {
        let _ = app.handle_key(key(KeyCode::Down));
    }
    let fx = app.handle_key(key(KeyCode::Enter));
    let req = actions(&fx)
        .iter()
        .find_map(|a| {
            if let UiAction::SearchGifs { request_id, .. } = a {
                Some(*request_id)
            } else {
                None
            }
        })
        .expect("gif search");
    let page = common::mock_events(false)
        .into_iter()
        .find_map(|e| {
            if let bbs_core::BackendEvent::GifSearchResults { page, .. } = e {
                Some(page)
            } else {
                None
            }
        })
        .expect("gifs");
    let _ = app.handle_event(bbs_core::BackendEvent::GifSearchResults {
        request_id: req,
        page,
    });
    let buf = render(&mut app, 16, 2);
    write_dump("micro-16x2-gif", &buf);
    let fx = app.handle_key(key(KeyCode::Enter));
    assert!(
        actions(&fx)
            .iter()
            .any(|a| matches!(a, UiAction::SendGif { .. })),
        "{fx:?}"
    );
    // Quit is reachable: Esc → NAV → MAIN, Up to top then Down×7 → Quit → Enter → y via Enter.
    let _ = app.handle_key(key(KeyCode::Esc));
    let _ = app.handle_key(key(KeyCode::Esc));
    for _ in 0..8 {
        let _ = app.handle_key(key(KeyCode::Down));
    }
    let _ = app.handle_key(key(KeyCode::Enter));
    let fx = app.handle_key(key(KeyCode::Enter));
    assert!(fx.iter().any(|e| matches!(e, Effect::Quit)), "{fx:?}");
}

#[test]
fn modal_sheet_on_24x5_has_four_list_rows() {
    let mut app = demo_app(ResolvedCharset::Utf8, false);
    let _ = render(&mut app, 24, 5);
    let _ = app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    let buf = render(&mut app, 24, 5);
    write_dump("Utf8-24x5-switcher", &buf);
    let d = common::dump(&buf);
    let lines: Vec<&str> = d.lines().collect();
    assert!(lines[0].starts_with("Jump>"), "{d}");
    assert_eq!(
        lines
            .iter()
            .skip(1)
            .filter(|l| !l.trim().is_empty())
            .count(),
        4,
        "{d}"
    );
}

#[test]
fn i5_resize_storm_never_moves_anchor() {
    let mut app = demo_app(ResolvedCharset::Utf8, false);
    let _ = render(&mut app, 640, 180);
    let _ = render(&mut app, 120, 30);
    app.scroll_lines(-3);
    let anchored = app.scroll_state();
    assert!(matches!(anchored, ScrollState::Anchored(_)), "{anchored:?}");
    let first = render(&mut app, 120, 30);
    for (c, r) in [
        (640, 180),
        (16, 2),
        (24, 5),
        (200, 8),
        (1, 1),
        (700, 220),
        (80, 24),
    ] {
        let _ = render(&mut app, c, r);
        assert_eq!(app.scroll_state(), anchored, "anchor mutated at {c}x{r}");
    }
    let last = render(&mut app, 120, 30);
    assert_eq!(
        common::dump(&first),
        common::dump(&last),
        "round trip must restore the identical frame"
    );
}

#[test]
fn randomized_sigwinch_walk() {
    let mut app = demo_app(ResolvedCharset::Utf8, true);
    let _ = render(&mut app, 100, 30);
    app.scroll_lines(-2);
    let anchored = app.scroll_state();
    let mut seed: u64 = 0x5EED_BB5C;
    for step in 0..400 {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let c = u16::try_from((seed >> 33) % 700).unwrap_or(1) + 1;
        let r = u16::try_from((seed >> 17) % 220).unwrap_or(1) + 1;
        let buf = render(&mut app, c, r);
        check_wide_cells(&buf).unwrap_or_else(|e| panic!("step {step} {c}x{r}: {e}"));
        assert_eq!(app.scroll_state(), anchored, "step {step} {c}x{r}");
    }
}

#[test]
fn ultra_decks_monotone_and_stage_inline_gate() {
    let mut app = demo_app(ResolvedCharset::Utf8, true);
    let mut prev: Vec<u16> = Vec::new();
    for c in (240..=700).step_by(7) {
        let _ = render(&mut app, c, 60);
        let l = app.layout().expect("layout");
        let d: Vec<u16> = l.decks().iter().map(|r| r.width).collect();
        for (a, b) in prev.iter().zip(&d) {
            assert!(b >= a, "I3 at {c}: {prev:?} -> {d:?}");
        }
        prev = d;
    }
    let _ = render(&mut app, 512, 144);
    let l = app.layout().expect("layout");
    assert_eq!(
        l.decks().iter().map(|r| r.width).collect::<Vec<_>>(),
        vec![120, 120, 65]
    );
    assert_eq!(l.h, geometry::HProfile::Ultra);
}

#[test]
fn themes_live_switch_and_revert() {
    let mut app = demo_app(ResolvedCharset::Utf8, false);
    let _ = render(&mut app, 80, 24);
    let before = app.theme().name().to_string();
    let _ = app.handle_key(key(KeyCode::F(2)));
    let _ = app.handle_key(key(KeyCode::Down));
    let preview = app.theme().name().to_string();
    let buf = render(&mut app, 80, 24);
    write_dump("Utf8-80x24-theme-picker", &buf);
    let _ = app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.theme().name(), before);
    let _ = app.handle_key(key(KeyCode::F(2)));
    let _ = app.handle_key(key(KeyCode::Down));
    let _ = app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.theme().name(), preview);
}

#[test]
fn insert_mode_readline_and_ctrl_k_dwim() {
    let mut app = demo_app(ResolvedCharset::Utf8, false);
    let _ = render(&mut app, 80, 24);
    let _ = app.handle_key(ch('i'));
    for c in "hello world".chars() {
        let _ = app.handle_key(ch(c));
    }
    // Ctrl+A then Ctrl+K kills (not at end).
    let _ = app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    let _ = app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert_eq!(app.composer.text(), "");
    // Ctrl+K on empty composer opens the switcher.
    let _ = app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    let buf = render(&mut app, 80, 24);
    assert!(
        common::dump(&buf).contains("Jump>"),
        "{}",
        common::dump(&buf)
    );
    let _ = app.handle_key(key(KeyCode::Esc));
    // Ctrl+J newline never sends; Enter sends.
    for c in "a".chars() {
        let _ = app.handle_key(ch(c));
    }
    let fx = app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
    assert!(actions(&fx).is_empty());
    let _ = app.handle_key(ch('b'));
    let fx = app.handle_key(key(KeyCode::Enter));
    assert!(
        actions(&fx)
            .iter()
            .any(|a| matches!(a, UiAction::SendMessage { content, .. } if content == "a\nb"))
    );
    // Paste with newlines inserts, never sends.
    let fx = app.handle_paste("line1\nline2\x1b[201~");
    assert!(actions(&fx).is_empty());
    assert!(!app.composer.text().contains('\x1b'));
}

#[test]
fn group_dm_and_voice_actions() {
    let mut app = demo_app(ResolvedCharset::Utf8, true);
    app.select_channel(mock_ids::CH_GROUP_DM_OPS);
    let _ = app.tick(1_764_000_001_000);
    let _ = render(&mut app, 160, 50);
    let _ = app.handle_key(ch('R'));
    for c in "ops".chars() {
        let _ = app.handle_key(ch(c));
    }
    let fx = app.handle_key(key(KeyCode::Enter));
    assert!(
        actions(&fx)
            .iter()
            .any(|a| matches!(a, UiAction::RenameGroupDm { name, .. } if name == "ops")),
        "{fx:?}"
    );
    let _ = app.handle_key(ch('L'));
    let fx = app.handle_key(ch('y'));
    assert!(
        actions(&fx)
            .iter()
            .any(|a| matches!(a, UiAction::LeaveGroupDm { .. }))
    );
    // Voice overlay volume control.
    let _ = app.handle_key(ch('V'));
    let buf = render(&mut app, 160, 50);
    write_dump("Utf8-160x50-voice-overlay", &buf);
    let fx = app.handle_key(ch('+'));
    assert!(
        actions(&fx)
            .iter()
            .any(|a| matches!(a, UiAction::SetParticipantVolume { volume: 110, .. })),
        "{fx:?}"
    );
}
