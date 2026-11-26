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

    // Verify locally via DAVE modal -> DAVE✓ returns.
    let _ = app.handle_key(ch('D'));
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
