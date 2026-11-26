//! `bbs-tui`: adaptive BBS-styled Ratatui interface for bbscord.
//!
//! Scales continuously from a `16×2` HD44780 character LCD to a `640×180` 4K cockpit.
//! The normative design lives in `docs/TUI_SPEC.md`; section references (`§2.2`) in this
//! crate point there.
//!
//! The crate is a pure UI model: feed it [`bbs_core::BackendEvent`]s and key/paste/resize
//! input, render it into any ratatui backend, and execute the [`Effect`]s it returns.
//! Terminal setup and real pixel graphics live in `bbs-runtime-term`; this crate exposes the
//! exact image boxes ([`ImageSlot`]) the runtime overlays.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod app;
pub mod charset;
pub mod composer;
pub mod config;
pub mod draw;
pub mod geometry;
pub mod governor;
pub mod markdown;
pub mod messages;
pub mod meters;
pub mod micro;
pub mod modals;
pub mod panes;
pub mod render;
pub mod sanitize;
pub mod scroll;
pub mod state;
pub mod text;
pub mod theme;

pub use app::{App, Effect, Focus, ImageSlot, Mode};
pub use bbs_core;
pub use charset::{FrameStyle, Glyphs};
pub use config::{CtrlKMode, TuiConfig};
pub use geometry::{HProfile, Layout, LayoutInput, VProfile};
