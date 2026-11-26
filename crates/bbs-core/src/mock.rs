//! Rich, deterministic, 100% offline mock implementations for `--mock-demo` and CI testing.
//!
//! All data here is synthetic and contains zero real user, guild, or credential information.

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::error::{CoreError, CoreResult};
use crate::events::BackendEvent;
use crate::model::{
    Attachment, CallRingState, Channel, ChannelKind, DaveSessionState, Embed, EmbedAuthor,
    EmbedField, EmbedFooter, EmbedKind, EmbedMedia, EmbedProvider, GifItem, GifProviderKind,
    GifSearchPage, GifSearchQuery, Guild, GuildFolder, GuildMemberSummary, Message,
    MessageReference, PollAnswerSummary, PollSummary, PresenceStatus, Reaction,
    RefreshedAttachmentUrl, RgbaFrame, Role, Snowflake, StreamDescriptor, StreamQuality,
    StreamSourceKind, User, VideoFrame, VideoSinkKind, VoiceChannelStatus, VoiceConnectionState,
    VoiceMessageMeta, VoiceParticipant, VoicePlaybackState,
};
use crate::traits::{DiscordBackend, GifProvider, VideoStreamEngine, VoiceEngine};

/// Well-known deterministic Snowflakes used by the offline mock fixtures.
pub mod mock_ids {
    use crate::model::Snowflake;

    /// Signed-in mock user (`sysop`).
    pub const USER_SELF: Snowflake = Snowflake(100_000_000_000_000_001);
    /// Peer mock user (`modem_wizard`).
    pub const USER_ALICE: Snowflake = Snowflake(100_000_000_000_000_002);
    /// Peer mock user (`crt_hacker`).
    pub const USER_BOB: Snowflake = Snowflake(100_000_000_000_000_003);
    /// Peer mock user (`ansi_artist`).
    pub const USER_CAROL: Snowflake = Snowflake(100_000_000_000_000_004);
    /// Bot mock user (`bbs_daemon`).
    pub const USER_BOT: Snowflake = Snowflake(100_000_000_000_000_005);
    /// Offline peer mock user (`tape_archivist`).
    pub const USER_DAVE: Snowflake = Snowflake(100_000_000_000_000_006);

    /// RetroNet role: SysOp (`#38BDF8`).
    pub const ROLE_SYSOP: Snowflake = Snowflake(600_000_000_000_000_001);
    /// RetroNet role: Daemon (`#BD93F9`).
    pub const ROLE_DAEMON: Snowflake = Snowflake(600_000_000_000_000_002);
    /// RetroNet role: Netrunner (`#50FA7B`).
    pub const ROLE_NETRUNNER: Snowflake = Snowflake(600_000_000_000_000_003);
    /// RetroNet role: ANSI Artist (`#FF79C6`).
    pub const ROLE_ARTIST: Snowflake = Snowflake(600_000_000_000_000_004);
    /// RetroNet role: Caretaker (`#FFB86C`).
    pub const ROLE_CARETAKER: Snowflake = Snowflake(600_000_000_000_000_005);
    /// Rust Lab role: Core Maintainer (`#F97316`).
    pub const ROLE_RUST_CORE: Snowflake = Snowflake(600_000_000_000_000_010);
    /// Demoscene role: Shader Coder (`#FACC15`).
    pub const ROLE_DEMO_CODER: Snowflake = Snowflake(600_000_000_000_000_020);

    /// Guild 1: RetroNet BBS.
    pub const GUILD_RETRONET: Snowflake = Snowflake(200_000_000_000_000_001);
    /// Guild 2: Rust Systems Lab.
    pub const GUILD_RUST_LAB: Snowflake = Snowflake(200_000_000_000_000_002);
    /// Guild 3: Midnight Demoscene.
    pub const GUILD_DEMOSCENE: Snowflake = Snowflake(200_000_000_000_000_003);

    /// RetroNet category: Lobby.
    pub const CH_CAT_LOBBY: Snowflake = Snowflake(300_000_000_000_000_001);
    /// RetroNet text channel: `#general-bbs`.
    pub const CH_RETRONET_GENERAL: Snowflake = Snowflake(300_000_000_000_000_002);
    /// RetroNet voice channel: `Lounge 56k`.
    pub const CH_RETRONET_VOICE: Snowflake = Snowflake(300_000_000_000_000_003);
    /// RetroNet stage channel: `Town Hall Stage`.
    pub const CH_RETRONET_STAGE: Snowflake = Snowflake(300_000_000_000_000_004);
    /// RetroNet public thread: `ANSI Theme Showcase`.
    pub const CH_RETRONET_THREAD: Snowflake = Snowflake(300_000_000_000_000_005);
    /// RetroNet muted text channel: `#bot-firehose`.
    pub const CH_RETRONET_MUTED_LOGS: Snowflake = Snowflake(300_000_000_000_000_006);
    /// Rust Lab text channel: `#async-runtime`.
    pub const CH_RUST_ASYNC: Snowflake = Snowflake(300_000_000_000_000_010);
    /// Rust Lab forum channel: `#rfc-board`.
    pub const CH_RUST_FORUM: Snowflake = Snowflake(300_000_000_000_000_011);
    /// Demoscene text channel: `#shaders-and-sixel`.
    pub const CH_DEMO_SHADERS: Snowflake = Snowflake(300_000_000_000_000_020);
    /// 1:1 Direct Message with `modem_wizard` (Online).
    pub const CH_DM_ALICE: Snowflake = Snowflake(300_000_000_000_000_100);
    /// 1:1 Direct Message with `crt_hacker` (Idle).
    pub const CH_DM_BOB: Snowflake = Snowflake(300_000_000_000_000_101);
    /// 1:1 Direct Message with `ansi_artist` (Do Not Disturb).
    pub const CH_DM_CAROL: Snowflake = Snowflake(300_000_000_000_000_102);
    /// 1:1 Direct Message with `tape_archivist` (Offline).
    pub const CH_DM_DAVE: Snowflake = Snowflake(300_000_000_000_000_103);
    /// Active Group DM (`type = 3`) `"BBS Ops War Room"` with active call state.
    pub const CH_GROUP_DM_OPS: Snowflake = Snowflake(300_000_000_000_000_200);

    /// Stream key for RetroNet voice Go Live screenshare.
    pub const STREAM_RETRONET: &str =
        "guild:200000000000000001:300000000000000003:100000000000000002";
    /// Stream key for Group DM Go Live screenshare.
    pub const STREAM_GROUP_DM: &str = "call:300000000000000200:100000000000000003";
}

/// Seeded voice channel roster tuple `(guild_id, channel_id, participants)` for `--mock-demo`.
pub type MockVoiceRoster = (Option<Snowflake>, Snowflake, Vec<VoiceParticipant>);

#[derive(Debug)]
struct MockDiscordState {
    current_user: User,
    users: HashMap<Snowflake, User>,
    guilds: Vec<Guild>,
    guild_folders: Vec<GuildFolder>,
    guild_members: HashMap<Snowflake, Vec<GuildMemberSummary>>,
    channels: HashMap<Snowflake, Channel>,
    messages: HashMap<Snowflake, Vec<Message>>,
    call_rings: HashMap<Snowflake, CallRingState>,
    voice_rosters: Vec<MockVoiceRoster>,
}

/// Deterministic in-memory [`DiscordBackend`] with rich guilds, Group DMs, embeds, polls, and calls.
pub struct MockDiscordBackend {
    state: RwLock<MockDiscordState>,
    next_id: AtomicU64,
    events: broadcast::Sender<BackendEvent>,
}

impl Default for MockDiscordBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MockDiscordBackend {
    /// Construct a fully seeded offline [`MockDiscordBackend`].
    #[must_use]
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(256);

        let user_self = User {
            id: mock_ids::USER_SELF,
            username: "sysop".to_string(),
            global_name: Some("SysOp Prime".to_string()),
            discriminator: None,
            avatar_hash: None,
            bot: false,
            accent_color: Some(0x38BDF8),
            presence: PresenceStatus::Online,
        };
        let user_alice = User {
            id: mock_ids::USER_ALICE,
            username: "modem_wizard".to_string(),
            global_name: Some("Modem Wizard".to_string()),
            discriminator: None,
            avatar_hash: None,
            bot: false,
            accent_color: Some(0x50FA7B),
            presence: PresenceStatus::Online,
        };
        let user_bob = User {
            id: mock_ids::USER_BOB,
            username: "crt_hacker".to_string(),
            global_name: Some("CRT Hacker".to_string()),
            discriminator: None,
            avatar_hash: None,
            bot: false,
            accent_color: Some(0xFFB86C),
            presence: PresenceStatus::Idle,
        };
        let user_carol = User {
            id: mock_ids::USER_CAROL,
            username: "ansi_artist".to_string(),
            global_name: Some("ANSI Artist".to_string()),
            discriminator: None,
            avatar_hash: None,
            bot: false,
            accent_color: Some(0xFF79C6),
            presence: PresenceStatus::DoNotDisturb,
        };
        let user_bot = User {
            id: mock_ids::USER_BOT,
            username: "bbs_daemon".to_string(),
            global_name: Some("BBS Daemon".to_string()),
            discriminator: None,
            avatar_hash: None,
            bot: true,
            accent_color: Some(0xBD93F9),
            presence: PresenceStatus::Online,
        };
        let user_dave = User {
            id: mock_ids::USER_DAVE,
            username: "tape_archivist".to_string(),
            global_name: Some("Tape Archivist".to_string()),
            discriminator: None,
            avatar_hash: None,
            bot: false,
            accent_color: Some(0x64748B),
            presence: PresenceStatus::Offline,
        };

        let mut users = HashMap::new();
        for u in [
            &user_self,
            &user_alice,
            &user_bob,
            &user_carol,
            &user_bot,
            &user_dave,
        ] {
            users.insert(u.id, u.clone());
        }

        let retronet_roles = vec![
            Role {
                id: mock_ids::ROLE_SYSOP,
                name: "SysOp".to_string(),
                color: 0x38BDF8,
                position: 30,
                hoist: true,
            },
            Role {
                id: mock_ids::ROLE_DAEMON,
                name: "Daemon".to_string(),
                color: 0xBD93F9,
                position: 25,
                hoist: true,
            },
            Role {
                id: mock_ids::ROLE_NETRUNNER,
                name: "Netrunner".to_string(),
                color: 0x50FA7B,
                position: 20,
                hoist: true,
            },
            Role {
                id: mock_ids::ROLE_ARTIST,
                name: "ANSI Artist".to_string(),
                color: 0xFF79C6,
                position: 15,
                hoist: true,
            },
            Role {
                id: mock_ids::ROLE_CARETAKER,
                name: "Caretaker".to_string(),
                color: 0xFFB86C,
                position: 10,
                hoist: false,
            },
        ];

        let guilds = vec![
            Guild {
                id: mock_ids::GUILD_RETRONET,
                name: "RetroNet BBS".to_string(),
                icon_hash: None,
                owner_id: Some(mock_ids::USER_SELF),
                member_count: Some(128),
                unread: true,
                mention_count: 2,
                roles: retronet_roles,
                muted: false,
                suppress_everyone: false,
                suppress_roles: false,
            },
            Guild {
                id: mock_ids::GUILD_RUST_LAB,
                name: "Rust Systems Lab".to_string(),
                icon_hash: None,
                owner_id: Some(mock_ids::USER_ALICE),
                member_count: Some(512),
                unread: true,
                mention_count: 0,
                roles: vec![Role {
                    id: mock_ids::ROLE_RUST_CORE,
                    name: "Core Maintainer".to_string(),
                    color: 0xF97316,
                    position: 20,
                    hoist: true,
                }],
                muted: false,
                suppress_everyone: false,
                suppress_roles: false,
            },
            Guild {
                id: mock_ids::GUILD_DEMOSCENE,
                name: "Midnight Demoscene".to_string(),
                icon_hash: None,
                owner_id: Some(mock_ids::USER_CAROL),
                member_count: Some(64),
                unread: false,
                mention_count: 0,
                roles: vec![Role {
                    id: mock_ids::ROLE_DEMO_CODER,
                    name: "Shader Coder".to_string(),
                    color: 0xFACC15,
                    position: 15,
                    hoist: true,
                }],
                muted: false,
                suppress_everyone: true,
                suppress_roles: false,
            },
        ];

        let mut guild_members: HashMap<Snowflake, Vec<GuildMemberSummary>> = HashMap::new();
        guild_members.insert(
            mock_ids::GUILD_RETRONET,
            vec![
                GuildMemberSummary {
                    user_id: mock_ids::USER_SELF,
                    nick: Some("Chief SysOp".to_string()),
                    role_ids: vec![mock_ids::ROLE_SYSOP],
                },
                GuildMemberSummary {
                    user_id: mock_ids::USER_BOT,
                    nick: None,
                    role_ids: vec![mock_ids::ROLE_DAEMON],
                },
                GuildMemberSummary {
                    user_id: mock_ids::USER_ALICE,
                    nick: Some("Modem Wiz".to_string()),
                    role_ids: vec![mock_ids::ROLE_NETRUNNER],
                },
                GuildMemberSummary {
                    user_id: mock_ids::USER_CAROL,
                    nick: Some("ANSI Virtuoso".to_string()),
                    role_ids: vec![mock_ids::ROLE_ARTIST],
                },
                GuildMemberSummary {
                    user_id: mock_ids::USER_BOB,
                    nick: None,
                    role_ids: vec![mock_ids::ROLE_CARETAKER],
                },
            ],
        );
        guild_members.insert(
            mock_ids::GUILD_RUST_LAB,
            vec![GuildMemberSummary {
                user_id: mock_ids::USER_ALICE,
                nick: None,
                role_ids: vec![mock_ids::ROLE_RUST_CORE],
            }],
        );
        guild_members.insert(
            mock_ids::GUILD_DEMOSCENE,
            vec![GuildMemberSummary {
                user_id: mock_ids::USER_BOB,
                nick: Some("CRT Demomaker".to_string()),
                role_ids: vec![mock_ids::ROLE_DEMO_CODER],
            }],
        );

        let guild_folders = vec![
            GuildFolder {
                id: Some(101),
                name: Some("BBS & Systems".to_string()),
                color: Some(0x38BDF8),
                guild_ids: vec![mock_ids::GUILD_RETRONET, mock_ids::GUILD_RUST_LAB],
                collapsed: false,
            },
            GuildFolder {
                id: None,
                name: None,
                color: None,
                guild_ids: vec![mock_ids::GUILD_DEMOSCENE],
                collapsed: false,
            },
        ];

        let channel_list = vec![
            Channel {
                id: mock_ids::CH_CAT_LOBBY,
                guild_id: Some(mock_ids::GUILD_RETRONET),
                kind: ChannelKind::GuildCategory,
                name: "MAIN DECK".to_string(),
                topic: None,
                parent_id: None,
                position: 0,
                recipients: Vec::new(),
                owner_id: None,
                icon_hash: None,
                last_message_id: None,
                unread_count: 0,
                mention_count: 0,
                nsfw: false,
                slowmode_secs: 0,
                muted: false,
                last_ack_message_id: None,
            },
            Channel {
                id: mock_ids::CH_RETRONET_GENERAL,
                guild_id: Some(mock_ids::GUILD_RETRONET),
                kind: ChannelKind::GuildText,
                name: "general-bbs".to_string(),
                topic: Some(
                    "Welcome to RetroNet BBS! DAVE E2EE voice + Kitty/Sixel inline media enabled."
                        .to_string(),
                ),
                parent_id: Some(mock_ids::CH_CAT_LOBBY),
                position: 1,
                recipients: Vec::new(),
                owner_id: None,
                icon_hash: None,
                last_message_id: Some(Snowflake(400_000_000_000_000_005)),
                unread_count: 3,
                mention_count: 2,
                nsfw: false,
                slowmode_secs: 0,
                muted: false,
                last_ack_message_id: Some(Snowflake(400_000_000_000_000_001)),
            },
            Channel {
                id: mock_ids::CH_RETRONET_VOICE,
                guild_id: Some(mock_ids::GUILD_RETRONET),
                kind: ChannelKind::GuildVoice,
                name: "Lounge 56k".to_string(),
                topic: Some("48kHz Opus + DAVE E2EE".to_string()),
                parent_id: Some(mock_ids::CH_CAT_LOBBY),
                position: 2,
                recipients: Vec::new(),
                owner_id: None,
                icon_hash: None,
                last_message_id: None,
                unread_count: 0,
                mention_count: 0,
                nsfw: false,
                slowmode_secs: 0,
                muted: false,
                last_ack_message_id: None,
            },
            Channel {
                id: mock_ids::CH_RETRONET_STAGE,
                guild_id: Some(mock_ids::GUILD_RETRONET),
                kind: ChannelKind::GuildStageVoice,
                name: "Town Hall Stage".to_string(),
                topic: Some("Friday ANSI Art Jam".to_string()),
                parent_id: Some(mock_ids::CH_CAT_LOBBY),
                position: 3,
                recipients: Vec::new(),
                owner_id: None,
                icon_hash: None,
                last_message_id: None,
                unread_count: 0,
                mention_count: 0,
                nsfw: false,
                slowmode_secs: 0,
                muted: false,
                last_ack_message_id: None,
            },
            Channel {
                id: mock_ids::CH_RETRONET_THREAD,
                guild_id: Some(mock_ids::GUILD_RETRONET),
                kind: ChannelKind::PublicThread,
                name: "ANSI Theme Showcase".to_string(),
                topic: Some("Drop your btop .theme palettes here".to_string()),
                parent_id: Some(mock_ids::CH_RETRONET_GENERAL),
                position: 4,
                recipients: Vec::new(),
                owner_id: Some(mock_ids::USER_CAROL),
                icon_hash: None,
                last_message_id: Some(Snowflake(400_000_000_000_000_020)),
                unread_count: 1,
                mention_count: 0,
                nsfw: false,
                slowmode_secs: 0,
                muted: false,
                last_ack_message_id: None,
            },
            Channel {
                id: mock_ids::CH_RETRONET_MUTED_LOGS,
                guild_id: Some(mock_ids::GUILD_RETRONET),
                kind: ChannelKind::GuildText,
                name: "bot-firehose".to_string(),
                topic: Some("Automated build & telemetry firehose (muted)".to_string()),
                parent_id: Some(mock_ids::CH_CAT_LOBBY),
                position: 5,
                recipients: Vec::new(),
                owner_id: None,
                icon_hash: None,
                last_message_id: Some(Snowflake(400_000_000_000_000_025)),
                unread_count: 12,
                mention_count: 0,
                nsfw: false,
                slowmode_secs: 0,
                muted: true,
                last_ack_message_id: None,
            },
            Channel {
                id: mock_ids::CH_RUST_ASYNC,
                guild_id: Some(mock_ids::GUILD_RUST_LAB),
                kind: ChannelKind::GuildText,
                name: "async-runtime".to_string(),
                topic: Some("Tokio, zeroize, and lock-free ring buffers".to_string()),
                parent_id: None,
                position: 0,
                recipients: Vec::new(),
                owner_id: None,
                icon_hash: None,
                last_message_id: Some(Snowflake(400_000_000_000_000_030)),
                unread_count: 1,
                mention_count: 0,
                nsfw: false,
                slowmode_secs: 5,
                muted: false,
                last_ack_message_id: None,
            },
            Channel {
                id: mock_ids::CH_RUST_FORUM,
                guild_id: Some(mock_ids::GUILD_RUST_LAB),
                kind: ChannelKind::GuildForum,
                name: "rfc-board".to_string(),
                topic: Some("Architecture proposals & benchmarks".to_string()),
                parent_id: None,
                position: 1,
                recipients: Vec::new(),
                owner_id: None,
                icon_hash: None,
                last_message_id: None,
                unread_count: 0,
                mention_count: 0,
                nsfw: false,
                slowmode_secs: 0,
                muted: false,
                last_ack_message_id: None,
            },
            Channel {
                id: mock_ids::CH_DEMO_SHADERS,
                guild_id: Some(mock_ids::GUILD_DEMOSCENE),
                kind: ChannelKind::GuildText,
                name: "shaders-and-sixel".to_string(),
                topic: Some("CRT scanlines, Sixel dither, and 16x2 HD44780 hacks".to_string()),
                parent_id: None,
                position: 0,
                recipients: Vec::new(),
                owner_id: None,
                icon_hash: None,
                last_message_id: Some(Snowflake(400_000_000_000_000_040)),
                unread_count: 0,
                mention_count: 0,
                nsfw: false,
                slowmode_secs: 0,
                muted: false,
                last_ack_message_id: Some(Snowflake(400_000_000_000_000_040)),
            },
            Channel {
                id: mock_ids::CH_DM_ALICE,
                guild_id: None,
                kind: ChannelKind::DirectMessage,
                name: String::new(),
                topic: None,
                parent_id: None,
                position: 0,
                recipients: vec![user_alice.clone()],
                owner_id: None,
                icon_hash: None,
                last_message_id: Some(Snowflake(400_000_000_000_000_100)),
                unread_count: 1,
                mention_count: 1,
                nsfw: false,
                slowmode_secs: 0,
                muted: false,
                last_ack_message_id: None,
            },
            Channel {
                id: mock_ids::CH_GROUP_DM_OPS,
                guild_id: None,
                kind: ChannelKind::GroupDm,
                name: "BBS Ops War Room".to_string(),
                topic: Some("Live Group DM call + Go Live deck".to_string()),
                parent_id: None,
                position: 1,
                recipients: vec![user_alice.clone(), user_bob.clone(), user_carol.clone()],
                owner_id: Some(mock_ids::USER_SELF),
                icon_hash: None,
                last_message_id: Some(Snowflake(400_000_000_000_000_201)),
                unread_count: 2,
                mention_count: 1,
                nsfw: false,
                slowmode_secs: 0,
                muted: false,
                last_ack_message_id: Some(Snowflake(400_000_000_000_000_200)),
            },
            Channel {
                id: mock_ids::CH_DM_BOB,
                guild_id: None,
                kind: ChannelKind::DirectMessage,
                name: String::new(),
                topic: None,
                parent_id: None,
                position: 2,
                recipients: vec![user_bob.clone()],
                owner_id: None,
                icon_hash: None,
                last_message_id: Some(Snowflake(400_000_000_000_000_101)),
                unread_count: 1,
                mention_count: 0,
                nsfw: false,
                slowmode_secs: 0,
                muted: false,
                last_ack_message_id: None,
            },
            Channel {
                id: mock_ids::CH_DM_CAROL,
                guild_id: None,
                kind: ChannelKind::DirectMessage,
                name: String::new(),
                topic: None,
                parent_id: None,
                position: 3,
                recipients: vec![user_carol.clone()],
                owner_id: None,
                icon_hash: None,
                last_message_id: Some(Snowflake(400_000_000_000_000_102)),
                unread_count: 0,
                mention_count: 0,
                nsfw: false,
                slowmode_secs: 0,
                muted: false,
                last_ack_message_id: Some(Snowflake(400_000_000_000_000_102)),
            },
            Channel {
                id: mock_ids::CH_DM_DAVE,
                guild_id: None,
                kind: ChannelKind::DirectMessage,
                name: String::new(),
                topic: None,
                parent_id: None,
                position: 4,
                recipients: vec![user_dave.clone()],
                owner_id: None,
                icon_hash: None,
                last_message_id: None,
                unread_count: 0,
                mention_count: 0,
                nsfw: false,
                slowmode_secs: 0,
                muted: true,
                last_ack_message_id: None,
            },
        ];

        let mut channels = HashMap::new();
        for ch in channel_list {
            channels.insert(ch.id, ch);
        }

        // Build deterministic 64-point voice message waveform (0..=255).
        let waveform: Vec<u8> = (0..64)
            .map(|i| {
                let phase = (i as f32) * 0.28;
                let envelope = ((i as f32) / 63.0 * std::f32::consts::PI).sin();
                let amp = (phase.sin().abs() * 0.7 + 0.3) * envelope;
                (amp * 240.0).round().clamp(12.0, 255.0) as u8
            })
            .collect();

        let mut messages: HashMap<Snowflake, Vec<Message>> = HashMap::new();

        // #general-bbs rich showcase messages
        messages.insert(
            mock_ids::CH_RETRONET_GENERAL,
            vec![
                Message {
                    id: Snowflake(400_000_000_000_000_001),
                    channel_id: mock_ids::CH_RETRONET_GENERAL,
                    guild_id: Some(mock_ids::GUILD_RETRONET),
                    author: user_bot.clone(),
                    content: "NODE 01 ONLINE — Telemetry & multi-column inline embed test:".to_string(),
                    timestamp: "2026-11-26T09:00:00Z".to_string(),
                    edited_timestamp: None,
                    pinned: true,
                    mention_everyone: false,
                    mentions: Vec::new(),
                    reply_to: None,
                    attachments: Vec::new(),
                    embeds: vec![Embed {
                        kind: EmbedKind::Rich,
                        title: Some("RetroNet Node 01 Diagnostics".to_string()),
                        url: Some("https://example.invalid/retronet/status".to_string()),
                        description: Some(
                            "Multi-column `inline` grid adapts seamlessly from 1 column (Compact) to 3 columns (Wide/4K).".to_string(),
                        ),
                        color: Some(0x38BDF8),
                        timestamp: Some("2026-11-26T09:00:00Z".to_string()),
                        provider: Some(EmbedProvider {
                            name: Some("RetroNet Telemetry".to_string()),
                            url: Some("https://example.invalid/retronet".to_string()),
                        }),
                        author: Some(EmbedAuthor {
                            name: "BBS Daemon".to_string(),
                            url: None,
                            icon_url: None,
                            proxy_icon_url: None,
                        }),
                        footer: Some(EmbedFooter {
                            text: "DAVE E2EE Ready • 48kHz Opus".to_string(),
                            icon_url: None,
                            proxy_icon_url: None,
                        }),
                        image: None,
                        thumbnail: Some(EmbedMedia {
                            url: "https://example.invalid/mock-media/node01-thumb.png".to_string(),
                            proxy_url: None,
                            width: Some(128),
                            height: Some(128),
                        }),
                        video: None,
                        fields: vec![
                            EmbedField {
                                name: "Gateway RTT".to_string(),
                                value: "24 ms".to_string(),
                                inline: true,
                            },
                            EmbedField {
                                name: "DAVE Epoch".to_string(),
                                value: "#4 (Verified)".to_string(),
                                inline: true,
                            },
                            EmbedField {
                                name: "Active Calls".to_string(),
                                value: "2 (Guild + Group DM)".to_string(),
                                inline: true,
                            },
                            EmbedField {
                                name: "Supported Viewports".to_string(),
                                value: "16x2 HD44780 LCD -> 640x180 4K Ultrawide Cockpit".to_string(),
                                inline: false,
                            },
                        ],
                    }],
                    reactions: vec![
                        Reaction {
                            emoji_id: None,
                            emoji_name: "⚡".to_string(),
                            animated: false,
                            count: 5,
                            me: true,
                        },
                        Reaction {
                            emoji_id: None,
                            emoji_name: "🔥".to_string(),
                            animated: false,
                            count: 3,
                            me: false,
                        },
                    ],
                    poll: None,
                    flags: 0,
                    nonce: None,
                    author_nick: None,
                    author_role_ids: vec![mock_ids::ROLE_DAEMON],
                    mention_roles: vec![mock_ids::ROLE_SYSOP],
                },
                Message {
                    id: Snowflake(400_000_000_000_000_002),
                    channel_id: mock_ids::CH_RETRONET_GENERAL,
                    guild_id: Some(mock_ids::GUILD_RETRONET),
                    author: user_alice.clone(),
                    content: "Check out this link-card preview & CJK/wide-glyph wrap test: 日本語テスト 终端自适应布局 🚀\n```rust\nlet profile = geom.layout_profile();\n```".to_string(),
                    timestamp: "2026-11-26T09:04:12Z".to_string(),
                    edited_timestamp: None,
                    pinned: false,
                    mention_everyone: false,
                    mentions: vec![mock_ids::USER_SELF],
                    reply_to: Some(MessageReference {
                        message_id: Some(Snowflake(400_000_000_000_000_001)),
                        channel_id: Some(mock_ids::CH_RETRONET_GENERAL),
                        guild_id: Some(mock_ids::GUILD_RETRONET),
                        author_name: Some("BBS Daemon".to_string()),
                        snippet: Some("NODE 01 ONLINE — Telemetry & multi-column inline embed test:".to_string()),
                    }),
                    attachments: Vec::new(),
                    embeds: vec![Embed {
                        kind: EmbedKind::Article,
                        title: Some("Building Zero-Drift Terminal Layouts".to_string()),
                        url: Some("https://example.invalid/articles/zero-drift-tui".to_string()),
                        description: Some(
                            "How content-addressed ScrollAnchor preserves exact reading position across 640x180 <-> 16x2 resizes.".to_string(),
                        ),
                        color: Some(0x50FA7B),
                        provider: Some(EmbedProvider {
                            name: Some("Systems Journal".to_string()),
                            url: None,
                        }),
                        thumbnail: Some(EmbedMedia {
                            url: "https://example.invalid/mock-media/article-card.png".to_string(),
                            proxy_url: None,
                            width: Some(256),
                            height: Some(144),
                        }),
                        ..Default::default()
                    }],
                    reactions: Vec::new(),
                    poll: None,
                    flags: 0,
                    nonce: None,
                    author_nick: Some("Modem Wiz".to_string()),
                    author_role_ids: vec![mock_ids::ROLE_NETRUNNER],
                    mention_roles: Vec::new(),
                },
                Message {
                    id: Snowflake(400_000_000_000_000_003),
                    channel_id: mock_ids::CH_RETRONET_GENERAL,
                    guild_id: Some(mock_ids::GUILD_RETRONET),
                    author: user_carol.clone(),
                    content: String::new(),
                    timestamp: "2026-11-26T09:07:45Z".to_string(),
                    edited_timestamp: None,
                    pinned: false,
                    mention_everyone: false,
                    mentions: Vec::new(),
                    reply_to: None,
                    attachments: vec![Attachment {
                        id: Snowflake(500_000_000_000_000_001),
                        filename: "voice-message.ogg".to_string(),
                        content_type: Some("audio/ogg".to_string()),
                        size_bytes: 28_412,
                        url: "https://example.invalid/mock-cdn/voice-message.ogg?ex=67a00000".to_string(),
                        proxy_url: None,
                        width: None,
                        height: None,
                        ephemeral: false,
                        expires_at_unix: Some(1_893_456_000),
                        voice_message: Some(VoiceMessageMeta {
                            duration_secs: 6.4,
                            waveform,
                        }),
                    }],
                    embeds: Vec::new(),
                    reactions: vec![Reaction {
                        emoji_id: None,
                        emoji_name: "🎧".to_string(),
                        animated: false,
                        count: 2,
                        me: false,
                    }],
                    poll: None,
                    flags: Message::FLAG_IS_VOICE_MESSAGE,
                    nonce: None,
                    author_nick: Some("ANSI Virtuoso".to_string()),
                    author_role_ids: vec![mock_ids::ROLE_ARTIST],
                    mention_roles: Vec::new(),
                },
                Message {
                    id: Snowflake(400_000_000_000_000_004),
                    channel_id: mock_ids::CH_RETRONET_GENERAL,
                    guild_id: Some(mock_ids::GUILD_RETRONET),
                    author: user_bob.clone(),
                    content: "Vote on tonight's default CRT phosphor theme:".to_string(),
                    timestamp: "2026-11-26T09:10:00Z".to_string(),
                    edited_timestamp: None,
                    pinned: false,
                    mention_everyone: false,
                    mentions: Vec::new(),
                    reply_to: None,
                    attachments: Vec::new(),
                    embeds: Vec::new(),
                    reactions: Vec::new(),
                    poll: Some(PollSummary {
                        question: "Default Friday Night BBS Theme?".to_string(),
                        answers: vec![
                            PollAnswerSummary {
                                id: 1,
                                text: "ANSI-BBS Royal".to_string(),
                                emoji: Some("👑".to_string()),
                                votes: 14,
                                me_voted: true,
                            },
                            PollAnswerSummary {
                                id: 2,
                                text: "Amber Phosphor".to_string(),
                                emoji: Some("🟠".to_string()),
                                votes: 11,
                                me_voted: false,
                            },
                            PollAnswerSummary {
                                id: 3,
                                text: "P1 Green CRT".to_string(),
                                emoji: Some("🟢".to_string()),
                                votes: 9,
                                me_voted: false,
                            },
                        ],
                        total_votes: 34,
                        allow_multiselect: false,
                        expired: false,
                        expires_at: Some("2026-11-27T09:10:00Z".to_string()),
                    }),
                    flags: 0,
                    nonce: None,
                    author_nick: None,
                    author_role_ids: vec![mock_ids::ROLE_CARETAKER],
                    mention_roles: Vec::new(),
                },
            ],
        );

        messages.insert(
            mock_ids::CH_RETRONET_THREAD,
            vec![Message {
                id: Snowflake(400_000_000_000_000_020),
                channel_id: mock_ids::CH_RETRONET_THREAD,
                guild_id: Some(mock_ids::GUILD_RETRONET),
                author: user_carol.clone(),
                content: "Dropped a custom btop .theme file in ~/.config/bbscord/themes/ — press F2 to hot-swap!".to_string(),
                timestamp: "2026-11-26T09:12:00Z".to_string(),
                edited_timestamp: None,
                pinned: false,
                mention_everyone: false,
                mentions: Vec::new(),
                reply_to: None,
                attachments: Vec::new(),
                embeds: Vec::new(),
                reactions: Vec::new(),
                poll: None,
                flags: 0,
                nonce: None,
                author_nick: Some("ANSI Virtuoso".to_string()),
                author_role_ids: vec![mock_ids::ROLE_ARTIST],
                mention_roles: Vec::new(),
            }],
        );

        messages.insert(
            mock_ids::CH_RETRONET_MUTED_LOGS,
            vec![Message {
                id: Snowflake(400_000_000_000_000_025),
                channel_id: mock_ids::CH_RETRONET_MUTED_LOGS,
                guild_id: Some(mock_ids::GUILD_RETRONET),
                author: user_bot.clone(),
                content: "[CI] Nightly snapshot build succeeded (142/142 tests passing).".to_string(),
                timestamp: "2026-11-26T09:13:30Z".to_string(),
                edited_timestamp: None,
                pinned: false,
                mention_everyone: false,
                mentions: Vec::new(),
                reply_to: None,
                attachments: Vec::new(),
                embeds: Vec::new(),
                reactions: Vec::new(),
                poll: None,
                flags: 0,
                nonce: None,
                author_nick: None,
                author_role_ids: vec![mock_ids::ROLE_DAEMON],
                mention_roles: Vec::new(),
            }],
        );

        messages.insert(
            mock_ids::CH_RUST_ASYNC,
            vec![Message {
                id: Snowflake(400_000_000_000_000_030),
                channel_id: mock_ids::CH_RUST_ASYNC,
                guild_id: Some(mock_ids::GUILD_RUST_LAB),
                author: user_alice.clone(),
                content: "Zeroize + SecretToken Debug redaction verified across all workspace crates.".to_string(),
                timestamp: "2026-11-26T09:15:00Z".to_string(),
                edited_timestamp: None,
                pinned: false,
                mention_everyone: false,
                mentions: Vec::new(),
                reply_to: None,
                attachments: Vec::new(),
                embeds: Vec::new(),
                reactions: Vec::new(),
                poll: None,
                flags: 0,
                nonce: None,
                author_nick: None,
                author_role_ids: vec![mock_ids::ROLE_RUST_CORE],
                mention_roles: Vec::new(),
            }],
        );

        messages.insert(
            mock_ids::CH_DEMO_SHADERS,
            vec![Message {
                id: Snowflake(400_000_000_000_000_040),
                channel_id: mock_ids::CH_DEMO_SHADERS,
                guild_id: Some(mock_ids::GUILD_DEMOSCENE),
                author: user_bob.clone(),
                content: "Test card frame renders in both Kitty Placeholders and Sixel inside tmux!".to_string(),
                timestamp: "2026-11-26T09:18:00Z".to_string(),
                edited_timestamp: None,
                pinned: false,
                mention_everyone: false,
                mentions: Vec::new(),
                reply_to: None,
                attachments: Vec::new(),
                embeds: Vec::new(),
                reactions: Vec::new(),
                poll: None,
                flags: 0,
                nonce: None,
                author_nick: Some("CRT Demomaker".to_string()),
                author_role_ids: vec![mock_ids::ROLE_DEMO_CODER],
                mention_roles: Vec::new(),
            }],
        );

        messages.insert(
            mock_ids::CH_DM_ALICE,
            vec![Message {
                id: Snowflake(400_000_000_000_000_100),
                channel_id: mock_ids::CH_DM_ALICE,
                guild_id: None,
                author: user_alice.clone(),
                content: "Hey SysOp, hop into the BBS Ops War Room Group DM call when ready!".to_string(),
                timestamp: "2026-11-26T09:20:00Z".to_string(),
                edited_timestamp: None,
                pinned: false,
                mention_everyone: false,
                mentions: vec![mock_ids::USER_SELF],
                reply_to: None,
                attachments: Vec::new(),
                embeds: Vec::new(),
                reactions: Vec::new(),
                poll: None,
                flags: 0,
                nonce: None,
                author_nick: None,
                author_role_ids: Vec::new(),
                mention_roles: Vec::new(),
            }],
        );

        messages.insert(
            mock_ids::CH_DM_BOB,
            vec![Message {
                id: Snowflake(400_000_000_000_000_101),
                channel_id: mock_ids::CH_DM_BOB,
                guild_id: None,
                author: user_bob.clone(),
                content: "Stepping away for coffee — ping me if the PipeWire quantum test finishes.".to_string(),
                timestamp: "2026-11-26T09:21:00Z".to_string(),
                edited_timestamp: None,
                pinned: false,
                mention_everyone: false,
                mentions: Vec::new(),
                reply_to: None,
                attachments: Vec::new(),
                embeds: Vec::new(),
                reactions: Vec::new(),
                poll: None,
                flags: 0,
                nonce: None,
                author_nick: None,
                author_role_ids: Vec::new(),
                mention_roles: Vec::new(),
            }],
        );

        messages.insert(
            mock_ids::CH_DM_CAROL,
            vec![Message {
                id: Snowflake(400_000_000_000_000_102),
                channel_id: mock_ids::CH_DM_CAROL,
                guild_id: None,
                author: user_carol.clone(),
                content: "In focus mode rendering ANSI banners — back online shortly.".to_string(),
                timestamp: "2026-11-26T09:21:30Z".to_string(),
                edited_timestamp: None,
                pinned: false,
                mention_everyone: false,
                mentions: Vec::new(),
                reply_to: None,
                attachments: Vec::new(),
                embeds: Vec::new(),
                reactions: Vec::new(),
                poll: None,
                flags: 0,
                nonce: None,
                author_nick: None,
                author_role_ids: Vec::new(),
                mention_roles: Vec::new(),
            }],
        );

        messages.insert(
            mock_ids::CH_GROUP_DM_OPS,
            vec![
                Message {
                    id: Snowflake(400_000_000_000_000_200),
                    channel_id: mock_ids::CH_GROUP_DM_OPS,
                    guild_id: None,
                    author: user_bob.clone(),
                    content: "Group DM call is live with DAVE E2EE! Sharing my terminal compositor window now.".to_string(),
                    timestamp: "2026-11-26T09:22:00Z".to_string(),
                    edited_timestamp: None,
                    pinned: false,
                    mention_everyone: false,
                    mentions: Vec::new(),
                    reply_to: None,
                    attachments: Vec::new(),
                    embeds: Vec::new(),
                    reactions: Vec::new(),
                    poll: None,
                    flags: 0,
                    nonce: None,
                    author_nick: None,
                    author_role_ids: Vec::new(),
                    mention_roles: Vec::new(),
                },
                Message {
                    id: Snowflake(400_000_000_000_000_201),
                    channel_id: mock_ids::CH_GROUP_DM_OPS,
                    guild_id: None,
                    author: user_alice.clone(),
                    content: "@SysOp ringing you in — click Join Call or press Ctrl+V!".to_string(),
                    timestamp: "2026-11-26T09:23:10Z".to_string(),
                    edited_timestamp: None,
                    pinned: false,
                    mention_everyone: false,
                    mentions: vec![mock_ids::USER_SELF],
                    reply_to: None,
                    attachments: Vec::new(),
                    embeds: Vec::new(),
                    reactions: Vec::new(),
                    poll: None,
                    flags: 0,
                    nonce: None,
                    author_nick: None,
                    author_role_ids: Vec::new(),
                    mention_roles: Vec::new(),
                },
            ],
        );

        // Pre-seed an active incoming/connected call state on the Group DM.
        let mut call_rings = HashMap::new();
        call_rings.insert(
            mock_ids::CH_GROUP_DM_OPS,
            CallRingState::IncomingRinging {
                channel_id: mock_ids::CH_GROUP_DM_OPS,
                caller_id: mock_ids::USER_ALICE,
                caller_name: user_alice.display_name().to_string(),
                is_group_dm: true,
            },
        );

        // Pre-seed visible voice channel occupants so sidebar rosters work in `--mock-demo`.
        let voice_rosters = vec![
            (
                Some(mock_ids::GUILD_RETRONET),
                mock_ids::CH_RETRONET_VOICE,
                vec![
                    VoiceParticipant {
                        user_id: mock_ids::USER_ALICE,
                        display_name: "Modem Wiz".to_string(),
                        speaking: true,
                        self_mute: false,
                        self_deaf: false,
                        server_mute: false,
                        server_deaf: false,
                        volume: 100,
                        rms_db: -20.0,
                        dave_verified: true,
                        streaming: true,
                    },
                    VoiceParticipant {
                        user_id: mock_ids::USER_BOB,
                        display_name: "CRT Hacker".to_string(),
                        speaking: false,
                        self_mute: true,
                        self_deaf: false,
                        server_mute: false,
                        server_deaf: false,
                        volume: 100,
                        rms_db: VoiceParticipant::MIN_RMS_DB,
                        dave_verified: true,
                        streaming: false,
                    },
                ],
            ),
            (
                None,
                mock_ids::CH_GROUP_DM_OPS,
                vec![
                    VoiceParticipant {
                        user_id: mock_ids::USER_ALICE,
                        display_name: "Modem Wizard".to_string(),
                        speaking: true,
                        self_mute: false,
                        self_deaf: false,
                        server_mute: false,
                        server_deaf: false,
                        volume: 100,
                        rms_db: -22.0,
                        dave_verified: true,
                        streaming: false,
                    },
                    VoiceParticipant {
                        user_id: mock_ids::USER_BOB,
                        display_name: "CRT Hacker".to_string(),
                        speaking: false,
                        self_mute: false,
                        self_deaf: false,
                        server_mute: false,
                        server_deaf: false,
                        volume: 100,
                        rms_db: -44.0,
                        dave_verified: true,
                        streaming: true,
                    },
                ],
            ),
        ];

        Self {
            state: RwLock::new(MockDiscordState {
                current_user: user_self,
                users,
                guilds,
                guild_folders,
                guild_members,
                channels,
                messages,
                call_rings,
                voice_rosters,
            }),
            next_id: AtomicU64::new(900_000_000_000_000_000),
            events,
        }
    }

    /// Query the current call ring state for a DM or Group DM channel.
    pub fn call_ring_state(&self, channel_id: Snowflake) -> CoreResult<CallRingState> {
        let guard = self
            .state
            .read()
            .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
        Ok(guard
            .call_rings
            .get(&channel_id)
            .cloned()
            .unwrap_or(CallRingState::Idle))
    }

    /// Return seeded guild member summaries (nicknames and role IDs) for `guild_id`.
    pub fn guild_members(&self, guild_id: Snowflake) -> CoreResult<Vec<GuildMemberSummary>> {
        let guard = self
            .state
            .read()
            .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
        Ok(guard
            .guild_members
            .get(&guild_id)
            .cloned()
            .unwrap_or_default())
    }

    /// Return seeded voice channel occupant rosters `(guild_id, channel_id, participants)`.
    pub fn voice_rosters(&self) -> CoreResult<Vec<MockVoiceRoster>> {
        let guard = self
            .state
            .read()
            .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
        Ok(guard.voice_rosters.clone())
    }

    fn alloc_snowflake(&self) -> Snowflake {
        Snowflake(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn emit(&self, event: BackendEvent) {
        let _ = self.events.send(event);
    }
}

#[async_trait]
impl DiscordBackend for MockDiscordBackend {
    async fn current_user(&self) -> CoreResult<User> {
        let guard = self
            .state
            .read()
            .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
        Ok(guard.current_user.clone())
    }

    async fn list_guilds(&self) -> CoreResult<(Vec<Guild>, Vec<GuildFolder>)> {
        let guard = self
            .state
            .read()
            .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
        Ok((guard.guilds.clone(), guard.guild_folders.clone()))
    }

    async fn list_channels(&self, guild_id: Snowflake) -> CoreResult<Vec<Channel>> {
        let guard = self
            .state
            .read()
            .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
        let mut out: Vec<Channel> = guard
            .channels
            .values()
            .filter(|ch| ch.guild_id == Some(guild_id))
            .cloned()
            .collect();
        out.sort_by_key(|ch| (ch.position, ch.id));
        Ok(out)
    }

    async fn list_dm_channels(&self) -> CoreResult<Vec<Channel>> {
        let guard = self
            .state
            .read()
            .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
        let mut out: Vec<Channel> = guard
            .channels
            .values()
            .filter(|ch| ch.guild_id.is_none() && ch.kind.is_dm())
            .cloned()
            .collect();
        out.sort_by_key(|ch| (ch.position, ch.id));
        Ok(out)
    }

    async fn fetch_messages(
        &self,
        channel_id: Snowflake,
        before: Option<Snowflake>,
        limit: u8,
    ) -> CoreResult<Vec<Message>> {
        let guard = self
            .state
            .read()
            .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
        if !guard.channels.contains_key(&channel_id) {
            return Err(CoreError::NotFound(format!("channel {channel_id}")));
        }
        let all = guard
            .messages
            .get(&channel_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let filtered: Vec<Message> = all
            .iter()
            .filter(|m| before.is_none_or(|b| m.id < b))
            .cloned()
            .collect();
        let take = usize::from(limit.max(1));
        let start = filtered.len().saturating_sub(take);
        Ok(filtered[start..].to_vec())
    }

    async fn send_message(
        &self,
        channel_id: Snowflake,
        content: &str,
        reply_to: Option<Snowflake>,
    ) -> CoreResult<Message> {
        self.send_message_with_mention(channel_id, content, reply_to, true)
            .await
    }

    async fn send_message_with_mention(
        &self,
        channel_id: Snowflake,
        content: &str,
        reply_to: Option<Snowflake>,
        mention_replied_user: bool,
    ) -> CoreResult<Message> {
        let msg_id = self.alloc_snowflake();
        let msg = {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
            let guild_id = guard
                .channels
                .get(&channel_id)
                .ok_or_else(|| CoreError::NotFound(format!("channel {channel_id}")))?
                .guild_id;

            let mut mentions = Vec::new();
            let reply_ref = reply_to.map(|target_id| {
                let existing = guard
                    .messages
                    .get(&channel_id)
                    .and_then(|list| list.iter().find(|m| m.id == target_id));
                if mention_replied_user
                    && let Some(target_msg) = existing
                    && target_msg.author.id != guard.current_user.id
                {
                    mentions.push(target_msg.author.id);
                }
                MessageReference {
                    message_id: Some(target_id),
                    channel_id: Some(channel_id),
                    guild_id,
                    author_name: existing.map(|m| m.author.display_name().to_string()),
                    snippet: existing.map(|m| m.content.chars().take(64).collect()),
                }
            });

            let (author_nick, author_role_ids) = guild_id
                .and_then(|gid| guard.guild_members.get(&gid))
                .and_then(|members| {
                    members
                        .iter()
                        .find(|m| m.user_id == guard.current_user.id)
                        .map(|m| (m.nick.clone(), m.role_ids.clone()))
                })
                .unwrap_or((None, Vec::new()));

            let msg = Message {
                id: msg_id,
                channel_id,
                guild_id,
                author: guard.current_user.clone(),
                content: content.to_string(),
                timestamp: "2026-11-26T09:30:00Z".to_string(),
                edited_timestamp: None,
                pinned: false,
                mention_everyone: false,
                mentions,
                reply_to: reply_ref,
                attachments: Vec::new(),
                embeds: Vec::new(),
                reactions: Vec::new(),
                poll: None,
                flags: 0,
                nonce: None,
                author_nick,
                author_role_ids,
                mention_roles: Vec::new(),
            };

            guard.messages.entry(channel_id).or_default().push(msg.clone());
            if let Some(ch) = guard.channels.get_mut(&channel_id) {
                ch.last_message_id = Some(msg_id);
            }
            msg
        };

        self.emit(BackendEvent::MessageCreated(msg.clone()));
        Ok(msg)
    }

    async fn ack_message(&self, channel_id: Snowflake, message_id: Snowflake) -> CoreResult<()> {
        {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
            let guild_id = {
                let ch = guard
                    .channels
                    .get_mut(&channel_id)
                    .ok_or_else(|| CoreError::NotFound(format!("channel {channel_id}")))?;
                ch.last_ack_message_id = Some(message_id);
                ch.unread_count = 0;
                ch.mention_count = 0;
                ch.guild_id
            };
            if let Some(gid) = guild_id {
                let (unread, mentions) = guard
                    .channels
                    .values()
                    .filter(|c| c.guild_id == Some(gid) && !c.muted)
                    .fold((false, 0u32), |(u, m), c| {
                        (u || c.unread_count > 0, m.saturating_add(c.mention_count))
                    });
                if let Some(g) = guard.guilds.iter_mut().find(|g| g.id == gid) {
                    g.unread = unread;
                    g.mention_count = mentions;
                }
            }
        }
        self.emit(BackendEvent::MessageAcked {
            channel_id,
            last_message_id: message_id,
            mention_count: 0,
        });
        Ok(())
    }

    async fn edit_message(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        content: &str,
    ) -> CoreResult<Message> {
        let updated = {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
            let list = guard
                .messages
                .get_mut(&channel_id)
                .ok_or_else(|| CoreError::NotFound(format!("channel {channel_id}")))?;
            let msg = list
                .iter_mut()
                .find(|m| m.id == message_id)
                .ok_or_else(|| CoreError::NotFound(format!("message {message_id}")))?;
            msg.content = content.to_string();
            msg.edited_timestamp = Some("2026-11-26T09:31:00Z".to_string());
            msg.clone()
        };

        self.emit(BackendEvent::MessageUpdated(updated.clone()));
        Ok(updated)
    }

    async fn delete_message(&self, channel_id: Snowflake, message_id: Snowflake) -> CoreResult<()> {
        {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
            let list = guard
                .messages
                .get_mut(&channel_id)
                .ok_or_else(|| CoreError::NotFound(format!("channel {channel_id}")))?;
            let before_len = list.len();
            list.retain(|m| m.id != message_id);
            if list.len() == before_len {
                return Err(CoreError::NotFound(format!("message {message_id}")));
            }
        }
        self.emit(BackendEvent::MessageDeleted {
            channel_id,
            message_id,
        });
        Ok(())
    }

    async fn add_reaction(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: &str,
    ) -> CoreResult<()> {
        let reaction = {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
            let list = guard
                .messages
                .get_mut(&channel_id)
                .ok_or_else(|| CoreError::NotFound(format!("channel {channel_id}")))?;
            let msg = list
                .iter_mut()
                .find(|m| m.id == message_id)
                .ok_or_else(|| CoreError::NotFound(format!("message {message_id}")))?;

            if let Some(existing) = msg.reactions.iter_mut().find(|r| r.emoji_name == emoji) {
                if !existing.me {
                    existing.count = existing.count.saturating_add(1);
                    existing.me = true;
                }
                existing.clone()
            } else {
                let r = Reaction {
                    emoji_id: None,
                    emoji_name: emoji.to_string(),
                    animated: false,
                    count: 1,
                    me: true,
                };
                msg.reactions.push(r.clone());
                r
            }
        };

        self.emit(BackendEvent::ReactionAdded {
            channel_id,
            message_id,
            reaction,
        });
        Ok(())
    }

    async fn remove_reaction(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: &str,
    ) -> CoreResult<()> {
        {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
            let list = guard
                .messages
                .get_mut(&channel_id)
                .ok_or_else(|| CoreError::NotFound(format!("channel {channel_id}")))?;
            let msg = list
                .iter_mut()
                .find(|m| m.id == message_id)
                .ok_or_else(|| CoreError::NotFound(format!("message {message_id}")))?;
            if let Some(existing) = msg.reactions.iter_mut().find(|r| r.emoji_name == emoji)
                && existing.me
            {
                existing.count = existing.count.saturating_sub(1);
                existing.me = false;
            }
            msg.reactions.retain(|r| r.count > 0);
        }

        self.emit(BackendEvent::ReactionRemoved {
            channel_id,
            message_id,
            emoji_id: None,
            emoji_name: emoji.to_string(),
            by_me: true,
        });
        Ok(())
    }

    async fn send_typing(&self, channel_id: Snowflake) -> CoreResult<()> {
        let user = self.current_user().await?;
        self.emit(BackendEvent::TypingStarted {
            channel_id,
            user_id: user.id,
            display_name: user.display_name().to_string(),
            expires_at_unix: 1_893_456_010,
        });
        Ok(())
    }

    async fn create_group_dm(
        &self,
        recipient_ids: &[Snowflake],
        name: Option<&str>,
    ) -> CoreResult<Channel> {
        let channel_id = self.alloc_snowflake();
        let ch = {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
            let recipients: Vec<User> = recipient_ids
                .iter()
                .map(|id| {
                    guard.users.get(id).cloned().unwrap_or_else(|| User {
                        id: *id,
                        username: format!("user_{id}"),
                        global_name: None,
                        discriminator: None,
                        avatar_hash: None,
                        bot: false,
                        accent_color: None,
                        presence: PresenceStatus::Offline,
                    })
                })
                .collect();

            let ch = Channel {
                id: channel_id,
                guild_id: None,
                kind: ChannelKind::GroupDm,
                name: name.unwrap_or("").to_string(),
                topic: None,
                parent_id: None,
                position: guard.channels.len() as i32,
                recipients,
                owner_id: Some(guard.current_user.id),
                icon_hash: None,
                last_message_id: None,
                unread_count: 0,
                mention_count: 0,
                nsfw: false,
                slowmode_secs: 0,
                muted: false,
                last_ack_message_id: None,
            };
            guard.channels.insert(channel_id, ch.clone());
            guard.messages.entry(channel_id).or_default();
            ch
        };

        self.emit(BackendEvent::ChannelUpserted(ch.clone()));
        Ok(ch)
    }

    async fn add_group_dm_recipient(
        &self,
        channel_id: Snowflake,
        user_id: Snowflake,
    ) -> CoreResult<Channel> {
        let ch = {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
            let user = guard.users.get(&user_id).cloned().unwrap_or_else(|| User {
                id: user_id,
                username: format!("user_{user_id}"),
                global_name: None,
                discriminator: None,
                avatar_hash: None,
                bot: false,
                accent_color: None,
                presence: PresenceStatus::Online,
            });
            let ch = guard
                .channels
                .get_mut(&channel_id)
                .ok_or_else(|| CoreError::NotFound(format!("group dm {channel_id}")))?;
            if !ch.kind.is_group_dm() {
                return Err(CoreError::Unsupported(
                    "channel is not a Group DM".to_string(),
                ));
            }
            if !ch.recipients.iter().any(|r| r.id == user_id) {
                ch.recipients.push(user);
            }
            ch.clone()
        };

        self.emit(BackendEvent::ChannelUpserted(ch.clone()));
        Ok(ch)
    }

    async fn remove_group_dm_recipient(
        &self,
        channel_id: Snowflake,
        user_id: Snowflake,
    ) -> CoreResult<Channel> {
        let ch = {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
            let ch = guard
                .channels
                .get_mut(&channel_id)
                .ok_or_else(|| CoreError::NotFound(format!("group dm {channel_id}")))?;
            if !ch.kind.is_group_dm() {
                return Err(CoreError::Unsupported(
                    "channel is not a Group DM".to_string(),
                ));
            }
            ch.recipients.retain(|r| r.id != user_id);
            ch.clone()
        };

        self.emit(BackendEvent::ChannelUpserted(ch.clone()));
        Ok(ch)
    }

    async fn rename_group_dm(&self, channel_id: Snowflake, name: &str) -> CoreResult<Channel> {
        let ch = {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
            let ch = guard
                .channels
                .get_mut(&channel_id)
                .ok_or_else(|| CoreError::NotFound(format!("group dm {channel_id}")))?;
            if !ch.kind.is_group_dm() {
                return Err(CoreError::Unsupported(
                    "channel is not a Group DM".to_string(),
                ));
            }
            ch.name = name.to_string();
            ch.clone()
        };

        self.emit(BackendEvent::ChannelUpserted(ch.clone()));
        Ok(ch)
    }

    async fn leave_group_dm(&self, channel_id: Snowflake) -> CoreResult<()> {
        {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
            let Some(ch) = guard.channels.get(&channel_id) else {
                return Err(CoreError::NotFound(format!("group dm {channel_id}")));
            };
            if !ch.kind.is_group_dm() {
                return Err(CoreError::Unsupported(
                    "channel is not a Group DM".to_string(),
                ));
            }
            guard.channels.remove(&channel_id);
            guard.messages.remove(&channel_id);
            guard.call_rings.remove(&channel_id);
        }

        self.emit(BackendEvent::ChannelDeleted {
            guild_id: None,
            channel_id,
        });
        Ok(())
    }

    async fn ring_call(
        &self,
        channel_id: Snowflake,
        recipient_ids: Option<&[Snowflake]>,
    ) -> CoreResult<CallRingState> {
        let ring = {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
            let ch = guard
                .channels
                .get(&channel_id)
                .ok_or_else(|| CoreError::NotFound(format!("channel {channel_id}")))?;
            if !ch.kind.is_dm() {
                return Err(CoreError::Unsupported(
                    "call ringing is only valid for DM and Group DM channels".to_string(),
                ));
            }
            let targets = match recipient_ids {
                Some(ids) => ids.to_vec(),
                None => ch.recipients.iter().map(|u| u.id).collect(),
            };
            let state = CallRingState::OutgoingRinging {
                channel_id,
                target_user_ids: targets,
            };
            guard.call_rings.insert(channel_id, state.clone());
            state
        };

        self.emit(BackendEvent::CallRingUpdated(ring.clone()));
        Ok(ring)
    }

    async fn stop_call_ring(&self, channel_id: Snowflake) -> CoreResult<CallRingState> {
        {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock state poisoned".to_string()))?;
            guard.call_rings.insert(channel_id, CallRingState::Idle);
        }
        self.emit(BackendEvent::CallRingUpdated(CallRingState::Idle));
        Ok(CallRingState::Idle)
    }

    async fn refresh_attachment_url(
        &self,
        original_url: &str,
    ) -> CoreResult<RefreshedAttachmentUrl> {
        let base = original_url.split('?').next().unwrap_or(original_url);
        let refreshed_url = format!("{base}?ex=70000000&is=6ffff000&hm=mockrefreshed");
        Ok(RefreshedAttachmentUrl {
            original_url: original_url.to_string(),
            refreshed_url,
            expires_at_unix: Some(1_879_048_192),
        })
    }

    fn subscribe_events(&self) -> broadcast::Receiver<BackendEvent> {
        self.events.subscribe()
    }
}

/// Deterministic offline [`GifProvider`] shipping animated procedural RGBA preview frames.
#[derive(Debug, Clone)]
pub struct MockGifProvider {
    catalog: Vec<GifItem>,
}

impl Default for MockGifProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MockGifProvider {
    /// Build the offline mock GIF catalog with pre-rendered 4-frame `16x12` animations.
    #[must_use]
    pub fn new() -> Self {
        let entries = [
            ("gif-matrix", "ANSI Matrix Rain", (0, 220, 90)),
            ("gif-modem", "56k Dial-Up Modem Handshake", (56, 189, 248)),
            ("gif-synthwave", "Neon Synthwave Grid", (236, 72, 153)),
            ("gif-ferris", "Rust Ferris Crab Dance", (249, 115, 22)),
            ("gif-parrot", "Party Parrot BBS", (168, 85, 247)),
            ("gif-crt", "Amber Phosphor CRT Boot", (250, 204, 21)),
        ];

        let catalog = entries
            .into_iter()
            .enumerate()
            .map(|(idx, (id, title, base_rgb))| {
                let frames = Self::synth_animation(16, 12, base_rgb, idx as u8);
                GifItem {
                    id: id.to_string(),
                    provider: GifProviderKind::Mock,
                    title: title.to_string(),
                    preview_url: format!("https://example.invalid/mock-gifs/{id}-preview.gif"),
                    gif_url: format!("https://example.invalid/mock-gifs/{id}.gif"),
                    width: 320,
                    height: 240,
                    frame_count: Some(frames.len() as u32),
                    duration_ms: Some((frames.len() as u32) * 120),
                    preview_rgba_frames: Some(frames),
                }
            })
            .collect();

        Self { catalog }
    }

    fn synth_animation(width: u32, height: u32, (br, bg, bb): (u8, u8, u8), seed: u8) -> Vec<RgbaFrame> {
        (0..4u32)
            .map(|frame_idx| {
                let mut pixels = Vec::with_capacity((width * height * 4) as usize);
                for y in 0..height {
                    for x in 0..width {
                        let band = ((x + y + frame_idx * 3 + u32::from(seed)) % 8) as f32 / 7.0;
                        let r = (f32::from(br) * (0.35 + 0.65 * band)).round() as u8;
                        let g = (f32::from(bg) * (0.35 + 0.65 * band)).round() as u8;
                        let b = (f32::from(bb) * (0.35 + 0.65 * band)).round() as u8;
                        pixels.extend_from_slice(&[r, g, b, 255]);
                    }
                }
                RgbaFrame {
                    width,
                    height,
                    delay_ms: 120,
                    pixels,
                }
            })
            .collect()
    }

    fn paginate(items: &[GifItem], limit: u8, cursor: Option<&str>) -> GifSearchPage {
        let offset = cursor
            .and_then(|c| c.parse::<usize>().ok())
            .unwrap_or(0)
            .min(items.len());
        let page_size = usize::from(limit.max(1));
        let end = (offset + page_size).min(items.len());
        let next_cursor = (end < items.len()).then(|| end.to_string());
        GifSearchPage {
            items: items[offset..end].to_vec(),
            next_cursor,
        }
    }
}

#[async_trait]
impl GifProvider for MockGifProvider {
    fn provider_kind(&self) -> GifProviderKind {
        GifProviderKind::Mock
    }

    async fn search(&self, query: &GifSearchQuery) -> CoreResult<GifSearchPage> {
        let q = query.query.trim().to_ascii_lowercase();
        if q.is_empty() || query.trending {
            return Ok(Self::paginate(
                &self.catalog,
                query.limit,
                query.cursor.as_deref(),
            ));
        }
        let matches: Vec<GifItem> = self
            .catalog
            .iter()
            .filter(|item| {
                item.title.to_ascii_lowercase().contains(&q)
                    || item.id.to_ascii_lowercase().contains(&q)
            })
            .cloned()
            .collect();
        Ok(Self::paginate(&matches, query.limit, query.cursor.as_deref()))
    }

    async fn trending(&self, limit: u8, cursor: Option<&str>) -> CoreResult<GifSearchPage> {
        Ok(Self::paginate(&self.catalog, limit, cursor))
    }
}

#[derive(Debug, Default)]
struct MockVoiceState {
    status: Option<VoiceChannelStatus>,
    playback: Option<VoicePlaybackState>,
}

/// Deterministic offline [`VoiceEngine`] simulating Guild Voice, Group DM calls, DAVE E2EE, and VU levels.
pub struct MockVoiceEngine {
    state: RwLock<MockVoiceState>,
    events: broadcast::Sender<BackendEvent>,
}

impl Default for MockVoiceEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MockVoiceEngine {
    /// Create a disconnected [`MockVoiceEngine`].
    #[must_use]
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            state: RwLock::new(MockVoiceState::default()),
            events,
        }
    }

    /// Advance simulated RMS dBFS audio levels deterministically for Braille VU meter demos/tests.
    pub fn step_audio_levels(&self, tick: u64) -> CoreResult<Vec<(Snowflake, f32, bool)>> {
        let (channel_id, levels) = {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock voice lock poisoned".to_string()))?;
            let Some(status) = guard.status.as_mut() else {
                return Ok(Vec::new());
            };
            let mut out = Vec::with_capacity(status.participants.len());
            for (idx, p) in status.participants.iter_mut().enumerate() {
                if p.self_mute || p.server_mute || p.self_deaf {
                    p.rms_db = VoiceParticipant::MIN_RMS_DB;
                    p.speaking = false;
                } else {
                    let phase = (tick as f32) * 0.45 + (idx as f32) * 1.7;
                    let wave = (phase.sin() + 1.0) * 0.5; // 0..=1
                    let rms_db = -54.0 + wave * 48.0; // -54..-6 dBFS
                    p.rms_db = rms_db;
                    p.speaking = rms_db > -32.0;
                }
                out.push((p.user_id, p.rms_db, p.speaking));
            }
            (status.channel_id, out)
        };

        let _ = self.events.send(BackendEvent::VoiceAudioLevels {
            channel_id,
            levels: levels.clone(),
        });
        Ok(levels)
    }

    fn emit(&self, event: BackendEvent) {
        let _ = self.events.send(event);
    }
}

#[async_trait]
impl VoiceEngine for MockVoiceEngine {
    async fn join_voice(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
    ) -> CoreResult<VoiceChannelStatus> {
        let is_dm_call = guild_id.is_none();
        let channel_name = if channel_id == mock_ids::CH_GROUP_DM_OPS {
            "BBS Ops War Room (Call)".to_string()
        } else if channel_id == mock_ids::CH_RETRONET_VOICE {
            "Lounge 56k".to_string()
        } else if is_dm_call {
            format!("DM Call {channel_id}")
        } else {
            format!("Voice {channel_id}")
        };

        let participants = vec![
            VoiceParticipant {
                user_id: mock_ids::USER_SELF,
                display_name: "SysOp Prime".to_string(),
                speaking: false,
                self_mute: false,
                self_deaf: false,
                server_mute: false,
                server_deaf: false,
                volume: 100,
                rms_db: -48.0,
                dave_verified: true,
                streaming: false,
            },
            VoiceParticipant {
                user_id: mock_ids::USER_ALICE,
                display_name: "Modem Wizard".to_string(),
                speaking: true,
                self_mute: false,
                self_deaf: false,
                server_mute: false,
                server_deaf: false,
                volume: 100,
                rms_db: -18.5,
                dave_verified: true,
                streaming: channel_id == mock_ids::CH_RETRONET_VOICE,
            },
            VoiceParticipant {
                user_id: mock_ids::USER_BOB,
                display_name: "CRT Hacker".to_string(),
                speaking: false,
                self_mute: false,
                self_deaf: false,
                server_mute: false,
                server_deaf: false,
                volume: 115,
                rms_db: -42.0,
                dave_verified: true,
                streaming: channel_id == mock_ids::CH_GROUP_DM_OPS,
            },
        ];

        let call_ring = if is_dm_call {
            CallRingState::Active {
                channel_id,
                connected_user_ids: participants.iter().map(|p| p.user_id).collect(),
            }
        } else {
            CallRingState::Idle
        };

        let status = VoiceChannelStatus {
            guild_id,
            channel_id,
            channel_name,
            connection_state: VoiceConnectionState::Connected,
            self_mute: false,
            self_deaf: false,
            push_to_talk: false,
            ping_ms: Some(19),
            packet_loss_pct: 0.0,
            dave: DaveSessionState::Active {
                epoch: 4,
                protocol_version: 1,
                verification_code: "48291-07314-92650-11842".to_string(),
            },
            call_ring,
            participants,
        };

        {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock voice lock poisoned".to_string()))?;
            guard.status = Some(status.clone());
        }
        self.emit(BackendEvent::VoiceStatusUpdated(status.clone()));
        Ok(status)
    }

    async fn leave_voice(&self) -> CoreResult<()> {
        let mut guard = self
            .state
            .write()
            .map_err(|_| CoreError::Internal("mock voice lock poisoned".to_string()))?;
        guard.status = None;
        Ok(())
    }

    async fn set_self_mute(&self, muted: bool) -> CoreResult<VoiceChannelStatus> {
        let status = {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock voice lock poisoned".to_string()))?;
            let status = guard
                .status
                .as_mut()
                .ok_or_else(|| CoreError::Voice("not connected to voice".to_string()))?;
            status.self_mute = muted;
            if let Some(me) = status
                .participants
                .iter_mut()
                .find(|p| p.user_id == mock_ids::USER_SELF)
            {
                me.self_mute = muted;
                if muted {
                    me.speaking = false;
                    me.rms_db = VoiceParticipant::MIN_RMS_DB;
                }
            }
            status.clone()
        };
        self.emit(BackendEvent::VoiceStatusUpdated(status.clone()));
        Ok(status)
    }

    async fn set_self_deaf(&self, deafened: bool) -> CoreResult<VoiceChannelStatus> {
        let status = {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock voice lock poisoned".to_string()))?;
            let status = guard
                .status
                .as_mut()
                .ok_or_else(|| CoreError::Voice("not connected to voice".to_string()))?;
            status.self_deaf = deafened;
            if deafened {
                status.self_mute = true;
            }
            if let Some(me) = status
                .participants
                .iter_mut()
                .find(|p| p.user_id == mock_ids::USER_SELF)
            {
                me.self_deaf = deafened;
                if deafened {
                    me.self_mute = true;
                    me.speaking = false;
                    me.rms_db = VoiceParticipant::MIN_RMS_DB;
                }
            }
            status.clone()
        };
        self.emit(BackendEvent::VoiceStatusUpdated(status.clone()));
        Ok(status)
    }

    async fn set_participant_volume(
        &self,
        user_id: Snowflake,
        volume: u8,
    ) -> CoreResult<VoiceParticipant> {
        let (participant, status) = {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock voice lock poisoned".to_string()))?;
            let status = guard
                .status
                .as_mut()
                .ok_or_else(|| CoreError::Voice("not connected to voice".to_string()))?;
            let p = status
                .participants
                .iter_mut()
                .find(|p| p.user_id == user_id)
                .ok_or_else(|| CoreError::NotFound(format!("voice participant {user_id}")))?;
            p.set_volume(volume);
            (p.clone(), status.clone())
        };
        self.emit(BackendEvent::VoiceStatusUpdated(status));
        Ok(participant)
    }

    async fn status(&self) -> CoreResult<Option<VoiceChannelStatus>> {
        let guard = self
            .state
            .read()
            .map_err(|_| CoreError::Internal("mock voice lock poisoned".to_string()))?;
        Ok(guard.status.clone())
    }

    async fn dave_state(&self) -> CoreResult<DaveSessionState> {
        let guard = self
            .state
            .read()
            .map_err(|_| CoreError::Internal("mock voice lock poisoned".to_string()))?;
        Ok(guard
            .status
            .as_ref()
            .map_or(DaveSessionState::Disabled, |s| s.dave.clone()))
    }

    async fn play_voice_message(
        &self,
        message_id: Snowflake,
        attachment: &Attachment,
    ) -> CoreResult<VoicePlaybackState> {
        let duration_secs = attachment
            .voice_message
            .as_ref()
            .map_or(5.0, |vm| vm.duration_secs);
        let pb = VoicePlaybackState {
            message_id,
            attachment_id: attachment.id,
            playing: true,
            position_secs: 0.0,
            duration_secs,
        };
        {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock voice lock poisoned".to_string()))?;
            guard.playback = Some(pb.clone());
        }
        self.emit(BackendEvent::VoicePlaybackUpdated(pb.clone()));
        Ok(pb)
    }

    async fn stop_voice_message(&self) -> CoreResult<()> {
        let stopped = {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock voice lock poisoned".to_string()))?;
            guard.playback.take().map(|mut pb| {
                pb.playing = false;
                pb
            })
        };
        if let Some(pb) = stopped {
            self.emit(BackendEvent::VoicePlaybackUpdated(pb));
        }
        Ok(())
    }

    fn subscribe_voice_events(&self) -> broadcast::Receiver<BackendEvent> {
        self.events.subscribe()
    }
}

#[derive(Debug)]
struct MockVideoState {
    streams: HashMap<String, StreamDescriptor>,
    watching: HashMap<String, VideoSinkKind>,
    broadcast: Option<StreamDescriptor>,
}

/// Deterministic offline [`VideoStreamEngine`] with synthetic SMPTE color-bar test-card frames.
pub struct MockVideoStreamEngine {
    state: RwLock<MockVideoState>,
    frame_seq: AtomicU64,
    frames: broadcast::Sender<VideoFrame>,
}

impl Default for MockVideoStreamEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MockVideoStreamEngine {
    /// Construct a [`MockVideoStreamEngine`] pre-seeded with Guild Voice and Group DM streams.
    #[must_use]
    pub fn new() -> Self {
        let (frames, _) = broadcast::channel(32);
        let mut streams = HashMap::new();

        streams.insert(
            mock_ids::STREAM_RETRONET.to_string(),
            StreamDescriptor {
                stream_key: mock_ids::STREAM_RETRONET.to_string(),
                guild_id: Some(mock_ids::GUILD_RETRONET),
                channel_id: mock_ids::CH_RETRONET_VOICE,
                owner_id: mock_ids::USER_ALICE,
                owner_name: "Modem Wizard".to_string(),
                title: "CRT Shader Lab — 1080p60".to_string(),
                resolution: (1920, 1080),
                fps: 60,
                viewer_count: 3,
                has_audio: true,
                dave_encrypted: true,
            },
        );

        streams.insert(
            mock_ids::STREAM_GROUP_DM.to_string(),
            StreamDescriptor {
                stream_key: mock_ids::STREAM_GROUP_DM.to_string(),
                guild_id: None,
                channel_id: mock_ids::CH_GROUP_DM_OPS,
                owner_id: mock_ids::USER_BOB,
                owner_name: "CRT Hacker".to_string(),
                title: "Hyprland Compositor Debug — 720p30".to_string(),
                resolution: (1280, 720),
                fps: 30,
                viewer_count: 2,
                has_audio: true,
                dave_encrypted: true,
            },
        );

        Self {
            state: RwLock::new(MockVideoState {
                streams,
                watching: HashMap::new(),
                broadcast: None,
            }),
            frame_seq: AtomicU64::new(0),
            frames,
        }
    }

    /// Generate a deterministic SMPTE-style color-bar RGBA8 test-card frame with an animated scanbar.
    #[must_use]
    pub fn generate_test_card_frame(
        stream_key: impl Into<String>,
        seq: u64,
        width: u32,
        height: u32,
    ) -> VideoFrame {
        const BARS: [(u8, u8, u8); 7] = [
            (235, 235, 235), // White
            (235, 235, 16),  // Yellow
            (16, 235, 235),  // Cyan
            (16, 235, 16),   // Green
            (235, 16, 235),  // Magenta
            (235, 16, 16),   // Red
            (16, 16, 235),   // Blue
        ];

        let w = width.max(1);
        let h = height.max(1);
        let scan_y = (seq as u32) % h;
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);

        for y in 0..h {
            for x in 0..w {
                if y == scan_y {
                    rgba.extend_from_slice(&[255, 255, 255, 255]);
                } else {
                    let bar_idx = ((x as usize) * BARS.len()) / (w as usize);
                    let (r, g, b) = BARS[bar_idx.min(BARS.len() - 1)];
                    // Lower third dark calibration strip.
                    if y >= (h * 3) / 4 {
                        rgba.extend_from_slice(&[r / 3, g / 3, b / 3, 255]);
                    } else {
                        rgba.extend_from_slice(&[r, g, b, 255]);
                    }
                }
            }
        }

        VideoFrame {
            stream_key: stream_key.into(),
            seq,
            timestamp_ms: seq.saturating_mul(33),
            width: w,
            height: h,
            rgba,
        }
    }

    /// Emit one synthetic test-card frame to all frame subscribers for `stream_key`.
    pub fn emit_test_frame(&self, stream_key: &str, width: u32, height: u32) -> VideoFrame {
        let seq = self.frame_seq.fetch_add(1, Ordering::Relaxed);
        let frame = Self::generate_test_card_frame(stream_key, seq, width, height);
        let _ = self.frames.send(frame.clone());
        frame
    }

    /// Return the currently selected [`VideoSinkKind`] for a watched stream, if active.
    pub fn active_sink(&self, stream_key: &str) -> CoreResult<Option<VideoSinkKind>> {
        let guard = self
            .state
            .read()
            .map_err(|_| CoreError::Internal("mock video lock poisoned".to_string()))?;
        Ok(guard.watching.get(stream_key).copied())
    }
}

#[async_trait]
impl VideoStreamEngine for MockVideoStreamEngine {
    async fn list_streams(&self, channel_id: Snowflake) -> CoreResult<Vec<StreamDescriptor>> {
        let guard = self
            .state
            .read()
            .map_err(|_| CoreError::Internal("mock video lock poisoned".to_string()))?;
        let mut out: Vec<StreamDescriptor> = guard
            .streams
            .values()
            .filter(|s| s.channel_id == channel_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.stream_key.cmp(&b.stream_key));
        Ok(out)
    }

    async fn watch_stream(
        &self,
        stream_key: &str,
        sink: VideoSinkKind,
    ) -> CoreResult<StreamDescriptor> {
        let descriptor = {
            let mut guard = self
                .state
                .write()
                .map_err(|_| CoreError::Internal("mock video lock poisoned".to_string()))?;
            let already_watching = guard.watching.contains_key(stream_key);
            let desc = guard
                .streams
                .get_mut(stream_key)
                .ok_or_else(|| CoreError::NotFound(format!("stream {stream_key}")))?;
            if !already_watching {
                desc.viewer_count = desc.viewer_count.saturating_add(1);
            }
            let cloned = desc.clone();
            guard.watching.insert(stream_key.to_string(), sink);
            cloned
        };

        if sink == VideoSinkKind::InlineTerminal {
            let _ = self.emit_test_frame(stream_key, 64, 36);
        }
        Ok(descriptor)
    }

    async fn stop_watching(&self, stream_key: &str) -> CoreResult<()> {
        let mut guard = self
            .state
            .write()
            .map_err(|_| CoreError::Internal("mock video lock poisoned".to_string()))?;
        if guard.watching.remove(stream_key).is_some()
            && let Some(desc) = guard.streams.get_mut(stream_key)
        {
            desc.viewer_count = desc.viewer_count.saturating_sub(1);
        }
        Ok(())
    }

    async fn start_broadcast(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
        source: StreamSourceKind,
        quality: StreamQuality,
    ) -> CoreResult<StreamDescriptor> {
        let stream_key = match guild_id {
            Some(gid) => format!("guild:{gid}:{channel_id}:{}", mock_ids::USER_SELF),
            None => format!("call:{channel_id}:{}", mock_ids::USER_SELF),
        };
        let desc = StreamDescriptor {
            stream_key: stream_key.clone(),
            guild_id,
            channel_id,
            owner_id: mock_ids::USER_SELF,
            owner_name: "SysOp Prime".to_string(),
            title: format!("bbscord Go Live ({source:?})"),
            resolution: (quality.width, quality.height),
            fps: quality.fps,
            viewer_count: 1,
            has_audio: true,
            dave_encrypted: true,
        };
        let mut guard = self
            .state
            .write()
            .map_err(|_| CoreError::Internal("mock video lock poisoned".to_string()))?;
        guard.streams.insert(stream_key, desc.clone());
        guard.broadcast = Some(desc.clone());
        Ok(desc)
    }

    async fn stop_broadcast(&self) -> CoreResult<()> {
        let mut guard = self
            .state
            .write()
            .map_err(|_| CoreError::Internal("mock video lock poisoned".to_string()))?;
        if let Some(prev) = guard.broadcast.take() {
            guard.streams.remove(&prev.stream_key);
        }
        Ok(())
    }

    async fn active_broadcast(&self) -> CoreResult<Option<StreamDescriptor>> {
        let guard = self
            .state
            .read()
            .map_err(|_| CoreError::Internal("mock video lock poisoned".to_string()))?;
        Ok(guard.broadcast.clone())
    }

    fn subscribe_frames(&self) -> broadcast::Receiver<VideoFrame> {
        self.frames.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_discord_backend_covers_guilds_group_dm_calls_and_messages() {
        let backend = MockDiscordBackend::new();
        let mut rx = backend.subscribe_events();

        let (guilds, folders) = backend.list_guilds().await.unwrap();
        assert_eq!(guilds.len(), 3);
        assert_eq!(folders.len(), 2);

        let dms = backend.list_dm_channels().await.unwrap();
        assert!(dms.iter().any(|c| c.kind == ChannelKind::GroupDm));
        assert!(matches!(
            backend.call_ring_state(mock_ids::CH_GROUP_DM_OPS).unwrap(),
            CallRingState::IncomingRinging {
                is_group_dm: true,
                ..
            }
        ));

        // Verify rich seeded messages include inline embeds, voice message, and poll.
        let msgs = backend
            .fetch_messages(mock_ids::CH_RETRONET_GENERAL, None, 20)
            .await
            .unwrap();
        assert_eq!(msgs.len(), 4);
        assert!(msgs[0].embeds[0].fields.iter().any(|f| f.inline));
        assert!(msgs[2].is_voice_message());
        assert!(msgs[3].poll.is_some());

        // Group DM CRUD + call ring lifecycle.
        let gdm = backend
            .create_group_dm(&[mock_ids::USER_ALICE, mock_ids::USER_BOB], Some("Night Ops"))
            .await
            .unwrap();
        assert_eq!(gdm.kind, ChannelKind::GroupDm);
        let gdm = backend
            .add_group_dm_recipient(gdm.id, mock_ids::USER_CAROL)
            .await
            .unwrap();
        assert_eq!(gdm.recipients.len(), 3);
        let gdm = backend.rename_group_dm(gdm.id, "BBS Night Ops").await.unwrap();
        assert_eq!(gdm.name, "BBS Night Ops");
        let ring = backend.ring_call(gdm.id, None).await.unwrap();
        assert!(matches!(ring, CallRingState::OutgoingRinging { .. }));
        assert_eq!(
            backend.stop_call_ring(gdm.id).await.unwrap(),
            CallRingState::Idle
        );
        let gdm = backend
            .remove_group_dm_recipient(gdm.id, mock_ids::USER_CAROL)
            .await
            .unwrap();
        assert_eq!(gdm.recipients.len(), 2);

        // Send, edit, react, delete message.
        let sent = backend
            .send_message(gdm.id, "hello bbs", None)
            .await
            .unwrap();
        let edited = backend
            .edit_message(gdm.id, sent.id, "hello bbs (edited)")
            .await
            .unwrap();
        assert!(edited.edited_timestamp.is_some());
        backend.add_reaction(gdm.id, sent.id, "🚀").await.unwrap();
        backend.remove_reaction(gdm.id, sent.id, "🚀").await.unwrap();
        backend.delete_message(gdm.id, sent.id).await.unwrap();

        // Attachment CDN URL refresh.
        let refreshed = backend
            .refresh_attachment_url("https://example.invalid/a/b.png?ex=old")
            .await
            .unwrap();
        assert!(refreshed.refreshed_url.contains("mockrefreshed"));

        // Leave Group DM cleanly.
        backend.leave_group_dm(gdm.id).await.unwrap();
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn mock_discord_backend_seeds_visual_hierarchy_roles_rosters_and_ack() {
        let backend = MockDiscordBackend::new();
        let mut rx = backend.subscribe_events();

        let (guilds, folders) = backend.list_guilds().await.unwrap();
        assert!(folders.iter().any(|f| f.id.is_some() && f.color.is_some()));
        let retronet = guilds
            .iter()
            .find(|g| g.id == mock_ids::GUILD_RETRONET)
            .unwrap();
        assert!(retronet.roles.len() >= 4);
        assert_eq!(
            retronet.highest_role_color(&[mock_ids::ROLE_NETRUNNER]),
            Some(0x50FA7B)
        );

        let channels = backend
            .list_channels(mock_ids::GUILD_RETRONET)
            .await
            .unwrap();
        assert!(channels.iter().any(|c| c.muted));
        assert!(channels.iter().any(|c| c.mention_count > 0));
        assert!(
            channels
                .iter()
                .any(|c| !c.muted && c.unread_count > 0 && c.mention_count == 0)
        );

        let dms = backend.list_dm_channels().await.unwrap();
        let dm_statuses: Vec<PresenceStatus> = dms
            .iter()
            .filter(|c| c.kind == ChannelKind::DirectMessage)
            .filter_map(|c| c.recipients.first().map(|u| u.presence))
            .collect();
        assert!(dm_statuses.contains(&PresenceStatus::Online));
        assert!(dm_statuses.contains(&PresenceStatus::Idle));
        assert!(dm_statuses.contains(&PresenceStatus::DoNotDisturb));
        assert!(dm_statuses.contains(&PresenceStatus::Offline));

        let rosters = backend.voice_rosters().unwrap();
        assert!(
            rosters
                .iter()
                .any(|(_, cid, ps)| *cid == mock_ids::CH_RETRONET_VOICE && !ps.is_empty())
        );
        let members = backend.guild_members(mock_ids::GUILD_RETRONET).unwrap();
        assert!(!members.is_empty());

        // Silent vs ping reply mention behavior.
        let reply_ping = backend
            .send_message_with_mention(
                mock_ids::CH_RETRONET_GENERAL,
                "ping reply",
                Some(Snowflake(400_000_000_000_000_002)),
                true,
            )
            .await
            .unwrap();
        assert!(reply_ping.mentions.contains(&mock_ids::USER_ALICE));

        let reply_silent = backend
            .send_message_with_mention(
                mock_ids::CH_RETRONET_GENERAL,
                "silent reply",
                Some(Snowflake(400_000_000_000_000_002)),
                false,
            )
            .await
            .unwrap();
        assert!(reply_silent.mentions.is_empty());

        // Ack clears unread & mention counters and emits MessageAcked.
        backend
            .ack_message(mock_ids::CH_RETRONET_GENERAL, reply_silent.id)
            .await
            .unwrap();
        let updated_channels = backend
            .list_channels(mock_ids::GUILD_RETRONET)
            .await
            .unwrap();
        let general = updated_channels
            .iter()
            .find(|c| c.id == mock_ids::CH_RETRONET_GENERAL)
            .unwrap();
        assert_eq!(general.unread_count, 0);
        assert_eq!(general.mention_count, 0);
        assert_eq!(general.last_ack_message_id, Some(reply_silent.id));

        let mut saw_ack = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(
                ev,
                BackendEvent::MessageAcked {
                    channel_id,
                    last_message_id,
                    mention_count: 0,
                } if channel_id == mock_ids::CH_RETRONET_GENERAL && last_message_id == reply_silent.id
            ) {
                saw_ack = true;
            }
        }
        assert!(saw_ack);
    }

    #[tokio::test]
    async fn mock_gif_provider_has_animated_frames_and_search() {
        let provider = MockGifProvider::new();
        assert_eq!(provider.provider_kind(), GifProviderKind::Mock);

        let page1 = provider.trending(2, None).await.unwrap();
        assert_eq!(page1.items.len(), 2);
        assert!(page1.next_cursor.is_some());
        let frames = page1.items[0].preview_rgba_frames.as_ref().unwrap();
        assert_eq!(frames.len(), 4);
        assert_eq!(frames[0].pixels.len(), (16 * 12 * 4) as usize);

        let search = provider
            .search(&GifSearchQuery {
                query: "ferris".to_string(),
                limit: 5,
                cursor: None,
                trending: false,
                locale: None,
            })
            .await
            .unwrap();
        assert_eq!(search.items.len(), 1);
        assert_eq!(search.items[0].id, "gif-ferris");
    }

    #[tokio::test]
    async fn mock_voice_engine_supports_group_dm_dave_and_vu_meters() {
        let voice = MockVoiceEngine::new();
        let mut rx = voice.subscribe_voice_events();

        let status = voice
            .join_voice(None, mock_ids::CH_GROUP_DM_OPS)
            .await
            .unwrap();
        assert_eq!(status.guild_id, None);
        assert!(matches!(status.dave, DaveSessionState::Active { epoch: 4, .. }));
        assert!(matches!(status.call_ring, CallRingState::Active { .. }));

        let levels = voice.step_audio_levels(3).unwrap();
        assert_eq!(levels.len(), 3);

        let muted = voice.set_self_mute(true).await.unwrap();
        assert!(muted.self_mute);
        let vol = voice
            .set_participant_volume(mock_ids::USER_ALICE, 240)
            .await
            .unwrap();
        assert_eq!(vol.volume, 200);

        let vm_attachment = Attachment {
            id: Snowflake(1),
            filename: "voice.ogg".into(),
            content_type: Some("audio/ogg".into()),
            size_bytes: 1024,
            url: "https://example.invalid/v.ogg".into(),
            proxy_url: None,
            width: None,
            height: None,
            ephemeral: false,
            expires_at_unix: None,
            voice_message: Some(VoiceMessageMeta {
                duration_secs: 4.2,
                waveform: vec![10, 80, 180, 90],
            }),
        };
        let pb = voice
            .play_voice_message(Snowflake(99), &vm_attachment)
            .await
            .unwrap();
        assert!(pb.playing);
        voice.stop_voice_message().await.unwrap();
        voice.leave_voice().await.unwrap();
        assert!(voice.status().await.unwrap().is_none());
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn mock_video_engine_watch_broadcast_and_test_cards() {
        let video = MockVideoStreamEngine::new();
        let mut rx = video.subscribe_frames();

        let streams = video
            .list_streams(mock_ids::CH_GROUP_DM_OPS)
            .await
            .unwrap();
        assert_eq!(streams.len(), 1);

        let watched = video
            .watch_stream(&streams[0].stream_key, VideoSinkKind::InlineTerminal)
            .await
            .unwrap();
        assert_eq!(watched.viewer_count, 3);
        let frame = rx.recv().await.unwrap();
        assert_eq!(frame.width, 64);
        assert_eq!(frame.height, 36);
        assert_eq!(frame.rgba.len(), (64 * 36 * 4) as usize);

        let bcast = video
            .start_broadcast(
                Some(mock_ids::GUILD_RETRONET),
                mock_ids::CH_RETRONET_VOICE,
                StreamSourceKind::SyntheticTestCard,
                StreamQuality::default(),
            )
            .await
            .unwrap();
        assert!(video.active_broadcast().await.unwrap().is_some());
        assert_eq!(bcast.resolution, (1280, 720));

        video.stop_broadcast().await.unwrap();
        assert!(video.active_broadcast().await.unwrap().is_none());
        video.stop_watching(&streams[0].stream_key).await.unwrap();
    }
}
