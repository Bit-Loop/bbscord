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
    // Quit is reachable: Esc → NAV → MAIN, Down×8 → Quit → Enter → Right (Yes) → Enter (6-key safe).
    let _ = app.handle_key(key(KeyCode::Esc));
    let _ = app.handle_key(key(KeyCode::Esc));
    for _ in 0..8 {
        let _ = app.handle_key(key(KeyCode::Down));
    }
    let _ = app.handle_key(key(KeyCode::Enter));
    let _ = app.handle_key(key(KeyCode::Right));
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

#[test]
fn i4_modal_right_border_over_cjk_has_zero_orphans() {
    let mut app = demo_app(ResolvedCharset::Utf8, false);
    // Fill active channel with CJK lines at every column parity so a boxed modal cuts them.
    for offset in 0..4u64 {
        let mut msg = app
            .active_messages()
            .and_then(|ms| ms.back())
            .cloned()
            .expect("msg");
        msg.id = bbs_core::Snowflake::new(msg.id.as_u64() + 100 + offset);
        msg.content = format!(
            "{}{}",
            " ".repeat(usize::try_from(offset).unwrap_or(0)),
            "日本語テスト终端自适应布局".repeat(6)
        );
        let _ = app.handle_event(bbs_core::BackendEvent::MessageCreated(msg));
    }
    let _ = app.handle_key(key(KeyCode::F(2)));
    for cols in [80u16, 81, 82, 83, 120, 121] {
        let buf = render(&mut app, cols, 24);
        check_wide_cells(&buf).unwrap_or_else(|e| panic!("modal CJK cut at {cols}x24: {e}"));
    }
}

#[test]
fn autocomplete_multibyte_casefold_and_empty_top_scroll() {
    let mut app = demo_app(ResolvedCharset::Utf8, false);
    app.store.users.insert(
        bbs_core::Snowflake::new(999_001),
        bbs_core::User {
            id: bbs_core::Snowflake::new(999_001),
            username: "İü".to_string(),
            global_name: None,
            discriminator: None,
            avatar_hash: None,
            bot: false,
            accent_color: None,
            presence: bbs_core::PresenceStatus::Online,
        },
    );
    let _ = render(&mut app, 80, 24);
    let _ = app.handle_key(ch('i'));
    let _ = app.handle_paste("@i\u{0307}");
    // Must not panic when slicing `İü` with multi-byte lowered prefix.
    let _ = app.handle_key(key(KeyCode::Tab));

    // Empty Timeline::top_scroll must never panic.
    let empty_msgs = std::collections::VecDeque::new();
    let (params, pkey, height) = bbs_tui::render::messages_view_params(&app).expect("params");
    let res = |_, _| None;
    let ment = |_: &bbs_core::Message| false;
    let tl = bbs_tui::scroll::Timeline {
        msgs: &empty_msgs,
        params,
        pkey,
        res: &res,
        mentions: &ment,
        playback: None,
    };
    let mut cache = bbs_tui::scroll::LayoutCache::default();
    assert_eq!(
        tl.top_scroll(&mut cache, height).state,
        ScrollState::FollowTail
    );
}

#[test]
fn image_slots_for_inline_video_gif_tiles_and_modal_culling() {
    let mut app = demo_app(ResolvedCharset::Utf8, true);
    app.cfg.graphics = bbs_core::GraphicsTier::KittyPlaceholder;
    let _ = render(&mut app, 269, 45);
    assert!(
        !app.image_slots().is_empty(),
        "chat embeds should emit ImageSlots"
    );

    // Default watch sink is PopoutWindow (§6.5).
    let fx = app.handle_key(ch('w'));
    assert!(
        actions(&fx).iter().any(|a| matches!(
            a,
            UiAction::WatchStream {
                sink: bbs_core::VideoSinkKind::PopoutWindow,
                ..
            }
        )),
        "default sink must be PopoutWindow: {fx:?}"
    );
    // Toggle sink (`W`) switches to InlineTerminal on 269x45 KittyPlaceholder.
    let fx = app.handle_key(ch('W'));
    assert!(
        actions(&fx).iter().any(|a| matches!(
            a,
            UiAction::WatchStream {
                sink: bbs_core::VideoSinkKind::InlineTerminal,
                ..
            }
        )),
        "W toggle must switch to InlineTerminal: {fx:?}"
    );
    let _ = render(&mut app, 269, 45);
    assert!(
        app.image_slots()
            .iter()
            .any(|s| s.url == mock_ids::STREAM_RETRONET && s.animated && s.focused),
        "inline video must emit focused animated ImageSlot: {:?}",
        app.image_slots()
    );

    // Opening GIF picker culls underlying overlapping chat slots and emits GIF tile slots.
    let fx = app.handle_key(ch('G'));
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
    let _ = render(&mut app, 120, 40);
    let modal = geometry::modal_rect(ratatui::layout::Rect::new(0, 0, 120, 40), 86, 24);
    let gif_slots: Vec<_> = app
        .image_slots()
        .iter()
        .filter(|s| s.animated && !s.visible.intersection(modal.rect).is_empty())
        .collect();
    assert!(!gif_slots.is_empty(), "GIF tiles must emit ImageSlots");
    assert_eq!(
        gif_slots.iter().filter(|s| s.focused).count(),
        1,
        "only the selected GIF tile should be focused"
    );
    // Switching to Help modal culls all underlying overlapping slots.
    let _ = app.handle_key(key(KeyCode::Esc));
    let _ = app.handle_key(ch('?'));
    let _ = render(&mut app, 120, 40);
    let help_mr = geometry::modal_rect(ratatui::layout::Rect::new(0, 0, 120, 40), 78, 30);
    assert!(
        app.image_slots()
            .iter()
            .all(|s| s.visible.intersection(help_mr.rect).is_empty()),
        "no chat ImageSlot may overlap an open modal"
    );
}

#[test]
fn dave_verification_truthfulness_epoch_reset_and_degraded_banner() {
    let mut app = demo_app(ResolvedCharset::Utf8, true);
    // Mark one participant unverified -> must show DAVE~ (not DAVE✓).
    let mut voice = app.store.voice.clone().expect("voice");
    voice.participants[1].dave_verified = false;
    let _ = app.handle_event(bbs_core::BackendEvent::VoiceStatusUpdated(voice.clone()));
    let buf = render(&mut app, 269, 45);
    let d = common::dump(&buf);
    assert!(
        d.contains("DAVE~"),
        "unverified session must show DAVE~: {d}"
    );
    assert!(
        !d.contains("DAVE✓"),
        "unverified session must not show DAVE✓"
    );

    // Verify locally via DAVE modal (Ctrl+D) -> DAVE✓ returns.
    let _ = app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    let _ = app.handle_key(key(KeyCode::Down));
    let _ = app.handle_key(ch('v'));
    let _ = app.handle_key(key(KeyCode::Esc));
    assert!(app.is_dave_all_verified());
    let buf = render(&mut app, 269, 45);
    assert!(common::dump(&buf).contains("DAVE✓"));

    // Epoch change resets local verification -> back to DAVE~.
    if let bbs_core::DaveSessionState::Active { epoch, .. } = &mut voice.dave {
        *epoch += 1;
    }
    let _ = app.handle_event(bbs_core::BackendEvent::VoiceStatusUpdated(voice.clone()));
    assert!(!app.is_dave_all_verified());
    let buf = render(&mut app, 269, 45);
    assert!(common::dump(&buf).contains("DAVE~"));

    // Degraded DAVE shows a 1-row banner even on StandardBbs 80x24 without a Stage deck.
    voice.dave = bbs_core::DaveSessionState::Degraded {
        reason: "legacy client joined".to_string(),
    };
    let _ = app.handle_event(bbs_core::BackendEvent::VoiceStatusUpdated(voice));
    let buf = render(&mut app, 80, 24);
    let d = common::dump(&buf);
    assert!(
        d.contains("DAVE!"),
        "80x24 must display DAVE! downgrade alert: {d}"
    );
    assert!(d.contains("legacy client joined"), "{d}");
}

#[test]
fn title_closing_bracket_short_ring_dedupe_oversized_paste_and_cache_refresh() {
    let mut app = demo_app(ResolvedCharset::Utf8, false);
    // 1. Truncated 80x24 frame title preserves closing ╞.
    let buf = render(&mut app, 80, 24);
    let row0 = common::dump(&buf).lines().next().unwrap_or("").to_string();
    assert!(
        row0.contains("…╞"),
        "truncated title must close with …╞: {row0}"
    );

    // 2. Short-mode (80x8) & tall-narrow (48x14) incoming call ring appears only once.
    let _ = app.handle_event(bbs_core::BackendEvent::CallRingUpdated(
        bbs_core::CallRingState::IncomingRinging {
            channel_id: mock_ids::CH_DM_ALICE,
            caller_id: mock_ids::USER_ALICE,
            caller_name: "Modem Wizard".to_string(),
            is_group_dm: false,
        },
    ));
    for (c, r) in [(80, 8), (48, 14)] {
        let buf = render(&mut app, c, r);
        let d = common::dump(&buf);
        assert_eq!(
            d.matches("is calling").count(),
            1,
            "ring banner duplicated at {c}x{r}:\n{d}"
        );
    }

    // 3. Oversized paste opens Modal::too_long immediately (§9.4).
    let _ = app.handle_event(bbs_core::BackendEvent::CallRingUpdated(
        bbs_core::CallRingState::Idle,
    ));
    let _ = render(&mut app, 80, 24);
    let huge = "x".repeat(2500);
    let fx = app.handle_paste(&huge);
    assert!(actions(&fx).is_empty());
    let buf = render(&mut app, 80, 24);
    assert!(
        common::dump(&buf).contains("message.txt"),
        "oversized paste must open too-long modal: {}",
        common::dump(&buf)
    );
    let _ = app.handle_key(key(KeyCode::Esc));

    // 4. LayoutCache invalidates on same-length edit and voice playback progress.
    let mut last = app
        .active_messages()
        .and_then(|ms| ms.back())
        .cloned()
        .expect("last msg");
    let orig_len = last.content.len();
    let replacement = "Z".repeat(orig_len.max(4));
    last.content = replacement.clone();
    if let Some(poll) = last.poll.as_mut() {
        poll.total_votes += 50;
        if let Some(ans) = poll.answers.first_mut() {
            ans.votes += 50;
        }
    }
    let _ = app.handle_event(bbs_core::BackendEvent::MessageUpdated(last));
    let buf = render(&mut app, 120, 40);
    assert!(
        common::dump(&buf).contains("ZZZZ"),
        "same-length MessageUpdated must invalidate LayoutCache"
    );

    // Voice playback progress updates the waveform/time readout.
    let vm = app
        .active_messages()
        .and_then(|ms| ms.iter().find(|m| m.is_voice_message()))
        .cloned()
        .expect("voice msg");
    let att_id = vm
        .attachments
        .iter()
        .find(|a| a.is_voice_message())
        .expect("att")
        .id;
    let _ = app.handle_event(bbs_core::BackendEvent::VoicePlaybackUpdated(
        bbs_core::VoicePlaybackState {
            message_id: vm.id,
            attachment_id: att_id,
            playing: true,
            position_secs: 3.0,
            duration_secs: 6.0,
        },
    ));
    let buf = render(&mut app, 120, 40);
    let d = common::dump(&buf);
    assert!(
        d.contains("⏸") && d.contains("0:03/0:06"),
        "voice playback progress must render pause glyph and position: {d}"
    );
}

fn mouse_click(x: u16, y: u16) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

fn mouse_wheel(x: u16, y: u16, down: bool) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent {
        kind: if down {
            crossterm::event::MouseEventKind::ScrollDown
        } else {
            crossterm::event::MouseEventKind::ScrollUp
        },
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn mouse_click_focus_tree_messages_modals_and_wheel_scroll() {
    let mut app = demo_app(ResolvedCharset::Utf8, false);
    let _ = app.tick(10_000);
    let _ = render(&mut app, 120, 36);
    let layout = app.layout().expect("layout").clone();
    let sidebar = layout
        .panes
        .iter()
        .find(|p| p.kind == geometry::PaneKind::Sidebar)
        .expect("sidebar")
        .rect;

    // Click composer -> enters Insert mode.
    let _ = app.handle_mouse(mouse_click(layout.composer.x + 1, layout.composer.y));
    assert_eq!(app.mode, bbs_tui::Mode::Insert);

    // Click messages pane -> exits Insert, focuses Messages, selects clicked row.
    let msg_y = layout.messages.bottom().saturating_sub(1);
    let _ = app.handle_mouse(mouse_click(layout.messages.x + 2, msg_y));
    assert_eq!(app.mode, bbs_tui::Mode::Normal);
    assert_eq!(app.focus, bbs_tui::Focus::Messages);
    assert!(app.selected.is_some());

    // Wheel up over messages scrolls off tail.
    let _ = app.handle_mouse(mouse_wheel(layout.messages.x + 2, msg_y, false));
    assert!(matches!(app.scroll_state(), ScrollState::Anchored(_)));

    // Click on a DM row in the sidebar -> opens channel & auto-focuses composer.
    let dm_row_y = sidebar.y + 1; // row 0 is Home ("DMs"), row 1 is first DM
    let fx = app.handle_mouse(mouse_click(sidebar.x + 1, dm_row_y));
    assert!(
        actions(&fx)
            .iter()
            .any(|a| matches!(a, UiAction::SelectChannel(_))),
        "clicking DM row must select channel: {fx:?}"
    );
    assert_eq!(
        app.mode,
        bbs_tui::Mode::Insert,
        "auto_focus should focus composer on channel click"
    );

    // Open Quit confirm modal -> click outside modal dismisses it.
    let _ = app.handle_key(key(KeyCode::Esc));
    let _ = app.handle_key(ch('q'));
    let _ = render(&mut app, 120, 36);
    let fx = app.handle_mouse(mouse_click(0, 0));
    assert!(!fx.iter().any(|e| matches!(e, Effect::Quit)));
    let buf = render(&mut app, 120, 36);
    assert!(!common::dump(&buf).contains("Quit bbscord?"));
}

#[test]
fn composer_autofocus_esc_reply_toggle_autocomplete_and_safe_confirm() {
    let mut app = demo_app(ResolvedCharset::Utf8, false);
    let _ = app.tick(20_000);
    let _ = render(&mut app, 120, 36);

    // Focus messages and select the last own message (or send one via event).
    let me = app.store.me.clone().expect("me");
    let ch_id = app.active_channel.expect("ch");
    let own_id = bbs_core::Snowflake::new(99_000_001);
    let own_msg = bbs_core::Message {
        id: own_id,
        channel_id: ch_id,
        guild_id: app.active_guild,
        author: me,
        content: "check https://example.invalid/a and https://example.invalid/b".to_string(),
        timestamp: "2026-11-26T12:00:00Z".to_string(),
        edited_timestamp: None,
        pinned: false,
        reply_to: None,
        mentions: Vec::new(),
        mention_everyone: false,
        attachments: Vec::new(),
        embeds: Vec::new(),
        reactions: Vec::new(),
        poll: None,
        flags: 0,
        nonce: None,
        author_nick: None,
        author_role_ids: Vec::new(),
        mention_roles: Vec::new(),
    };
    let _ = app.handle_event(bbs_core::BackendEvent::MessageCreated(own_msg));
    app.focus = bbs_tui::Focus::Messages;
    app.selected = Some(own_id);

    // Safe delete confirmation: `d` opens confirm with [ No ] focused -> Enter does NOT delete!
    let _ = app.handle_key(ch('d'));
    let buf = render(&mut app, 120, 36);
    let dump = common::dump(&buf);
    assert!(dump.contains("Delete this message?"), "{dump}");
    assert!(dump.contains("[ No ]") && dump.contains("[ Yes ]"), "{dump}");
    let fx = app.handle_key(key(KeyCode::Enter));
    assert!(
        !actions(&fx)
            .iter()
            .any(|a| matches!(a, UiAction::DeleteMessage { .. })),
        "Enter on default-No confirm must NOT delete: {fx:?}"
    );
    // Re-open, move Right to [ Yes ], press Enter -> deletes!
    let _ = app.handle_key(ch('d'));
    let _ = app.handle_key(key(KeyCode::Right));
    let fx = app.handle_key(key(KeyCode::Enter));
    assert!(
        actions(&fx)
            .iter()
            .any(|a| matches!(a, UiAction::DeleteMessage { message_id, .. } if *message_id == own_id)),
        "Right+Enter must delete: {fx:?}"
    );

    // Copy message (`y`) and URL (`u`), and multi-URL opener (`o`) -> picker -> Enter -> take_open_url().
    let _ = app.handle_key(ch('y'));
    assert!(app.take_copy_text().is_some_and(|t| t.contains("example.invalid/a")));
    let _ = app.handle_key(ch('u'));
    assert_eq!(app.take_copy_text().as_deref(), Some("https://example.invalid/a"));
    let _ = app.handle_key(ch('o'));
    let buf = render(&mut app, 120, 36);
    assert!(common::dump(&buf).contains("Open>"));
    let _ = app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.take_open_url().as_deref(), Some("https://example.invalid/a"));

    // Reply (`r`) defaults to mention: true; Tab toggles mention OFF; Esc cancels target without wiping draft!
    let _ = app.handle_key(ch('r'));
    assert_eq!(app.mode, bbs_tui::Mode::Insert);
    assert!(matches!(
        app.composer.target,
        bbs_tui::composer::ComposeTarget::Reply { mention: true, .. }
    ));
    let _ = app.handle_key(key(KeyCode::Tab));
    assert!(matches!(
        app.composer.target,
        bbs_tui::composer::ComposeTarget::Reply { mention: false, .. }
    ));
    let mut saw_typing = false;
    for c in "draft :fi".chars() {
        let fx = app.handle_key(ch(c));
        if actions(&fx).iter().any(|a| matches!(a, UiAction::SendTyping { .. })) {
            saw_typing = true;
        }
    }
    assert!(saw_typing, "typing in Insert mode should emit throttled SendTyping");
    assert!(app.autocomplete.is_open(), ":fi should open emoji autocomplete");
    // Accept `:fire:` via Tab -> inserts 🔥.
    let _ = app.handle_key(key(KeyCode::Tab));
    assert!(app.composer.text().contains('🔥'), "{}", app.composer.text());
    assert!(!app.autocomplete.is_open());

    // First Esc cancels Reply target while preserving draft text; second Esc exits to Normal preserving draft!
    let _ = app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.composer.target, bbs_tui::composer::ComposeTarget::New);
    assert_eq!(app.mode, bbs_tui::Mode::Insert);
    assert!(app.composer.text().starts_with("draft "));
    let _ = app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.mode, bbs_tui::Mode::Normal);
    assert!(app.composer.text().starts_with("draft "));

    // Typing `@mod` opens Mention autocomplete; Tab accepts `@modem_wizard `.
    app.composer.clear();
    let _ = app.handle_key(ch('i'));
    for c in "@mod".chars() {
        let _ = app.handle_key(ch(c));
    }
    assert!(app.autocomplete.is_open(), "@mod should open mention autocomplete");
    let _ = app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.composer.text(), "@modem_wizard ");
    // Typing `#gen` opens Channel autocomplete; Tab accepts `#general-bbs `.
    for c in "#gen".chars() {
        let _ = app.handle_key(ch(c));
    }
    assert!(app.autocomplete.is_open(), "#gen should open channel autocomplete");
    let _ = app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.composer.text(), "@modem_wizard #general-bbs ");
    let _ = app.handle_key(key(KeyCode::Esc));

    // Sending `@modem_wizard` outside backticks expands to `<@USER_ALICE>`, while `` `@modem_wizard` `` stays literal!
    app.composer.clear();
    let _ = app.handle_key(ch('r'));
    let _ = app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT));
    let _ = app.handle_paste("hi @modem_wizard and `@modem_wizard` in #general-bbs");
    let fx = app.handle_key(key(KeyCode::Enter));
    let sent = actions(&fx).into_iter().find_map(|a| {
        if let UiAction::SendMessage {
            content,
            mention_replied_user,
            ..
        } = a
        {
            Some((content.clone(), *mention_replied_user))
        } else {
            None
        }
    });
    let (content, mention_replied) = sent.expect("send message");
    assert!(!mention_replied, "Alt+M should toggle reply ping off");
    assert!(
        content.contains(&format!("<@{}>", mock_ids::USER_ALICE)),
        "outgoing @modem_wizard must expand to <@id>: {content}"
    );
    assert!(
        content.contains("`@modem_wizard`"),
        "inline code `@modem_wizard` must NOT expand: {content}"
    );
    assert!(
        content.contains(&format!("<#{}>", mock_ids::CH_RETRONET_GENERAL)),
        "outgoing #general-bbs must expand to <#id>: {content}"
    );
}

#[test]
fn at_a_glance_coloring_roles_presence_folders_mutes_and_ack() {
    let mut app = demo_app(ResolvedCharset::Utf8, false);
    // Add guild roles + guild member with role color & guild nick.
    let role_id = bbs_core::Snowflake::new(777_001);
    let _ = app.handle_event(bbs_core::BackendEvent::GuildRolesUpdated {
        guild_id: mock_ids::GUILD_RETRONET,
        roles: vec![bbs_core::Role {
            id: role_id,
            name: "SysOps".to_string(),
            color: 0x00FF_55AA,
            position: 10,
            hoist: true,
        }],
    });
    let _ = app.handle_event(bbs_core::BackendEvent::GuildMembersUpdated {
        guild_id: mock_ids::GUILD_RETRONET,
        members: vec![bbs_core::GuildMemberSummary {
            user_id: mock_ids::USER_ALICE,
            nick: Some("ModemQueen".to_string()),
            role_ids: vec![role_id],
        }],
    });

    // Add a colored GuildFolder and mute a channel while keeping another unread and mentioned.
    app.store.folders = vec![bbs_core::GuildFolder {
        id: Some(1),
        name: Some("RetroNetOps".to_string()),
        color: Some(0x0033_CCFF),
        guild_ids: vec![mock_ids::GUILD_RETRONET],
        collapsed: false,
    }];
    let _ = app.handle_event(bbs_core::BackendEvent::GuildSettingsUpdated {
        guild_id: Some(mock_ids::GUILD_RETRONET),
        muted: false,
        suppress_everyone: true,
        suppress_roles: false,
        muted_channels: vec![(mock_ids::CH_RETRONET_MUTED_LOGS, true)],
    });
    assert!(app.store.is_channel_muted(mock_ids::CH_RETRONET_MUTED_LOGS));

    // Verify ChannelsLoaded preserves unread/mention/muted state when REST returns zeroes!
    let mut fresh_chs = app
        .store
        .channels
        .get(&mock_ids::GUILD_RETRONET)
        .cloned()
        .unwrap_or_default();
    for c in &mut fresh_chs {
        c.unread_count = 0;
        c.mention_count = 0;
        c.muted = false;
        c.last_ack_message_id = None;
    }
    if let Some(chs) = app.store.channels.get_mut(&mock_ids::GUILD_RETRONET)
        && let Some(thr) = chs.iter_mut().find(|c| c.id == mock_ids::CH_RETRONET_THREAD)
    {
        thr.unread_count = 3;
        thr.mention_count = 1;
    }
    let _ = app.handle_event(bbs_core::BackendEvent::ChannelsLoaded {
        guild_id: Some(mock_ids::GUILD_RETRONET),
        channels: fresh_chs,
    });
    assert!(app.store.is_channel_muted(mock_ids::CH_RETRONET_MUTED_LOGS));
    let thr = app
        .store
        .channel(mock_ids::CH_RETRONET_THREAD)
        .expect("thread");
    assert_eq!(thr.unread_count, 3);
    assert_eq!(thr.mention_count, 1);

    // Render and verify folder + guild nick render cleanly.
    let buf = render(&mut app, 160, 40);
    let dump = common::dump(&buf);
    assert!(dump.contains("RetroNetOps"), "folder name must render: {dump}");
    assert!(
        dump.contains("ModemQueen"),
        "guild nick must render in timeline/members: {dump}"
    );

    // Selecting an unread channel emits deduplicated AckMessage and clears unread.
    app.select_channel(mock_ids::CH_RETRONET_THREAD);
    let fx = app.tick(30_000);
    let _ = fx;
    let _ = app.handle_event(bbs_core::BackendEvent::MessageAcked {
        channel_id: mock_ids::CH_RETRONET_THREAD,
        last_message_id: bbs_core::Snowflake::new(9_999_999),
        mention_count: 0,
    });
    let thr = app
        .store
        .channel(mock_ids::CH_RETRONET_THREAD)
        .expect("thread");
    assert_eq!(thr.unread_count, 0);
    assert_eq!(thr.mention_count, 0);
}

#[test]
fn voice_sidebar_occupants_toggle_leave_and_guards() {
    let mut app = demo_app(ResolvedCharset::Utf8, false);
    // Not in voice: m and D must NOT emit SetSelfMute/SetSelfDeaf!
    let fx_m = app.handle_key(ch('m'));
    let fx_d = app.handle_key(ch('D'));
    assert!(actions(&fx_m).is_empty(), "mute outside voice must no-op: {fx_m:?}");
    assert!(actions(&fx_d).is_empty(), "deaf outside voice must no-op: {fx_d:?}");

    // OutgoingRinging + h must emit StopCallRing AND LeaveVoice.
    let _ = app.handle_event(bbs_core::BackendEvent::CallRingUpdated(
        bbs_core::CallRingState::OutgoingRinging {
            channel_id: mock_ids::CH_DM_ALICE,
            target_user_ids: vec![mock_ids::USER_ALICE],
        },
    ));
    app.focus = bbs_tui::Focus::Messages;
    let fx_h = app.handle_key(ch('h'));
    assert!(
        actions(&fx_h)
            .iter()
            .any(|a| matches!(a, UiAction::StopCallRing { .. })),
        "decline during OutgoingRinging must StopCallRing: {fx_h:?}"
    );
    assert!(
        actions(&fx_h)
            .iter()
            .any(|a| matches!(a, UiAction::LeaveVoice)),
        "decline during OutgoingRinging must LeaveVoice: {fx_h:?}"
    );

    // VoiceRosterUpdated populates nested VoiceMember rows under the voice channel.
    let _ = app.handle_event(bbs_core::BackendEvent::VoiceRosterUpdated {
        guild_id: Some(mock_ids::GUILD_RETRONET),
        channel_id: mock_ids::CH_RETRONET_VOICE,
        participants: vec![bbs_core::VoiceParticipant {
            user_id: mock_ids::USER_ALICE,
            display_name: "Alice".to_string(),
            speaking: true,
            self_mute: false,
            self_deaf: false,
            server_mute: false,
            server_deaf: false,
            volume: 120,
            rms_db: -12.0,
            dave_verified: true,
            streaming: false,
        }],
    });
    app.select_guild(Some(mock_ids::GUILD_RETRONET));
    let items = app.sidebar_items();
    assert!(
        items.iter().any(|it| matches!(
            it,
            bbs_tui::app::SideItem::VoiceMember(ch, uid, _)
                if *ch == mock_ids::CH_RETRONET_VOICE && *uid == mock_ids::USER_ALICE
        )),
        "voice occupants must appear nested under voice channel: {items:?}"
    );

    // Re-selecting an already-joined voice channel emits LeaveVoice instead of duplicate JoinVoice!
    let mut app_vc = demo_app(ResolvedCharset::Utf8, true);
    let vc_id = app_vc.store.voice.as_ref().expect("voice").channel_id;
    app_vc.select_channel(vc_id);
    let fx = app_vc.tick(1000);
    assert!(
        actions(&fx).iter().any(|a| matches!(a, UiAction::LeaveVoice)),
        "re-selecting joined voice channel must emit LeaveVoice: {fx:?}"
    );
    assert!(
        !actions(&fx)
            .iter()
            .any(|a| matches!(a, UiAction::JoinVoice { .. })),
        "re-selecting joined voice channel must NOT re-send JoinVoice: {fx:?}"
    );
}

#[test]
fn gif_debounce_error_grid_nav_and_media_polish() {
    let mut app = demo_app(ResolvedCharset::Utf8, false);
    app.cfg.graphics = bbs_core::GraphicsTier::KittyPlaceholder;
    let _ = app.tick(5_000);
    let _ = render(&mut app, 120, 40);

    // Open GIF picker -> initial trending SearchGifs emitted.
    let fx_open = app.handle_key(ch('G'));
    let req0 = actions(&fx_open)
        .iter()
        .find_map(|a| {
            if let UiAction::SearchGifs { request_id, .. } = a {
                Some(*request_id)
            } else {
                None
            }
        })
        .expect("initial gif search");

    // Typing "cat" debounces 300ms (zero SearchGifs on individual keystrokes!).
    for c in "cat".chars() {
        let fx = app.handle_key(ch(c));
        assert!(
            actions(&fx).is_empty(),
            "typing in GIF picker must debounce 300ms: {fx:?}"
        );
    }
    let fx_early = app.tick(5_200);
    assert!(actions(&fx_early).is_empty(), "must not fire before 300ms");
    let fx_due = app.tick(5_350);
    let req1 = actions(&fx_due)
        .iter()
        .find_map(|a| {
            if let UiAction::SearchGifs { request_id, query } = a {
                assert_eq!(query.query, "cat");
                assert!(!query.trending);
                Some(*request_id)
            } else {
                None
            }
        })
        .expect("debounced gif search after 300ms");
    assert_ne!(req0, req1);

    // GifSearchFailed shows error instead of hanging on "searching…".
    let _ = app.handle_event(bbs_core::BackendEvent::GifSearchFailed {
        request_id: req1,
        error: "upstream 429 rate limited".to_string(),
    });
    let buf = render(&mut app, 120, 40);
    let dump = common::dump(&buf);
    assert!(dump.contains("GIF error:"), "{dump}");
    assert!(dump.contains("upstream 429"), "{dump}");
    let _ = app.handle_key(key(KeyCode::Esc));

    // MessagePatched merges EmbedKind::Gifv WITH title, invalidates cache, and resizes Discord proxy URL.
    let ch_id = app.active_channel.expect("ch");
    let guild_id = app.active_guild;
    let me = app.store.me.clone().expect("me");
    let m1_id = bbs_core::Snowflake::new(1_300_000_000_000_000_000);
    let m2_id = bbs_core::Snowflake::new(1_300_000_000_000_000_000 + (90_000_000 << 22)); // next day!
    let proxy_url =
        "https://media.discordapp.net/attachments/1/2/dance.gif?ex=aaa&is=bbb&hm=ccc".to_string();
    let base_msg = |id: bbs_core::Snowflake, text: &str| bbs_core::Message {
        id,
        channel_id: ch_id,
        guild_id,
        author: me.clone(),
        content: text.to_string(),
        timestamp: String::new(),
        edited_timestamp: None,
        pinned: false,
        reply_to: None,
        mentions: Vec::new(),
        mention_everyone: false,
        attachments: Vec::new(),
        embeds: Vec::new(),
        reactions: Vec::new(),
        poll: None,
        flags: 0,
        nonce: None,
        author_nick: None,
        author_role_ids: Vec::new(),
        mention_roles: Vec::new(),
    };
    let _ = app.handle_event(bbs_core::BackendEvent::MessageCreated(base_msg(
        m1_id,
        "first day",
    )));
    let _ = app.handle_event(bbs_core::BackendEvent::MessageCreated(base_msg(
        m2_id,
        "second day",
    )));
    let _ = render(&mut app, 120, 40);

    // Patch BOTH messages with identical GIFV thumbnail URL AND a title!
    let gifv_embed = bbs_core::Embed {
        kind: bbs_core::EmbedKind::Gifv,
        title: Some("Dancing Cat".to_string()),
        thumbnail: Some(bbs_core::EmbedMedia {
            url: proxy_url.clone(),
            proxy_url: Some(proxy_url.clone()),
            width: Some(400),
            height: Some(300),
        }),
        ..Default::default()
    };
    for id in [m1_id, m2_id] {
        let _ = app.handle_event(bbs_core::BackendEvent::MessagePatched(
            bbs_core::MessagePatch {
                id,
                channel_id: ch_id,
                embeds: Some(vec![gifv_embed.clone()]),
                ..Default::default()
            },
        ));
    }
    let buf = render(&mut app, 120, 40);
    let dump = common::dump(&buf);
    // Date separator rendered between m1 and m2!
    assert!(
        dump.contains(&bbs_tui::messages::fmt_date_separator(m2_id, 0)),
        "date separator must appear on local date change: {dump}"
    );
    // Resized proxy URL with ex/is/hm preserved, animated=true, AND two distinct non-merged slots!
    let matching_slots: Vec<_> = app
        .image_slots()
        .iter()
        .filter(|s| s.url.contains("media.discordapp.net/attachments/1/2/dance.gif"))
        .collect();
    assert_eq!(
        matching_slots.len(),
        2,
        "duplicate URLs in separate messages must NOT merge into one slot: {matching_slots:?}"
    );
    for s in &matching_slots {
        assert!(s.animated, "Gifv embed must be marked animated");
        assert!(s.url.contains("ex=aaa") && s.url.contains("width=") && s.url.contains("format=webp"));
    }
    assert!(
        matching_slots.iter().any(|s| s.focused),
        "newest visible animated chat GIF must be focused when no message is selected"
    );
}
