//! Persistent configuration (`~/.config/bbscord/config.toml`) and `0600`-checked optional API secrets.

use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bbs_core::{CharsetMode, ColorDepth, GraphicsTier, SecretToken};
use bbs_tui::{CtrlKMode, FrameStyle};
use bbs_voice::InputMode;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// Persistent voice settings stored under `[voice]` in `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    /// Microphone activation mode: `"vad"` or `"ptt"`.
    pub input_mode: String,
    /// Voice Activity Detection threshold in dBFS (default `-42.0`).
    pub vad_threshold_db: f32,
    /// Master output volume percentage (`0..=200`, default `100`).
    pub master_volume: u8,
    /// Enable 80 Hz DC/rumble high-pass microphone filter.
    pub high_pass_filter: bool,
    /// Push-to-talk key name (used with Kitty keyboard flag 2 or XDG GlobalShortcuts).
    pub ptt_key: String,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            input_mode: "vad".to_string(),
            vad_threshold_db: -42.0,
            master_volume: 100,
            high_pass_filter: true,
            ptt_key: "v".to_string(),
        }
    }
}

impl VoiceConfig {
    /// Convert [`Self::input_mode`] into `bbs_voice::InputMode`.
    #[must_use]
    pub fn resolved_input_mode(&self) -> InputMode {
        if self.input_mode.eq_ignore_ascii_case("ptt")
            || self.input_mode.eq_ignore_ascii_case("push_to_talk")
        {
            InputMode::PushToTalk
        } else {
            InputMode::VoiceActivity {
                threshold_db: self.vad_threshold_db.clamp(-60.0, 0.0),
            }
        }
    }
}

/// Top-level user configuration persisted at `~/.config/bbscord/config.toml`.
///
/// Never stores Discord authentication tokens or third-party API keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Active theme name or slug (e.g. `"ANSI-BBS Royal"`, `"amber-phosphor"`, `"btop/dracula"`).
    pub theme: String,
    /// Whether the left guilds/channels sidebar is expanded by default.
    pub show_sidebar: bool,
    /// Whether the members pane is visible on Wide/UltraWide layouts.
    pub show_members: bool,
    /// Maximum number of UltraWide side decks (`1..=3`).
    pub max_decks: u8,
    /// UTF-8 outer frame border style (`"double"`, `"single"`, `"rounded"`, `"heavy"`, `"ascii"`).
    pub frame_style: String,
    /// Render `btop`-style positional vertical gradients on pane borders.
    pub gradient_borders: bool,
    /// Composer `Ctrl+K` policy (`"dwim"`, `"kill"`, `"switcher"`).
    pub ctrl_k: CtrlKMode,
    /// Optional charset override (`"auto"`, `"utf8"`, `"vtsafe"`, `"ascii"`).
    pub charset: Option<String>,
    /// Optional graphics tier override (`"auto"`, `"kitty-placeholder"`, `"kitty-direct"`, `"sixel"`, `"iterm2"`, `"halfblocks"`, `"tags"`).
    pub graphics: Option<String>,
    /// Optional color depth override (`"auto"`, `"truecolor"`, `"ansi256"`, `"ansi16"`, `"mono"`).
    pub color_depth: Option<String>,
    /// Enable HD44780 A02 ROM extended ASCII characters (`\` and `~`).
    pub lcd_a02: bool,
    /// Prefer inline Stage video over native pop-out `mpv` window (default `false` per §6.5).
    pub prefer_inline_video: bool,
    /// Allow inline Stage video over lower-fidelity tiers (`Sixel` / `HalfBlock`).
    pub inline_video_low_tier: bool,
    /// Allow inline Stage video over SSH connections.
    pub inline_video_ssh: bool,
    /// Voice & audio preferences.
    pub voice: VoiceConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: "ANSI-BBS Royal".to_string(),
            show_sidebar: true,
            show_members: true,
            max_decks: 3,
            frame_style: "double".to_string(),
            gradient_borders: true,
            ctrl_k: CtrlKMode::Dwim,
            charset: None,
            graphics: None,
            color_depth: None,
            lcd_a02: false,
            prefer_inline_video: false,
            inline_video_low_tier: false,
            inline_video_ssh: false,
            voice: VoiceConfig::default(),
        }
    }
}

impl AppConfig {
    /// Parse configured [`FrameStyle`].
    #[must_use]
    pub fn parsed_frame_style(&self) -> FrameStyle {
        match self.frame_style.trim().to_ascii_lowercase().as_str() {
            "single" => FrameStyle::Single,
            "rounded" => FrameStyle::Rounded,
            "heavy" => FrameStyle::Heavy,
            "ascii" => FrameStyle::Ascii,
            _ => FrameStyle::Double,
        }
    }

    /// Parse optional configured [`CharsetMode`].
    #[must_use]
    pub fn parsed_charset(&self) -> Option<CharsetMode> {
        match self.charset.as_deref()?.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(CharsetMode::Auto),
            "utf8" | "utf-8" | "rich" => Some(CharsetMode::Utf8),
            "vtsafe" | "vt" => Some(CharsetMode::VtSafe),
            "ascii" | "ascii7bit" | "7bit" => Some(CharsetMode::Ascii7Bit),
            _ => None,
        }
    }

    /// Parse optional configured [`GraphicsTier`] (`None` when unset or `"auto"`).
    #[must_use]
    pub fn parsed_graphics(&self) -> Option<GraphicsTier> {
        match self.graphics.as_deref()?.trim().to_ascii_lowercase().as_str() {
            "kitty-placeholder" | "kitty" => Some(GraphicsTier::KittyPlaceholder),
            "kitty-direct" => Some(GraphicsTier::KittyDirect),
            "sixel" => Some(GraphicsTier::Sixel),
            "iterm2" | "iterm" => Some(GraphicsTier::ITerm2),
            "halfblocks" | "halfblock" => Some(GraphicsTier::HalfBlock),
            "tags" | "text" | "text-only" => Some(GraphicsTier::TextTagOnly),
            _ => None,
        }
    }

    /// Parse optional configured [`ColorDepth`] (`None` when unset or `"auto"`).
    #[must_use]
    pub fn parsed_color_depth(&self) -> Option<ColorDepth> {
        match self
            .color_depth
            .as_deref()?
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "truecolor" | "24bit" | "rgb" => Some(ColorDepth::TrueColor),
            "ansi256" | "256" => Some(ColorDepth::Ansi256),
            "ansi16" | "16" => Some(ColorDepth::Ansi16),
            "mono" | "monochrome" => Some(ColorDepth::Monochrome),
            _ => None,
        }
    }
}

/// Optional third-party GIF API keys loaded from `0600`-checked `~/.config/bbscord/secrets.toml`.
///
/// All values are wrapped in [`SecretToken`] (zeroized on drop, never logged, never serialized).
#[derive(Clone, Default)]
pub struct OptionalApiSecrets {
    /// Optional Klipy API key for fallback GIF search.
    pub klipy_api_key: Option<SecretToken>,
    /// Optional Giphy API key for fallback GIF search.
    pub giphy_api_key: Option<SecretToken>,
}

impl fmt::Debug for OptionalApiSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OptionalApiSecrets")
            .field(
                "klipy_api_key",
                &self.klipy_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "giphy_api_key",
                &self.giphy_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Deserialize, Default)]
struct RawSecretsFile {
    klipy_api_key: Option<String>,
    giphy_api_key: Option<String>,
    #[serde(default)]
    secrets: Option<RawSecretsSection>,
}

#[derive(Deserialize, Default)]
struct RawSecretsSection {
    klipy_api_key: Option<String>,
    giphy_api_key: Option<String>,
}

/// Default configuration directory (`$XDG_CONFIG_HOME/bbscord` or `~/.config/bbscord`).
#[must_use]
pub fn default_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("bbscord")
}

/// Default configuration file path (`~/.config/bbscord/config.toml`).
#[must_use]
pub fn default_config_path() -> PathBuf {
    default_config_dir().join("config.toml")
}

/// Default optional API secrets file path (`~/.config/bbscord/secrets.toml`).
#[must_use]
pub fn default_secrets_path() -> PathBuf {
    default_config_dir().join("secrets.toml")
}

/// Load [`AppConfig`] from `path`, returning defaults cleanly if the file does not exist yet.
pub fn load_config(path: &Path) -> Result<AppConfig> {
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let mut cfg: AppConfig = toml::from_str(&text)
        .with_context(|| format!("failed to parse TOML config {}", path.display()))?;
    cfg.max_decks = cfg.max_decks.clamp(1, 3);
    cfg.voice.master_volume = cfg.voice.master_volume.min(200);
    Ok(cfg)
}

/// Atomically persist [`AppConfig`] to `path`, creating parent directories when needed.
pub fn save_config(path: &Path, cfg: &AppConfig) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }

    let rendered =
        toml::to_string_pretty(cfg).context("failed to serialize bbscord configuration to TOML")?;

    let tmp_name = format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config.toml"),
        std::process::id()
    );
    let tmp_path = path.with_file_name(tmp_name);

    {
        let mut file = fs::File::create(&tmp_path)
            .with_context(|| format!("failed to create temp config {}", tmp_path.display()))?;
        file.write_all(rendered.as_bytes())
            .with_context(|| format!("failed to write temp config {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync temp config {}", tmp_path.display()))?;
    }

    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "failed to replace config {} from {}",
            path.display(),
            tmp_path.display()
        )
    })?;
    Ok(())
}

/// Load optional third-party GIF API secrets from `path` after enforcing strict `0600` permissions on Unix.
///
/// If `path` does not exist, returns empty [`OptionalApiSecrets`]. If permissions allow group or
/// world access (`mode & 0o077 != 0`), refuses to read the file and returns an error instructing
/// the user to run `chmod 600`.
pub fn load_optional_secrets(path: &Path) -> Result<OptionalApiSecrets> {
    if !path.exists() {
        return Ok(OptionalApiSecrets::default());
    }

    let meta = fs::metadata(path)
        .with_context(|| format!("failed to inspect secrets file {}", path.display()))?;
    if !meta.is_file() {
        bail!("secrets path {} is not a regular file", path.display());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = meta.permissions().mode() & 0o777;
        if (mode & 0o077) != 0 {
            bail!(
                "refusing to load {} with overly permissive mode {:04o}; run `chmod 600 {}`",
                path.display(),
                mode,
                path.display()
            );
        }
    }

    let raw_text = Zeroizing::new(
        fs::read_to_string(path)
            .with_context(|| format!("failed to read secrets file {}", path.display()))?,
    );
    let parsed: RawSecretsFile = toml::from_str(&raw_text)
        .map_err(|_| anyhow::anyhow!("failed to parse TOML syntax in {}", path.display()))?;

    let nested = parsed.secrets.unwrap_or_default();
    let klipy_raw = parsed.klipy_api_key.or(nested.klipy_api_key);
    let giphy_raw = parsed.giphy_api_key.or(nested.giphy_api_key);

    let klipy_api_key = klipy_raw.and_then(|s| SecretToken::new(s).ok());
    let giphy_api_key = giphy_raw.and_then(|s| SecretToken::new(s).ok());

    Ok(OptionalApiSecrets {
        klipy_api_key,
        giphy_api_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "bbscord-cfg-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn config_roundtrip_and_missing_file_defaults() {
        let dir = unique_temp_dir("roundtrip");
        let path = dir.join("config.toml");

        let default_cfg = load_config(&path).unwrap();
        assert_eq!(default_cfg, AppConfig::default());

        let custom = AppConfig {
            theme: "amber-phosphor".to_string(),
            prefer_inline_video: true,
            voice: VoiceConfig {
                master_volume: 135,
                input_mode: "ptt".to_string(),
                ..VoiceConfig::default()
            },
            ..AppConfig::default()
        };
        save_config(&path, &custom).unwrap();

        let reloaded = load_config(&path).unwrap();
        assert_eq!(reloaded, custom);
        assert!(matches!(
            reloaded.voice.resolved_input_mode(),
            InputMode::PushToTalk
        ));

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn secrets_file_enforces_0600_and_redacts_debug() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = unique_temp_dir("secrets");
        let path = dir.join("secrets.toml");
        // Synthetic placeholder keys for unit test only (never real credentials).
        let payload = "klipy_api_key = \"mock-klipy-000\"\ngiphy_api_key = \"mock-giphy-000\"\n";
        fs::write(&path, payload).unwrap();

        // 0644 must be rejected
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_optional_secrets(&path).is_err());

        // 0600 must succeed and redact Debug
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let loaded = load_optional_secrets(&path).unwrap();
        assert_eq!(
            loaded.klipy_api_key.as_ref().map(SecretToken::expose_secret),
            Some("mock-klipy-000")
        );
        assert_eq!(
            loaded.giphy_api_key.as_ref().map(SecretToken::expose_secret),
            Some("mock-giphy-000")
        );
        let dbg = format!("{loaded:?}");
        assert!(!dbg.contains("mock-klipy-000"));
        assert!(!dbg.contains("mock-giphy-000"));
        assert!(dbg.contains("[REDACTED]"));

        let _ = fs::remove_dir_all(&dir);
    }
}
