//! Shared offline fixtures: drive the core mock backends and feed their data to an `App`
//! exactly as the runtime would (as `BackendEvent`s). All data is synthetic.

#![allow(dead_code)]

use bbs_core::traits::{DiscordBackend, GifProvider, VideoStreamEngine, VoiceEngine};
use bbs_core::{
    BackendEvent, ColorDepth, ConnectionState, GraphicsTier, MockDiscordBackend, MockGifProvider,
    MockVideoStreamEngine, MockVoiceEngine, ResolvedCharset, mock_ids,
};
use bbs_tui::{App, TuiConfig};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

/// Collect the mock world as backend events.
pub fn mock_events(with_voice: bool) -> Vec<BackendEvent> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let be = MockDiscordBackend::new();
        let mut ev = Vec::new();
        let me = be.current_user().await.expect("user");
        let (guilds, folders) = be.list_guilds().await.expect("guilds");
        let dms = be.list_dm_channels().await.expect("dms");
        let gids: Vec<_> = guilds.iter().map(|g| g.id).collect();
        ev.push(BackendEvent::Ready {
            current_user: me,
            guilds,
            guild_folders: folders,
            dm_channels: dms.clone(),
        });
        let mut chans = Vec::new();
        for g in gids {
            let c = be.list_channels(g).await.expect("channels");
            chans.extend(c.iter().map(|c| c.id));
            ev.push(BackendEvent::ChannelsLoaded {
                guild_id: Some(g),
                channels: c,
            });
        }
        chans.extend(dms.iter().map(|c| c.id));
        for ch in chans {
            if let Ok(msgs) = be.fetch_messages(ch, None, 50).await
                && !msgs.is_empty()
            {
                ev.push(BackendEvent::MessagesLoaded {
                    channel_id: ch,
                    messages: msgs,
                    has_more_before: false,
                });
            }
        }
        for (i, l) in [42u32, 38, 51, 47, 120, 44, 40, 39, 41, 36]
            .iter()
            .enumerate()
        {
            let _ = i;
            ev.push(BackendEvent::ConnectionChanged {
                state: ConnectionState::Ready,
                latency_ms: Some(*l),
                detail: None,
            });
        }
        let gif = MockGifProvider::new();
        if let Ok(page) = gif.trending(12, None).await {
            ev.push(BackendEvent::GifSearchResults {
                request_id: 1,
                page,
            });
        }
        if with_voice {
            let voice = MockVoiceEngine::new();
            if let Ok(st) = voice
                .join_voice(Some(mock_ids::GUILD_RETRONET), mock_ids::CH_RETRONET_VOICE)
                .await
            {
                ev.push(BackendEvent::VoiceStatusUpdated(st));
            }
            if let Ok(levels) = voice.step_audio_levels(3) {
                ev.push(BackendEvent::VoiceAudioLevels {
                    channel_id: mock_ids::CH_RETRONET_VOICE,
                    levels,
                });
            }
            let video = MockVideoStreamEngine::new();
            if let Ok(streams) = video.list_streams(mock_ids::CH_RETRONET_VOICE).await {
                ev.push(BackendEvent::ActiveStreamsUpdated {
                    channel_id: mock_ids::CH_RETRONET_VOICE,
                    streams,
                });
            }
        }
        ev
    })
}

/// Config for a charset tier.
pub fn config(charset: ResolvedCharset) -> TuiConfig {
    let mut cfg = TuiConfig {
        charset,
        ..TuiConfig::default()
    };
    if charset == ResolvedCharset::Ascii7Bit {
        cfg.graphics = GraphicsTier::TextTagOnly;
        cfg.depth = ColorDepth::Monochrome;
    }
    cfg
}

/// A populated app with `#general-bbs` open.
pub fn demo_app(charset: ResolvedCharset, with_voice: bool) -> App {
    let mut app = App::new(config(charset));
    app.tick(1_764_000_000_000);
    for e in mock_events(with_voice) {
        let _ = app.handle_event(e);
    }
    app.select_channel(mock_ids::CH_RETRONET_GENERAL);
    let _ = app.tick(1_764_000_000_500);
    app
}

/// Render the app at a size into a fresh buffer.
pub fn render(app: &mut App, cols: u16, rows: u16) -> Buffer {
    let backend = TestBackend::new(cols, rows);
    let mut term = Terminal::new(backend).expect("terminal");
    term.draw(|f| app.render(f)).expect("draw");
    term.backend().buffer().clone()
}

/// Buffer → text lines (wide-glyph continuation cells skipped).
pub fn dump(buf: &Buffer) -> String {
    let a = buf.area;
    let mut out = String::new();
    for y in a.top()..a.bottom() {
        let mut skip = 0usize;
        for x in a.left()..a.right() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let s = buf[(x, y)].symbol();
            out.push_str(if s.is_empty() { " " } else { s });
            skip = unicode_width::UnicodeWidthStr::width(s).saturating_sub(1);
        }
        out.push('\n');
    }
    out
}

/// I4: no wide glyph crosses the right edge, and every continuation cell follows a wide glyph.
pub fn check_wide_cells(buf: &Buffer) -> Result<(), String> {
    let a = buf.area;
    for y in a.top()..a.bottom() {
        let mut expect_cont = false;
        for x in a.left()..a.right() {
            let s = buf[(x, y)].symbol();
            let w = unicode_width::UnicodeWidthStr::width(s);
            if expect_cont {
                expect_cont = false;
                continue;
            }
            if s.is_empty() {
                return Err(format!("orphan continuation at ({x},{y})"));
            }
            if w >= 2 {
                if x + 1 >= a.right() {
                    return Err(format!("wide glyph {s:?} crosses right edge at ({x},{y})"));
                }
                expect_cont = true;
            }
        }
    }
    Ok(())
}

/// I6: every symbol is HD44780-A00 safe.
pub fn check_a00(buf: &Buffer) -> Result<(), String> {
    for (i, c) in buf.content.iter().enumerate() {
        let s = c.symbol();
        if !s.bytes().all(|b| (0x20..=0x7D).contains(&b) && b != 0x5C) {
            return Err(format!("non-A00 symbol {s:?} at cell {i}"));
        }
    }
    Ok(())
}

/// Write a frame dump under the cargo target tmpdir for inspection.
pub fn write_dump(name: &str, buf: &Buffer) {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("frame-dumps");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(format!("{name}.txt")), dump(buf));
}
