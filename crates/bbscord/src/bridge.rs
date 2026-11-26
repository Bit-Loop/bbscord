//! Unified runtime service wiring for both offline `--mock-demo` and production live Discord mode.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use async_trait::async_trait;
use bbs_core::{
    BackendEvent, CallRingState, ConnectionState, CoreResult, DiscordBackend, GifProvider,
    MockDiscordBackend, MockGifProvider, MockVideoStreamEngine, MockVoiceEngine, SecretToken,
    Snowflake, StreamDescriptor, UiAction, VideoStreamEngine, VoiceEngine, mock_ids,
};
use bbs_protocol::wire::WireGuildMember;
use bbs_protocol::{
    CompositeGifProvider, DiscordFingerprint, DiscordGateway, DiscordGifProvider,
    DiscordRestClient, FingerprintConfig, GatewayConfig, GatewayHandle, GatewaySignalingEvent,
    GiphyGifProvider, KlipyGifProvider, UploadAttachment, is_direct_gif_or_webp_url,
    resolve_direct_gif_url,
};
use bbs_video::signaling::{
    GatewayStreamCommand, GatewayStreamEvent, RedactedStreamToken, StreamKey,
};
use bbs_video::{GStreamerVideoStreamEngine, VideoEngineConfig};
use bbs_voice::{
    AudioBackendMode, CallUpdateEvent, DiscordVoiceEngine, VoiceEngineConfig, VoiceError,
    VoiceResult, VoiceServerUpdateEvent, VoiceSignalingBridge, VoiceStateUpdateCommand,
    VoiceStateUpdateEvent,
};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use zeroize::Zeroizing;

use crate::config::{AppConfig, OptionalApiSecrets};

/// Bridges `bbs-voice` Op 4 (`UPDATE_VOICE_STATE`) commands onto `bbs-protocol`'s [`GatewayHandle`].
pub struct GatewayVoiceBridge {
    gateway: GatewayHandle,
}

impl GatewayVoiceBridge {
    /// Wrap a [`GatewayHandle`] as a [`VoiceSignalingBridge`].
    #[must_use]
    pub const fn new(gateway: GatewayHandle) -> Self {
        Self { gateway }
    }
}

#[async_trait]
impl VoiceSignalingBridge for GatewayVoiceBridge {
    async fn send_voice_state_update(&self, cmd: VoiceStateUpdateCommand) -> VoiceResult<()> {
        self.gateway
            .update_voice_state(
                cmd.guild_id,
                cmd.channel_id,
                cmd.self_mute,
                cmd.self_deaf,
                false,
            )
            .map_err(|err| VoiceError::Signaling(err.to_string()))
    }
}

/// Shared channel & user name lookup populated from live/mock backend events so voice
/// status headers and remote participants display real names instead of raw Snowflakes.
#[derive(Default)]
struct NameRegistry {
    channels: RwLock<HashMap<Snowflake, String>>,
    users: RwLock<HashMap<Snowflake, String>>,
}

impl NameRegistry {
    fn observe_event(&self, ev: &BackendEvent) {
        match ev {
            BackendEvent::Ready {
                current_user,
                dm_channels,
                ..
            } => {
                self.record_user(current_user.id, current_user.display_name());
                for ch in dm_channels {
                    self.record_channel(ch.id, &ch.display_label());
                    for r in &ch.recipients {
                        self.record_user(r.id, r.display_name());
                    }
                }
            }
            BackendEvent::ChannelsLoaded { channels, .. } => {
                for ch in channels {
                    self.record_channel(ch.id, &ch.display_label());
                    for r in &ch.recipients {
                        self.record_user(r.id, r.display_name());
                    }
                }
            }
            BackendEvent::ChannelUpserted(ch) => {
                self.record_channel(ch.id, &ch.display_label());
                for r in &ch.recipients {
                    self.record_user(r.id, r.display_name());
                }
            }
            BackendEvent::MessagesLoaded { messages, .. } => {
                for m in messages {
                    self.record_user(
                        m.author.id,
                        m.author_nick
                            .as_deref()
                            .unwrap_or_else(|| m.author.display_name()),
                    );
                }
            }
            BackendEvent::MessageCreated(m) | BackendEvent::MessageUpdated(m) => {
                self.record_user(
                    m.author.id,
                    m.author_nick
                        .as_deref()
                        .unwrap_or_else(|| m.author.display_name()),
                );
            }
            BackendEvent::GuildMembersUpdated { members, .. } => {
                for m in members {
                    if let Some(nick) = m.nick.as_deref().filter(|s| !s.is_empty()) {
                        self.record_user(m.user_id, nick);
                    }
                }
            }
            _ => {}
        }
    }

    fn record_channel(&self, id: Snowflake, label: &str) {
        if !label.is_empty()
            && let Ok(mut map) = self.channels.write()
        {
            map.insert(id, label.to_owned());
        }
    }

    fn record_user(&self, id: Snowflake, name: &str) {
        if !name.is_empty()
            && let Ok(mut map) = self.users.write()
        {
            map.insert(id, name.to_owned());
        }
    }

    fn channel_name(&self, id: Snowflake) -> Option<String> {
        self.channels.read().ok().and_then(|m| m.get(&id).cloned())
    }

    fn user_name(&self, id: Snowflake) -> Option<String> {
        self.users.read().ok().and_then(|m| m.get(&id).cloned())
    }
}

/// Unified container owning all protocol, GIF, voice, and Go Live video services.
pub struct RuntimeServices {
    /// Core Discord REST/state backend (live or mock).
    pub discord: Arc<dyn DiscordBackend>,
    /// Concrete mock Discord backend for seeding rich offline member/voice-roster demos.
    mock_discord: Option<Arc<MockDiscordBackend>>,
    /// Live REST client (present only in live mode for multipart uploads and signed CDN fetches).
    pub rest_client: Option<Arc<DiscordRestClient>>,
    /// Live Gateway handle (present only in live mode).
    pub gateway: Option<GatewayHandle>,
    /// GIF search/trending catalog (Discord proxy + optional Klipy/Giphy fallbacks, or Mock).
    pub gifs: Arc<dyn GifProvider>,
    /// Voice & DAVE E2EE engine (live or mock).
    pub voice: Arc<dyn VoiceEngine>,
    /// Concrete live voice engine for Gateway signaling & voice-message byte playback.
    pub live_voice: Option<Arc<DiscordVoiceEngine>>,
    /// Concrete mock voice engine for deterministic VU progression in `--mock-demo`.
    pub mock_voice: Option<Arc<MockVoiceEngine>>,
    /// Go Live video/screenshare engine (live or mock).
    pub video: Arc<dyn VideoStreamEngine>,
    /// Concrete live GStreamer video engine for shutdown and Go Live signaling.
    pub live_video: Option<Arc<GStreamerVideoStreamEngine>>,
    /// Concrete mock video engine for synthetic SMPTE test-card emission in `--mock-demo`.
    pub mock_video: Option<Arc<MockVideoStreamEngine>>,
    /// Unified event bus consumed by the TUI loop.
    pub event_tx: mpsc::UnboundedSender<BackendEvent>,
    /// Sequential FIFO lane for voice/call actions (`JoinVoice -> RingCall`, `LeaveVoice`, etc.).
    voice_action_tx: mpsc::UnboundedSender<UiAction>,
    /// Cached channel & user display names for voice headers and remote participants.
    names: Arc<NameRegistry>,
    /// Background task handles aborted/drained cleanly on shutdown.
    tasks: Vec<JoinHandle<()>>,
}

impl RuntimeServices {
    /// Build a 100% offline deterministic mock service stack for `--mock-demo` and `--headless-smoke`.
    #[must_use]
    pub fn build_mock() -> (Self, mpsc::UnboundedReceiver<BackendEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let names = Arc::new(NameRegistry::default());

        let mock_discord = Arc::new(MockDiscordBackend::new());
        let mock_gifs = Arc::new(MockGifProvider::new());
        let mock_voice = Arc::new(MockVoiceEngine::new());
        let mock_video = Arc::new(MockVideoStreamEngine::new());

        let discord_trait: Arc<dyn DiscordBackend> = Arc::clone(&mock_discord) as Arc<dyn DiscordBackend>;
        let voice_trait: Arc<dyn VoiceEngine> = Arc::clone(&mock_voice) as Arc<dyn VoiceEngine>;
        let video_trait: Arc<dyn VideoStreamEngine> = Arc::clone(&mock_video) as Arc<dyn VideoStreamEngine>;

        let (voice_action_tx, voice_worker) = spawn_voice_action_worker(
            Arc::clone(&discord_trait),
            None,
            None,
            Arc::clone(&mock_gifs) as Arc<dyn GifProvider>,
            Arc::clone(&voice_trait),
            None,
            Arc::clone(&video_trait),
            event_tx.clone(),
            Arc::clone(&names),
        );

        let tasks = vec![
            forward_broadcast(
                mock_discord.subscribe_events(),
                event_tx.clone(),
                Arc::clone(&names),
            ),
            forward_broadcast(
                mock_voice.subscribe_voice_events(),
                event_tx.clone(),
                Arc::clone(&names),
            ),
            forward_video_frames(mock_video.subscribe_frames(), event_tx.clone()),
            voice_worker,
        ];

        let services = Self {
            discord: discord_trait,
            mock_discord: Some(mock_discord),
            rest_client: None,
            gateway: None,
            gifs: mock_gifs,
            voice: voice_trait,
            live_voice: None,
            mock_voice: Some(mock_voice),
            video: video_trait,
            live_video: None,
            mock_video: Some(mock_video),
            event_tx,
            voice_action_tx,
            names,
            tasks,
        };

        (services, event_rx)
    }

    /// Build the full production live stack (`bbs-protocol` + `bbs-voice` + `bbs-video`).
    pub async fn build_live(
        token: SecretToken,
        app_cfg: &AppConfig,
        api_secrets: &OptionalApiSecrets,
        fingerprint: DiscordFingerprint,
    ) -> Result<(Self, mpsc::UnboundedReceiver<BackendEvent>)> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (shared_bus, _) = broadcast::channel::<BackendEvent>(512);
        let names = Arc::new(NameRegistry::default());

        let rest = Arc::new(
            DiscordRestClient::new(token.clone(), fingerprint.clone())
                .context("failed to initialize Discord REST v10 client")?
                .with_event_sender(shared_bus.clone()),
        );

        let current_user = rest
            .validate_token()
            .await
            .context("Discord token validation via GET /users/@me failed")?;
        names.record_user(current_user.id, current_user.display_name());

        let (gateway_runner, gateway_handle) = DiscordGateway::new(
            token.clone(),
            fingerprint.clone(),
            GatewayConfig::default(),
            shared_bus.clone(),
        );

        // Composite GIF provider: Discord first-party proxy -> optional Klipy -> optional Giphy.
        let discord_gifs: Arc<dyn GifProvider> = Arc::new(
            DiscordGifProvider::new(token, &fingerprint)
                .context("failed to initialize Discord GIF provider")?,
        );
        let mut gif_fallbacks: Vec<Arc<dyn GifProvider>> = Vec::new();
        if let Some(klipy_key) = api_secrets.klipy_api_key.clone()
            && let Ok(provider) = KlipyGifProvider::new(klipy_key, &fingerprint)
        {
            gif_fallbacks.push(Arc::new(provider));
        }
        if let Some(giphy_key) = api_secrets.giphy_api_key.clone()
            && let Ok(provider) = GiphyGifProvider::new(giphy_key, &fingerprint)
        {
            gif_fallbacks.push(Arc::new(provider));
        }
        let gifs: Arc<dyn GifProvider> =
            Arc::new(CompositeGifProvider::new(discord_gifs, gif_fallbacks));

        // Voice engine (Guild + 1:1 / Group DM calls, DAVE MLS E2EE, Opus + cpal/PipeWire).
        let mut voice_cfg = VoiceEngineConfig::new(current_user.id, current_user.display_name());
        voice_cfg.input_mode = app_cfg.voice.resolved_input_mode();
        voice_cfg.join_muted = app_cfg.voice.join_muted;
        voice_cfg.audio_mode = AudioBackendMode::Auto;
        let voice_bridge: Arc<dyn VoiceSignalingBridge> =
            Arc::new(GatewayVoiceBridge::new(gateway_handle.clone()));
        let live_voice = Arc::new(
            DiscordVoiceEngine::new(voice_cfg, Some(voice_bridge))
                .context("failed to initialize DiscordVoiceEngine")?,
        );
        let _ = live_voice.set_master_volume(app_cfg.voice.master_volume);
        let _ = live_voice
            .set_high_pass_enabled(app_cfg.voice.high_pass_filter)
            .await;

        // Video engine (Go Live WebRTC watch & PipeWire broadcast + DAVE).
        let video_cfg = VideoEngineConfig::new(current_user.id, current_user.display_name());
        let (video_trait, live_video): (
            Arc<dyn VideoStreamEngine>,
            Option<Arc<GStreamerVideoStreamEngine>>,
        ) = match GStreamerVideoStreamEngine::new(video_cfg) {
            Ok(engine) => {
                let arc = Arc::new(engine);
                (Arc::clone(&arc) as Arc<dyn VideoStreamEngine>, Some(arc))
            }
            Err(err) => {
                warn!(error = %err, "GStreamer video initialization unavailable; falling back to mock video stub");
                let fallback = Arc::new(MockVideoStreamEngine::new());
                (fallback, None)
            }
        };

        let discord_trait: Arc<dyn DiscordBackend> = Arc::clone(&rest) as Arc<dyn DiscordBackend>;
        let voice_trait: Arc<dyn VoiceEngine> = Arc::clone(&live_voice) as Arc<dyn VoiceEngine>;

        let (voice_action_tx, voice_worker) = spawn_voice_action_worker(
            Arc::clone(&discord_trait),
            Some(Arc::clone(&rest)),
            Some(gateway_handle.clone()),
            Arc::clone(&gifs),
            Arc::clone(&voice_trait),
            Some(Arc::clone(&live_voice)),
            Arc::clone(&video_trait),
            event_tx.clone(),
            Arc::clone(&names),
        );

        let mut tasks = vec![
            forward_broadcast(shared_bus.subscribe(), event_tx.clone(), Arc::clone(&names)),
            forward_broadcast(
                live_voice.subscribe_voice_events(),
                event_tx.clone(),
                Arc::clone(&names),
            ),
            forward_video_frames(video_trait.subscribe_frames(), event_tx.clone()),
            voice_worker,
            // Forward Gateway signaling dispatches into bbs-voice and bbs-video.
            spawn_gateway_signaling_bridge(
                current_user.id,
                gateway_handle.subscribe_signaling(),
                Arc::clone(&live_voice),
                live_video.clone(),
                event_tx.clone(),
                Arc::clone(&names),
            ),
        ];

        // Forward outbound bbs-video Go Live commands onto the Main Gateway.
        if let Some(ref vid) = live_video {
            tasks.push(spawn_video_command_bridge(
                vid.subscribe_gateway_commands(),
                gateway_handle.clone(),
            ));
        }

        // Launch Main Gateway v10 WebSocket loop.
        let gw_events_tx = event_tx.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(err) = gateway_runner.run().await {
                let _ = gw_events_tx.send(BackendEvent::ErrorBanner {
                    scope: "gateway".to_string(),
                    message: format!("Gateway disconnected: {err}"),
                    recoverable: true,
                });
            }
        }));

        let services = Self {
            discord: discord_trait,
            mock_discord: None,
            rest_client: Some(rest),
            gateway: Some(gateway_handle),
            gifs,
            voice: voice_trait,
            live_voice: Some(live_voice),
            mock_voice: None,
            video: video_trait,
            live_video,
            mock_video: None,
            event_tx,
            voice_action_tx,
            names,
            tasks,
        };

        Ok((services, event_rx))
    }

    /// Seed initial `Ready`, channels, guild members, voice rosters, messages, streams, and ring
    /// state in mock mode so `--mock-demo` showcases roles, folders, muted channels, and voice rosters.
    pub async fn seed_mock_state(&self) -> Result<()> {
        let user = self.discord.current_user().await?;
        let (guilds, folders) = self.discord.list_guilds().await?;
        let dms = self.discord.list_dm_channels().await?;

        let _ = self.event_tx.send(BackendEvent::ConnectionChanged {
            state: ConnectionState::Ready,
            latency_ms: Some(24),
            detail: Some("MOCK-DEMO OFFLINE".to_string()),
        });
        let ready_ev = BackendEvent::Ready {
            current_user: user,
            guilds: guilds.clone(),
            guild_folders: folders,
            dm_channels: dms.clone(),
        };
        self.names.observe_event(&ready_ev);
        let _ = self.event_tx.send(ready_ev);

        for guild in &guilds {
            if let Ok(channels) = self.discord.list_channels(guild.id).await {
                let ev = BackendEvent::ChannelsLoaded {
                    guild_id: Some(guild.id),
                    channels,
                };
                self.names.observe_event(&ev);
                let _ = self.event_tx.send(ev);
            }
            if let Some(ref mock) = self.mock_discord
                && let Ok(members) = mock.guild_members(guild.id)
                && !members.is_empty()
            {
                let ev = BackendEvent::GuildMembersUpdated {
                    guild_id: guild.id,
                    members,
                };
                self.names.observe_event(&ev);
                let _ = self.event_tx.send(ev);
            }
        }

        if let Some(ref mock) = self.mock_discord
            && let Ok(rosters) = mock.voice_rosters()
        {
            for (guild_id, channel_id, participants) in rosters {
                let _ = self.event_tx.send(BackendEvent::VoiceRosterUpdated {
                    guild_id,
                    channel_id,
                    participants,
                });
            }
        }

        for ch_id in [
            mock_ids::CH_RETRONET_GENERAL,
            mock_ids::CH_RETRONET_MUTED_LOGS,
            mock_ids::CH_GROUP_DM_OPS,
            mock_ids::CH_DM_ALICE,
            mock_ids::CH_DM_BOB,
            mock_ids::CH_DM_CAROL,
        ] {
            if let Ok(messages) = self.discord.fetch_messages(ch_id, None, 50).await {
                let ev = BackendEvent::MessagesLoaded {
                    channel_id: ch_id,
                    messages,
                    has_more_before: false,
                };
                self.names.observe_event(&ev);
                let _ = self.event_tx.send(ev);
            }
        }

        for ch_id in [mock_ids::CH_RETRONET_VOICE, mock_ids::CH_GROUP_DM_OPS] {
            if let Ok(streams) = self.video.list_streams(ch_id).await
                && !streams.is_empty()
            {
                let _ = self.event_tx.send(BackendEvent::ActiveStreamsUpdated {
                    channel_id: ch_id,
                    streams,
                });
            }
        }

        // Seed active Group DM incoming call banner in mock mode.
        let _ = self
            .event_tx
            .send(BackendEvent::CallRingUpdated(CallRingState::IncomingRinging {
                channel_id: mock_ids::CH_GROUP_DM_OPS,
                caller_id: mock_ids::USER_ALICE,
                caller_name: "Modem Wizard".to_string(),
                is_group_dm: true,
            }));

        Ok(())
    }

    /// Update Push-to-Talk microphone transmission state asynchronously (non-blocking for UI loop).
    pub fn set_ptt_active(&self, active: bool) {
        if let Some(ref lv) = self.live_voice {
            let lv = Arc::clone(lv);
            tokio::spawn(async move {
                let _ = lv.set_ptt_active(active).await;
            });
        }
    }

    /// Dispatch a [`UiAction`] without stalling the TUI render loop.
    ///
    /// Voice & call lifecycle actions (`JoinVoice`, `RingCall`, `StopCallRing`, `LeaveVoice`,
    /// mute/deaf/volume) are routed through a strictly ordered FIFO worker so DM `JoinVoice` ->
    /// `RingCall` never race.
    pub fn dispatch_action(&self, action: UiAction) {
        if is_ordered_voice_action(&action) {
            let _ = self.voice_action_tx.send(action);
            return;
        }

        let discord = Arc::clone(&self.discord);
        let rest_client = self.rest_client.clone();
        let gateway = self.gateway.clone();
        let gifs = Arc::clone(&self.gifs);
        let voice = Arc::clone(&self.voice);
        let live_voice = self.live_voice.clone();
        let video = Arc::clone(&self.video);
        let event_tx = self.event_tx.clone();
        let names = Arc::clone(&self.names);

        tokio::spawn(async move {
            if let Err(err) = execute_ui_action(
                action,
                discord,
                rest_client,
                gateway,
                gifs,
                voice,
                live_voice,
                video,
                event_tx.clone(),
                names,
            )
            .await
            {
                let _ = event_tx.send(BackendEvent::ErrorBanner {
                    scope: "action".to_string(),
                    message: err.to_string(),
                    recoverable: true,
                });
            }
        });
    }

    /// Upload oversized composer content as a `message.txt` attachment.
    pub fn dispatch_send_as_file(
        &self,
        channel_id: Snowflake,
        content: String,
        reply_to: Option<Snowflake>,
    ) {
        let discord = Arc::clone(&self.discord);
        let rest_client = self.rest_client.clone();
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            let res = if let Some(rest) = rest_client {
                let attachment = UploadAttachment {
                    filename: "message.txt".to_string(),
                    content_type: Some("text/plain; charset=utf-8".to_string()),
                    description: Some("Oversized message exported by bbscord".to_string()),
                    bytes: content.into_bytes(),
                };
                rest.send_message_with_attachments(channel_id, "", reply_to, &[attachment])
                    .await
            } else {
                let summary: String = content.chars().take(120).collect();
                discord
                    .send_message(
                        channel_id,
                        &format!("[attachment: message.txt] {summary}..."),
                        reply_to,
                    )
                    .await
            };
            if let Err(err) = res {
                let _ = event_tx.send(BackendEvent::ErrorBanner {
                    scope: "upload".to_string(),
                    message: format!("Failed to send message.txt: {err}"),
                    recoverable: true,
                });
            }
        });
    }

    /// Gracefully tear down active voice, video, gateway, and background bridge tasks.
    pub async fn shutdown(&mut self) {
        let _ = self.voice.leave_voice().await;
        let _ = self.voice.stop_voice_message().await;
        if let Some(ref vid) = self.live_video {
            vid.shutdown().await;
        } else {
            let _ = self.video.stop_broadcast().await;
        }
        if let Some(ref gw) = self.gateway {
            let _ = gw.shutdown();
        }
        for handle in self.tasks.drain(..) {
            handle.abort();
        }
    }
}

/// Construct a default Chrome browser session fingerprint for live mode / QR login.
pub fn default_fingerprint() -> Result<DiscordFingerprint> {
    DiscordFingerprint::new(FingerprintConfig::default())
        .context("failed to initialize Discord Chrome browser fingerprint")
}

fn is_ordered_voice_action(action: &UiAction) -> bool {
    matches!(
        action,
        UiAction::JoinVoice { .. }
            | UiAction::LeaveVoice
            | UiAction::RingCall { .. }
            | UiAction::StopCallRing { .. }
            | UiAction::SetSelfMute(_)
            | UiAction::SetSelfDeaf(_)
            | UiAction::SetParticipantVolume { .. }
    )
}

#[allow(clippy::too_many_arguments)]
fn spawn_voice_action_worker(
    discord: Arc<dyn DiscordBackend>,
    rest_client: Option<Arc<DiscordRestClient>>,
    gateway: Option<GatewayHandle>,
    gifs: Arc<dyn GifProvider>,
    voice: Arc<dyn VoiceEngine>,
    live_voice: Option<Arc<DiscordVoiceEngine>>,
    video: Arc<dyn VideoStreamEngine>,
    event_tx: mpsc::UnboundedSender<BackendEvent>,
    names: Arc<NameRegistry>,
) -> (mpsc::UnboundedSender<UiAction>, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<UiAction>();
    let handle = tokio::spawn(async move {
        let mut last_failed_join: Option<Snowflake> = None;
        while let Some(action) = rx.recv().await {
            // If an outgoing DM JoinVoice just failed, do not fire the paired RingCall behind it.
            if let UiAction::RingCall { channel_id, .. } = &action
                && last_failed_join == Some(*channel_id)
            {
                last_failed_join = None;
                continue;
            }
            let join_target = match &action {
                UiAction::JoinVoice { channel_id, .. } => Some(*channel_id),
                _ => None,
            };
            let res = execute_ui_action(
                action,
                Arc::clone(&discord),
                rest_client.clone(),
                gateway.clone(),
                Arc::clone(&gifs),
                Arc::clone(&voice),
                live_voice.clone(),
                Arc::clone(&video),
                event_tx.clone(),
                Arc::clone(&names),
            )
            .await;
            match res {
                Ok(()) => {
                    if join_target.is_some() {
                        last_failed_join = None;
                    }
                }
                Err(err) => {
                    if let Some(ch) = join_target {
                        last_failed_join = Some(ch);
                    }
                    let _ = event_tx.send(BackendEvent::ErrorBanner {
                        scope: "voice".to_string(),
                        message: err.to_string(),
                        recoverable: true,
                    });
                }
            }
        }
    });
    (tx, handle)
}

#[allow(clippy::too_many_arguments)]
async fn execute_ui_action(
    action: UiAction,
    discord: Arc<dyn DiscordBackend>,
    rest_client: Option<Arc<DiscordRestClient>>,
    gateway: Option<GatewayHandle>,
    gifs: Arc<dyn GifProvider>,
    voice: Arc<dyn VoiceEngine>,
    live_voice: Option<Arc<DiscordVoiceEngine>>,
    video: Arc<dyn VideoStreamEngine>,
    event_tx: mpsc::UnboundedSender<BackendEvent>,
    names: Arc<NameRegistry>,
) -> CoreResult<()> {
    match action {
        UiAction::SelectGuild(Some(guild_id)) => {
            let channels = discord.list_channels(guild_id).await?;
            let _ = event_tx.send(BackendEvent::ChannelsLoaded {
                guild_id: Some(guild_id),
                channels,
            });
        }
        UiAction::SelectGuild(None) => {
            let dms = discord.list_dm_channels().await?;
            let _ = event_tx.send(BackendEvent::ChannelsLoaded {
                guild_id: None,
                channels: dms,
            });
        }
        UiAction::SelectChannel(channel_id) => {
            let messages = discord.fetch_messages(channel_id, None, 50).await?;
            let has_more_before = messages.len() >= 50;
            let _ = event_tx.send(BackendEvent::MessagesLoaded {
                channel_id,
                messages,
                has_more_before,
            });
            if let Ok(streams) = video.list_streams(channel_id).await
                && !streams.is_empty()
            {
                let _ = event_tx.send(BackendEvent::ActiveStreamsUpdated {
                    channel_id,
                    streams,
                });
            }
        }
        UiAction::LoadOlderMessages {
            channel_id,
            before,
            limit,
        } => {
            let messages = discord
                .fetch_messages(channel_id, Some(before), limit)
                .await?;
            let has_more_before = messages.len() >= usize::from(limit);
            let _ = event_tx.send(BackendEvent::MessagesLoaded {
                channel_id,
                messages,
                has_more_before,
            });
        }
        UiAction::SendMessage {
            channel_id,
            content,
            reply_to,
            mention_replied_user,
            attachment_paths,
        } => {
            if !attachment_paths.is_empty()
                && let Some(rest) = rest_client
            {
                let mut uploads = Vec::with_capacity(attachment_paths.len());
                for raw_path in &attachment_paths {
                    let path = Path::new(raw_path);
                    let bytes = tokio::fs::read(path).await.map_err(|err| {
                        bbs_core::CoreError::Media(format!(
                            "failed to read attachment '{}': {err}",
                            path.display()
                        ))
                    })?;
                    let filename = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("attachment.bin")
                        .to_string();
                    uploads.push(UploadAttachment {
                        filename,
                        content_type: None,
                        description: None,
                        bytes,
                    });
                }
                let _ = rest
                    .send_message_with_attachments_and_mention(
                        channel_id,
                        &content,
                        reply_to,
                        mention_replied_user,
                        &uploads,
                    )
                    .await?;
            } else {
                let _ = discord
                    .send_message_with_mention(channel_id, &content, reply_to, mention_replied_user)
                    .await?;
            }
        }
        UiAction::AckMessage {
            channel_id,
            message_id,
        } => {
            discord.ack_message(channel_id, message_id).await?;
        }
        UiAction::SendTyping { channel_id } => {
            discord.send_typing(channel_id).await?;
        }
        UiAction::RequestGuildMembers {
            guild_id,
            query,
            user_ids,
        } => {
            if let Some(ref gw) = gateway {
                gw.request_guild_members(guild_id, query, user_ids)?;
            }
        }
        UiAction::EditMessage {
            channel_id,
            message_id,
            content,
        } => {
            let _ = discord
                .edit_message(channel_id, message_id, &content)
                .await?;
        }
        UiAction::DeleteMessage {
            channel_id,
            message_id,
        } => {
            discord.delete_message(channel_id, message_id).await?;
        }
        UiAction::ToggleReaction {
            channel_id,
            message_id,
            emoji,
            add,
        } => {
            if add {
                discord.add_reaction(channel_id, message_id, &emoji).await?;
            } else {
                discord
                    .remove_reaction(channel_id, message_id, &emoji)
                    .await?;
            }
        }
        UiAction::CreateGroupDm {
            recipient_ids,
            name,
        } => {
            let _ = discord
                .create_group_dm(&recipient_ids, name.as_deref())
                .await?;
        }
        UiAction::AddGroupDmRecipient {
            channel_id,
            user_id,
        } => {
            let _ = discord.add_group_dm_recipient(channel_id, user_id).await?;
        }
        UiAction::RemoveGroupDmRecipient {
            channel_id,
            user_id,
        } => {
            let _ = discord
                .remove_group_dm_recipient(channel_id, user_id)
                .await?;
        }
        UiAction::RenameGroupDm { channel_id, name } => {
            let _ = discord.rename_group_dm(channel_id, &name).await?;
        }
        UiAction::LeaveGroupDm { channel_id } => {
            discord.leave_group_dm(channel_id).await?;
        }
        UiAction::RingCall {
            channel_id,
            recipient_ids,
        } => {
            let _ = discord
                .ring_call(channel_id, recipient_ids.as_deref())
                .await?;
        }
        UiAction::StopCallRing { channel_id } => {
            let _ = discord.stop_call_ring(channel_id).await?;
        }
        UiAction::SearchGifs { request_id, query } => {
            let trimmed = query.query.trim();
            let result = if !trimmed.is_empty() && is_direct_gif_or_webp_url(trimmed) {
                resolve_direct_gif_url(trimmed).map(|item| bbs_core::GifSearchPage {
                    items: vec![item],
                    next_cursor: None,
                })
            } else if query.trending && trimmed.is_empty() {
                gifs.trending(query.limit, query.cursor.as_deref()).await
            } else {
                gifs.search(&query).await
            };
            match result {
                Ok(page) => {
                    let _ = event_tx.send(BackendEvent::GifSearchResults { request_id, page });
                }
                Err(err) => {
                    let _ = event_tx.send(BackendEvent::GifSearchFailed {
                        request_id,
                        error: err.to_string(),
                    });
                }
            }
        }
        UiAction::SendGif {
            channel_id,
            gif,
            reply_to,
        } => {
            let _ = discord
                .send_message(channel_id, &gif.gif_url, reply_to)
                .await?;
        }
        UiAction::JoinVoice {
            guild_id,
            channel_id,
        } => {
            if let Some(ref lv) = live_voice
                && let Some(ch_name) = names.channel_name(channel_id)
            {
                lv.set_channel_name(channel_id, ch_name);
            }
            // Do NOT re-emit `BackendEvent::VoiceStatusUpdated` here: `VoiceEngine` is the sole
            // authoritative owner and emits snapshots on its event stream.
            let _ = voice.join_voice(guild_id, channel_id).await?;
            if let Some(ref gw) = gateway
                && let Some(gid) = guild_id
            {
                let _ = gw.subscribe_guild_channel(gid, channel_id, vec![[0, 99]]);
            }
            if let Ok(streams) = video.list_streams(channel_id).await
                && !streams.is_empty()
            {
                let _ = event_tx.send(BackendEvent::ActiveStreamsUpdated {
                    channel_id,
                    streams,
                });
            }
        }
        UiAction::LeaveVoice => {
            voice.leave_voice().await?;
        }
        UiAction::SetSelfMute(muted) => {
            let _ = voice.set_self_mute(muted).await?;
        }
        UiAction::SetSelfDeaf(deafened) => {
            let _ = voice.set_self_deaf(deafened).await?;
        }
        UiAction::SetParticipantVolume { user_id, volume } => {
            let _ = voice.set_participant_volume(user_id, volume).await?;
        }
        UiAction::PlayVoiceMessage {
            message_id,
            attachment,
        } => {
            if let (Some(rest), Some(lv)) = (rest_client, live_voice) {
                let (bytes, _) = rest.fetch_attachment_bytes(&attachment.url).await?;
                let dur = attachment.voice_message.as_ref().map(|vm| vm.duration_secs);
                let pb = lv
                    .player()
                    .play_bytes(message_id, attachment.id, &bytes, dur)
                    .await
                    .map_err(|err| bbs_core::CoreError::Voice(err.to_string()))?;
                let _ = event_tx.send(BackendEvent::VoicePlaybackUpdated(pb));
            } else {
                let pb = voice.play_voice_message(message_id, &attachment).await?;
                let _ = event_tx.send(BackendEvent::VoicePlaybackUpdated(pb));
            }
        }
        UiAction::StopVoiceMessage => {
            voice.stop_voice_message().await?;
        }
        UiAction::WatchStream { stream_key, sink } => {
            let _ = video.watch_stream(&stream_key, sink).await?;
        }
        UiAction::StopWatchingStream { stream_key } => {
            video.stop_watching(&stream_key).await?;
        }
        UiAction::StartBroadcast {
            guild_id,
            channel_id,
            source,
            quality,
        } => {
            let desc = video
                .start_broadcast(guild_id, channel_id, source, quality)
                .await?;
            let _ = event_tx.send(BackendEvent::ActiveStreamsUpdated {
                channel_id,
                streams: vec![desc],
            });
        }
        UiAction::StopBroadcast => {
            video.stop_broadcast().await?;
        }
        UiAction::RefreshAttachmentUrl {
            channel_id,
            message_id,
            attachment_id,
            original_url,
        } => {
            let refreshed = discord.refresh_attachment_url(&original_url).await?;
            let _ = event_tx.send(BackendEvent::AttachmentUrlRefreshed {
                channel_id,
                message_id,
                attachment_id,
                url: refreshed.refreshed_url,
                expires_at_unix: refreshed.expires_at_unix,
            });
        }
        UiAction::SwitchTheme(_) | UiAction::ViewportResized(_) | UiAction::Quit => {}
    }

    Ok(())
}

fn forward_broadcast(
    mut rx: broadcast::Receiver<BackendEvent>,
    tx: mpsc::UnboundedSender<BackendEvent>,
    names: Arc<NameRegistry>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    names.observe_event(&ev);
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    debug!(skipped, "backend event bridge lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

fn forward_video_frames(
    mut rx: broadcast::Receiver<bbs_core::VideoFrame>,
    tx: mpsc::UnboundedSender<BackendEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if tx.send(BackendEvent::VideoFrameReceived(frame)).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

fn spawn_gateway_signaling_bridge(
    local_user_id: Snowflake,
    mut rx: broadcast::Receiver<GatewaySignalingEvent>,
    voice: Arc<DiscordVoiceEngine>,
    video: Option<Arc<GStreamerVideoStreamEngine>>,
    event_tx: mpsc::UnboundedSender<BackendEvent>,
    names: Arc<NameRegistry>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let sig = match rx.recv().await {
                Ok(s) => s,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            };

            match sig {
                GatewaySignalingEvent::InitialVoiceStates { guild_id, states } => {
                    for st in states {
                        let display_name = st
                            .member
                            .as_ref()
                            .and_then(WireGuildMember::display_name)
                            .or_else(|| names.user_name(st.user_id));
                        if let Some(ref name) = display_name {
                            names.record_user(st.user_id, name);
                        }
                        let ev = VoiceStateUpdateEvent {
                            guild_id: st.guild_id.or(guild_id),
                            channel_id: st.channel_id,
                            user_id: st.user_id,
                            session_id: st.session_id,
                            self_mute: st.self_mute,
                            self_deaf: st.self_deaf,
                            mute: st.mute,
                            deaf: st.deaf,
                            self_stream: st.self_stream.unwrap_or(false),
                            display_name,
                        };
                        voice.handle_voice_state_update(ev).await;
                    }
                }
                GatewaySignalingEvent::VoiceStateUpdate(st) => {
                    let display_name = st
                        .member
                        .as_ref()
                        .and_then(WireGuildMember::display_name)
                        .or_else(|| names.user_name(st.user_id));
                    if let Some(ref name) = display_name {
                        names.record_user(st.user_id, name);
                    }
                    if st.user_id == local_user_id
                        && !st.session_id.is_empty()
                        && let Some(ref vid) = video
                    {
                        vid.set_gateway_session_id(st.session_id.clone()).await;
                    }
                    if st.self_stream.unwrap_or(false)
                        && let Some(ch_id) = st.channel_id
                        && let Some(ref vid) = video
                    {
                        let key = StreamKey::from_ids(st.guild_id, ch_id, st.user_id);
                        let owner_name = display_name
                            .clone()
                            .unwrap_or_else(|| format!("user-{}", st.user_id));
                        let desc = StreamDescriptor {
                            stream_key: key.to_canonical_string(),
                            guild_id: st.guild_id,
                            channel_id: ch_id,
                            owner_id: st.user_id,
                            owner_name,
                            title: "Go Live Screenshare".to_string(),
                            resolution: (1280, 720),
                            fps: 30,
                            viewer_count: 0,
                            has_audio: true,
                            dave_encrypted: true,
                        };
                        let _ = vid.upsert_stream(desc).await;
                        if let Ok(streams) = vid.list_streams(ch_id).await {
                            let _ = event_tx.send(BackendEvent::ActiveStreamsUpdated {
                                channel_id: ch_id,
                                streams,
                            });
                        }
                    }
                    let ev = VoiceStateUpdateEvent {
                        guild_id: st.guild_id,
                        channel_id: st.channel_id,
                        user_id: st.user_id,
                        session_id: st.session_id,
                        self_mute: st.self_mute,
                        self_deaf: st.self_deaf,
                        mute: st.mute,
                        deaf: st.deaf,
                        self_stream: st.self_stream.unwrap_or(false),
                        display_name,
                    };
                    voice.handle_voice_state_update(ev).await;
                }
                GatewaySignalingEvent::VoiceServerUpdate {
                    token,
                    guild_id,
                    channel_id,
                    endpoint,
                } => {
                    voice
                        .handle_voice_server_update(VoiceServerUpdateEvent {
                            guild_id,
                            channel_id,
                            endpoint,
                            token: Zeroizing::new(token.expose_secret().to_owned()),
                        })
                        .await;
                }
                GatewaySignalingEvent::CallCreate {
                    channel_id,
                    ringing,
                    voice_states,
                    caller_id,
                    caller_name,
                    is_group_dm,
                }
                | GatewaySignalingEvent::CallUpdate {
                    channel_id,
                    ringing,
                    voice_states,
                    caller_id,
                    caller_name,
                    is_group_dm,
                } => {
                    let connected_user_ids: Vec<Snowflake> =
                        voice_states.iter().map(|vs| vs.user_id).collect();
                    let resolved_caller_name = caller_name
                        .or_else(|| caller_id.and_then(|cid| names.user_name(cid)));
                    voice.handle_call_update(CallUpdateEvent {
                        channel_id,
                        ringing,
                        connected_user_ids,
                        caller_id,
                        caller_name: resolved_caller_name,
                        is_group_dm,
                    });
                }
                GatewaySignalingEvent::CallDelete { channel_id } => {
                    voice.handle_call_delete(channel_id);
                }
                GatewaySignalingEvent::StreamCreate {
                    stream_key,
                    rtc_server_id,
                    region,
                    viewer_ids,
                    paused,
                } => {
                    if let Some(ref vid) = video
                        && let Ok(parsed_key) = StreamKey::parse(&stream_key)
                        && let Ok(rtc_sf) = rtc_server_id.parse::<Snowflake>()
                    {
                        let _ = vid
                            .apply_gateway_event(GatewayStreamEvent::StreamCreate {
                                stream_key: parsed_key,
                                rtc_server_id: rtc_sf,
                                rtc_channel_id: None,
                                region,
                                viewer_ids,
                                paused,
                            })
                            .await;
                    }
                }
                GatewaySignalingEvent::StreamServerUpdate {
                    token,
                    stream_key,
                    endpoint,
                } => {
                    if let Some(ref vid) = video
                        && let Some(ep) = endpoint
                        && let Ok(parsed_key) = StreamKey::parse(&stream_key)
                    {
                        let _ = vid
                            .apply_gateway_event(GatewayStreamEvent::StreamServerUpdate {
                                stream_key: parsed_key,
                                endpoint: ep,
                                token: RedactedStreamToken::new(token.expose_secret().to_owned()),
                            })
                            .await;
                    }
                }
                GatewaySignalingEvent::StreamDelete { stream_key, reason } => {
                    if let Some(ref vid) = video
                        && let Ok(parsed_key) = StreamKey::parse(&stream_key)
                    {
                        let _ = vid
                            .apply_gateway_event(GatewayStreamEvent::StreamDelete {
                                stream_key: parsed_key,
                                reason,
                                unavailable: false,
                            })
                            .await;
                    }
                }
            }
        }
    })
}

fn spawn_video_command_bridge(
    mut rx: broadcast::Receiver<GatewayStreamCommand>,
    gateway: GatewayHandle,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let cmd = match rx.recv().await {
                Ok(c) => c,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let res = match cmd {
                GatewayStreamCommand::Create {
                    stream_key,
                    preferred_region,
                } => gateway.stream_create(
                    stream_key.guild_id(),
                    stream_key.channel_id(),
                    preferred_region,
                ),
                GatewayStreamCommand::Watch { stream_key } => {
                    gateway.stream_watch(stream_key.to_canonical_string())
                }
                GatewayStreamCommand::SetPaused { stream_key, paused } => {
                    gateway.stream_set_paused(stream_key.to_canonical_string(), paused)
                }
                GatewayStreamCommand::Delete { stream_key } => {
                    gateway.stream_delete(stream_key.to_canonical_string())
                }
                GatewayStreamCommand::Ping { .. } => Ok(()),
            };
            if let Err(err) = res {
                warn!(error = %err, "failed to forward Go Live command to Gateway");
            }
        }
    })
}
