//! Theme file loading (btop `.theme` + native TOML §8.3), source directories and the switcher
//! catalog (§8.1, §8.4).
//!
//! Everything here is defensive: unreadable directories are skipped, oversized/garbage/non-UTF-8
//! files produce a [`ThemeError`], and nothing panics.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use bbs_core::{
    Ansi16Gradient, Ansi16Palette, ColorGradient, ThemePalette, ThemeSource, ThemeSpec,
};

use super::builtin::{builtin_by_name, builtin_specs, slug};

/// Largest theme file we will read (real btop themes are ~3 KiB).
pub const MAX_THEME_FILE_BYTES: u64 = 256 * 1024;

/// Longest theme name kept from a file's `[meta] name`.
const MAX_NAME_CHARS: usize = 64;

/// btop gradient prefix ↔ bbscord gradient name (§8.5). Both spellings are accepted in
/// `[gradients]` / `[ansi16]`; the serializer writes the bbscord name.
const GRADIENT_KEYS: [(&str, &str); 9] = [
    ("temp", "latency"),
    ("cpu", "border"),
    ("download", "vu_remote"),
    ("upload", "vu_mic"),
    ("used", "unread"),
    ("process", "selection"),
    ("free", "free"),
    ("cached", "cached"),
    ("available", "available"),
];

/// Error loading or parsing a theme.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ThemeError {
    /// Filesystem error (message already includes the path).
    #[error("cannot read theme {path}: {message}")]
    Io {
        /// Offending file.
        path: PathBuf,
        /// OS error text.
        message: String,
    },
    /// File exceeds [`MAX_THEME_FILE_BYTES`].
    #[error("theme file {0} is too large")]
    TooLarge(PathBuf),
    /// btop-format parse failure.
    #[error("btop theme: {0}")]
    Btop(String),
    /// TOML syntax error.
    #[error("theme TOML: {0}")]
    Toml(String),
    /// Well-formed TOML with invalid content (bad colour, index out of range, …).
    #[error("invalid theme: {0}")]
    Invalid(String),
    /// No catalog entry / built-in with that index or name.
    #[error("theme not found: {0}")]
    NotFound(String),
}

/// `true` when the first non-blank, non-`#` line starts with `theme[` (§8.4 sniff).
#[must_use]
pub fn is_btop_format(content: &str) -> bool {
    content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .is_some_and(|l| l.starts_with("theme["))
}

/// Parse a theme file's text. btop files are detected by sniffing; everything else is native
/// TOML (§8.3). `name` is used when the file carries no `[meta] name`.
pub fn parse_theme_file(name: &str, content: &str) -> Result<ThemeSpec, ThemeError> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    if is_btop_format(content) {
        let mut spec = bbs_core::parse_btop_theme(name, content)
            .map_err(|e| ThemeError::Btop(e.to_string()))?;
        spec.name = clean_name(&spec.name, name);
        return Ok(spec);
    }
    parse_native_toml(name, content)
}

fn clean_name(raw: &str, fallback: &str) -> String {
    let n: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect();
    let n = n.trim();
    if n.is_empty() {
        fallback
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_NAME_CHARS)
            .collect()
    } else {
        n.to_string()
    }
}

fn btop_prefix(key: &str) -> Option<&'static str> {
    GRADIENT_KEYS
        .iter()
        .find(|(b, s)| *b == key || *s == key)
        .map(|(b, _)| *b)
}

fn valid_key(k: &str) -> bool {
    !k.is_empty()
        && k.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn str_value<'a>(v: &'a toml::Value, what: &str) -> Result<&'a str, ThemeError> {
    let s = v
        .as_str()
        .ok_or_else(|| ThemeError::Invalid(format!("{what} must be a string")))?;
    if s.contains(['"', '\n', '\r']) {
        return Err(ThemeError::Invalid(format!(
            "{what} contains an invalid character"
        )));
    }
    Ok(s)
}

fn parse_native_toml(name: &str, content: &str) -> Result<ThemeSpec, ThemeError> {
    let table: toml::Table =
        toml::from_str(content).map_err(|e| ThemeError::Toml(e.to_string()))?;

    let meta_name = table
        .get("meta")
        .and_then(toml::Value::as_table)
        .and_then(|m| m.get("name"))
        .and_then(toml::Value::as_str)
        .unwrap_or(name);

    // [colors] and [gradients] are lowered into btop lines so both formats share one parser
    // (identical defaults and empty-stop rules).
    let mut btop = String::new();
    if let Some(colors) = table.get("colors") {
        let colors = colors
            .as_table()
            .ok_or_else(|| ThemeError::Invalid("[colors] must be a table".into()))?;
        for (k, v) in colors {
            if !valid_key(k) {
                continue;
            }
            let s = str_value(v, &format!("colors.{k}"))?;
            let _ = writeln!(btop, "theme[{k}]=\"{s}\"");
        }
    }
    if let Some(grads) = table.get("gradients") {
        let grads = grads
            .as_table()
            .ok_or_else(|| ThemeError::Invalid("[gradients] must be a table".into()))?;
        for (k, v) in grads {
            let Some(prefix) = btop_prefix(k) else {
                continue;
            };
            let arr = v
                .as_array()
                .ok_or_else(|| ThemeError::Invalid(format!("gradients.{k} must be an array")))?;
            let stops = arr
                .iter()
                .map(|s| str_value(s, &format!("gradients.{k}")))
                .collect::<Result<Vec<_>, _>>()?;
            let (s, m, e) = match stops.as_slice() {
                [s] => (*s, "", ""),
                [s, e] => (*s, "", *e),
                [s, m, e] => (*s, *m, *e),
                _ => {
                    return Err(ThemeError::Invalid(format!(
                        "gradients.{k} needs 1 to 3 stops"
                    )));
                }
            };
            let _ = writeln!(btop, "theme[{prefix}_start]=\"{s}\"");
            let _ = writeln!(btop, "theme[{prefix}_mid]=\"{m}\"");
            let _ = writeln!(btop, "theme[{prefix}_end]=\"{e}\"");
        }
    }

    let mut palette = if btop.is_empty() {
        ThemePalette::default()
    } else {
        bbs_core::parse_btop_theme(meta_name, &btop)
            .map_err(|e| ThemeError::Invalid(e.to_string()))?
            .palette
    };

    palette.ansi16 = match table.get("ansi16") {
        None => None,
        Some(v) => {
            let t = v
                .as_table()
                .ok_or_else(|| ThemeError::Invalid("[ansi16] must be a table".into()))?;
            Some(parse_ansi16(t, &palette)?)
        }
    };

    Ok(ThemeSpec {
        name: clean_name(meta_name, name),
        source: ThemeSource::TomlFile,
        palette,
    })
}

fn ansi_index(v: &toml::Value, what: &str) -> Result<u8, ThemeError> {
    v.as_integer()
        .and_then(|i| u8::try_from(i).ok())
        .filter(|i| *i <= 15)
        .ok_or_else(|| ThemeError::Invalid(format!("ansi16.{what} must be an integer 0..=15")))
}

fn parse_ansi16(t: &toml::Table, rgb: &ThemePalette) -> Result<Ansi16Palette, ThemeError> {
    // Keys not listed fall back to the redmean-derived value for that role (§8.4).
    let mut a = Ansi16Palette::from_rgb_palette(rgb);
    for (k, v) in t {
        if k == "main_bg" {
            a.main_bg = match v {
                toml::Value::Integer(-1) => None,
                toml::Value::String(s) if s.is_empty() || s == "default" => None,
                _ => Some(ansi_index(v, k)?),
            };
            continue;
        }
        let role = match k.as_str() {
            "main_fg" => Some(&mut a.main_fg),
            "title" => Some(&mut a.title),
            "hi_fg" => Some(&mut a.hi_fg),
            "selected_bg" => Some(&mut a.selected_bg),
            "selected_fg" => Some(&mut a.selected_fg),
            "inactive_fg" => Some(&mut a.inactive_fg),
            "graph_text" => Some(&mut a.graph_text),
            "meter_bg" => Some(&mut a.meter_bg),
            "proc_misc" => Some(&mut a.proc_misc),
            "cpu_box" => Some(&mut a.cpu_box),
            "mem_box" => Some(&mut a.mem_box),
            "net_box" => Some(&mut a.net_box),
            "proc_box" => Some(&mut a.proc_box),
            "div_line" => Some(&mut a.div_line),
            _ => None,
        };
        if let Some(slot) = role {
            *slot = ansi_index(v, k)?;
            continue;
        }
        let Some(prefix) = btop_prefix(k) else {
            continue;
        };
        let arr = v
            .as_array()
            .ok_or_else(|| ThemeError::Invalid(format!("ansi16.{k} must be an array")))?;
        let idx = arr
            .iter()
            .map(|x| ansi_index(x, k))
            .collect::<Result<Vec<_>, _>>()?;
        let g = match idx.as_slice() {
            [s] => Ansi16Gradient::solid(*s),
            [s, e] => Ansi16Gradient::new_2stop(*s, *e),
            [s, m, e] => Ansi16Gradient::new_3stop(*s, *m, *e),
            _ => {
                return Err(ThemeError::Invalid(format!(
                    "ansi16.{k} needs 1 to 3 stops"
                )));
            }
        };
        *ansi_gradient_mut(&mut a, prefix) = g;
    }
    Ok(a)
}

fn ansi_gradient_mut<'a>(a: &'a mut Ansi16Palette, btop_prefix: &str) -> &'a mut Ansi16Gradient {
    match btop_prefix {
        "temp" => &mut a.temp_gradient,
        "cpu" => &mut a.cpu_gradient,
        "download" => &mut a.download_gradient,
        "upload" => &mut a.upload_gradient,
        "used" => &mut a.used_gradient,
        "process" => &mut a.process_gradient,
        "free" => &mut a.free_gradient,
        "cached" => &mut a.cached_gradient,
        _ => &mut a.available_gradient,
    }
}

fn rgb_gradient<'a>(p: &'a ThemePalette, btop_prefix: &str) -> &'a ColorGradient {
    match btop_prefix {
        "temp" => &p.temp_gradient,
        "cpu" => &p.cpu_gradient,
        "download" => &p.download_gradient,
        "upload" => &p.upload_gradient,
        "used" => &p.used_gradient,
        "process" => &p.process_gradient,
        "free" => &p.free_gradient,
        "cached" => &p.cached_gradient,
        _ => &p.available_gradient,
    }
}

fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Serialize a spec as a native §8.3 TOML theme; [`parse_theme_file`] reads it back losslessly
/// (for 1/2/3-stop gradients whose `end` is set or which are solid).
#[must_use]
pub fn to_theme_toml(spec: &ThemeSpec) -> String {
    let p = &spec.palette;
    let mut o = String::new();
    let _ = writeln!(o, "[meta]\nname = {}\n\n[colors]", toml_string(&spec.name));
    let bg = p.main_bg.map(|c| c.to_hex()).unwrap_or_default();
    let _ = writeln!(o, "main_bg = \"{bg}\"");
    for (k, c) in [
        ("main_fg", p.main_fg),
        ("title", p.title),
        ("hi_fg", p.hi_fg),
        ("selected_bg", p.selected_bg),
        ("selected_fg", p.selected_fg),
        ("inactive_fg", p.inactive_fg),
        ("graph_text", p.graph_text),
        ("meter_bg", p.meter_bg),
        ("proc_misc", p.proc_misc),
        ("cpu_box", p.cpu_box),
        ("mem_box", p.mem_box),
        ("net_box", p.net_box),
        ("proc_box", p.proc_box),
        ("div_line", p.div_line),
    ] {
        let _ = writeln!(o, "{k} = \"{}\"", c.to_hex());
    }
    o.push_str("\n[gradients]\n");
    for (b, s) in GRADIENT_KEYS {
        let g = rgb_gradient(p, b);
        let stops: Vec<String> = [Some(g.start), g.mid, g.end]
            .into_iter()
            .flatten()
            .map(|c| format!("\"{}\"", c.to_hex()))
            .collect();
        let _ = writeln!(o, "{s} = [{}]", stops.join(", "));
    }
    if let Some(a) = &p.ansi16 {
        o.push_str("\n[ansi16]\n");
        let _ = writeln!(o, "main_bg = {}", a.main_bg.map_or(-1, i16::from));
        for (k, v) in [
            ("main_fg", a.main_fg),
            ("title", a.title),
            ("hi_fg", a.hi_fg),
            ("selected_bg", a.selected_bg),
            ("selected_fg", a.selected_fg),
            ("inactive_fg", a.inactive_fg),
            ("graph_text", a.graph_text),
            ("meter_bg", a.meter_bg),
            ("proc_misc", a.proc_misc),
            ("cpu_box", a.cpu_box),
            ("mem_box", a.mem_box),
            ("net_box", a.net_box),
            ("proc_box", a.proc_box),
            ("div_line", a.div_line),
        ] {
            let _ = writeln!(o, "{k} = {v}");
        }
        let mut a = a.clone();
        for (b, s) in GRADIENT_KEYS {
            let g = *ansi_gradient_mut(&mut a, b);
            let stops: Vec<String> = [Some(g.start), g.mid, g.end]
                .into_iter()
                .flatten()
                .map(|i| i.to_string())
                .collect();
            let _ = writeln!(o, "{s} = [{}]", stops.join(", "));
        }
    }
    o
}

/// Where a catalog entry comes from (§8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeOrigin {
    /// Compiled in.
    BuiltIn,
    /// `~/.config/bbscord/themes/*.theme` (TOML or btop format).
    User,
    /// A btop theme directory (listed as `btop/<name>`).
    Btop,
}

/// Ordered list of theme source directories.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThemeDirs {
    /// `(directory, origin)` in priority order (earlier wins on duplicate names).
    pub dirs: Vec<(PathBuf, ThemeOrigin)>,
}

impl ThemeDirs {
    /// Explicit directory list (tests, `--theme-dir`).
    #[must_use]
    pub fn new(dirs: Vec<(PathBuf, ThemeOrigin)>) -> Self {
        Self { dirs }
    }

    /// Standard §8.4 sources: `~/.config/bbscord/themes`, `$XDG_CONFIG_HOME/btop/themes`,
    /// `/usr/share/btop/themes`, `/usr/local/share/btop/themes`.
    #[must_use]
    pub fn from_env() -> Self {
        let mut dirs = Vec::new();
        // `dirs::config_dir()` honours `$XDG_CONFIG_HOME` and falls back to `~/.config`.
        if let Some(cfg) = dirs::config_dir() {
            dirs.push((cfg.join("bbscord").join("themes"), ThemeOrigin::User));
            dirs.push((cfg.join("btop").join("themes"), ThemeOrigin::Btop));
        }
        dirs.push((PathBuf::from("/usr/share/btop/themes"), ThemeOrigin::Btop));
        dirs.push((
            PathBuf::from("/usr/local/share/btop/themes"),
            ThemeOrigin::Btop,
        ));
        Self { dirs }
    }
}

/// One selectable theme in the switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    /// Display name; also the name a loaded spec gets (and the config value).
    pub label: String,
    /// Source kind.
    pub origin: ThemeOrigin,
    /// File path (`None` for built-ins).
    pub path: Option<PathBuf>,
}

/// The switcher's list: built-ins (§8.2 order), then user themes, then `btop/<name>`, each file
/// group sorted case-insensitively, deduplicated by slug (first occurrence wins).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThemeCatalog {
    /// Entries in display order.
    pub entries: Vec<CatalogEntry>,
}

fn read_capped(path: &Path) -> Result<String, ThemeError> {
    let io = |e: std::io::Error| ThemeError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    };
    let f = std::fs::File::open(path).map_err(io)?;
    let meta = f.metadata().map_err(io)?;
    if !meta.is_file() {
        return Err(ThemeError::Io {
            path: path.to_path_buf(),
            message: "not a regular file".into(),
        });
    }
    if meta.len() > MAX_THEME_FILE_BYTES {
        return Err(ThemeError::TooLarge(path.to_path_buf()));
    }
    let mut buf = Vec::new();
    f.take(MAX_THEME_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(io)?;
    if buf.len() as u64 > MAX_THEME_FILE_BYTES {
        return Err(ThemeError::TooLarge(path.to_path_buf()));
    }
    // Lossy: btop themes sometimes carry Latin-1 comments; colours are ASCII anyway.
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// `[meta] name` of a native TOML theme file, if it has one.
fn peek_toml_name(path: &Path) -> Option<String> {
    let text = read_capped(path).ok()?;
    if is_btop_format(&text) {
        return None;
    }
    let t: toml::Table = toml::from_str(&text).ok()?;
    let n = t.get("meta")?.as_table()?.get("name")?.as_str()?;
    let n = clean_name(n, "");
    (!n.is_empty()).then_some(n)
}

fn theme_files(dir: &Path) -> Vec<(PathBuf, String)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(PathBuf, String)> = rd
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("theme") {
                return None;
            }
            let stem = path.file_stem()?.to_str()?.to_string();
            if stem.is_empty() || stem.chars().any(char::is_control) {
                return None;
            }
            path.is_file().then_some((path, stem))
        })
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

impl ThemeCatalog {
    /// Only the ten built-ins.
    #[must_use]
    pub fn builtin_only() -> Self {
        Self {
            entries: builtin_specs()
                .into_iter()
                .map(|s| CatalogEntry {
                    label: s.name,
                    origin: ThemeOrigin::BuiltIn,
                    path: None,
                })
                .collect(),
        }
    }

    /// Scan `dirs` (missing/unreadable directories are skipped silently).
    #[must_use]
    pub fn discover(dirs: &ThemeDirs) -> Self {
        let mut cat = Self::builtin_only();
        let mut seen: HashSet<String> = cat.entries.iter().map(|e| slug(&e.label)).collect();
        let mut user = Vec::new();
        let mut btop = Vec::new();
        for (dir, origin) in &dirs.dirs {
            for (path, stem) in theme_files(dir) {
                let (label, group) = match origin {
                    ThemeOrigin::BuiltIn => continue,
                    ThemeOrigin::User => (peek_toml_name(&path).unwrap_or(stem), &mut user),
                    ThemeOrigin::Btop => (format!("btop/{stem}"), &mut btop),
                };
                if seen.insert(slug(&label)) {
                    group.push(CatalogEntry {
                        label,
                        origin: *origin,
                        path: Some(path),
                    });
                }
            }
        }
        for g in [&mut user, &mut btop] {
            g.sort_by_key(|e| e.label.to_lowercase());
        }
        cat.entries.extend(user);
        cat.entries.extend(btop);
        cat
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when there are no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Load entry `idx`. The returned spec's `name` is always the entry label, so
    /// `position(spec.name)` finds it again.
    pub fn load(&self, idx: usize) -> Result<ThemeSpec, ThemeError> {
        let e = self
            .entries
            .get(idx)
            .ok_or_else(|| ThemeError::NotFound(format!("#{idx}")))?;
        let Some(path) = &e.path else {
            return builtin_by_name(&e.label).ok_or_else(|| ThemeError::NotFound(e.label.clone()));
        };
        let text = read_capped(path)?;
        let mut spec = parse_theme_file(&e.label, &text)?;
        spec.name.clone_from(&e.label);
        Ok(spec)
    }

    /// Index of the entry whose label (or slug, or file stem) matches `name`, case-insensitively.
    #[must_use]
    pub fn position(&self, name: &str) -> Option<usize> {
        if let Some(i) = self
            .entries
            .iter()
            .position(|e| e.label.eq_ignore_ascii_case(name))
        {
            return Some(i);
        }
        let want = slug(name);
        if want.is_empty() {
            return None;
        }
        self.entries.iter().position(|e| {
            slug(&e.label) == want
                || e.path
                    .as_deref()
                    .and_then(Path::file_stem)
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| slug(s) == want)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbs_core::RgbColor;

    const MATCHA: &str = r##"#Bashtop theme with matcha-dark-sea palette

# Main bg
theme[main_bg]=""

theme[main_fg]="#F8F8F2"
theme[title]="#F8F8F2"
theme[hi_fg]="#2eb398"
theme[selected_bg]="#0d493d"
theme[selected_fg]="#F8F8F2"
theme[inactive_fg]="#595647"
theme[graph_text]="#797667"
theme[proc_misc]="#33b165"
theme[cpu_box]="#75715E"
theme[mem_box]="#75715E"
theme[net_box]="#75715E"
theme[proc_box]="#75715E"
theme[div_line]="#595647"
theme[temp_start]="#7976B7"
theme[temp_mid]="#D8B8B2"
theme[temp_end]="#33b165"
theme[cpu_start]="#33b165"
theme[cpu_mid]="#F8F8F2" #2eb398"
theme[cpu_end]="#2eb398"
"##;

    const ADAPTA: &str = r##"# Adapta-ish (2-hex grey shorthand + trailing whitespace)
theme[main_bg]="#00"   
theme[main_fg]="#d0"
theme[title]="#ee"
theme[hi_fg]="#00bcd4"
theme[selected_bg]="#22"
theme[selected_fg]="#00bcd4"
theme[inactive_fg]="#40"
theme[cpu_start]="#00bcd4"
theme[cpu_mid]=""
theme[cpu_end]="#008ec4"
theme[used_start]="#ff0000"
theme[used_mid]="#00ff00"
theme[used_end]=""
"##;

    const DECIMAL: &str = "theme[main_bg]=\"40 42 54\"\ntheme[main_fg]=\"248 248 242\"\n";

    #[test]
    fn sniffing() {
        assert!(is_btop_format(MATCHA));
        assert!(is_btop_format("\n   \n# c\n  theme[x]=\"\""));
        assert!(!is_btop_format("[meta]\nname = \"x\"\n"));
        assert!(!is_btop_format(""));
    }

    #[test]
    fn btop_fixtures() {
        let m = parse_theme_file("matcha-dark-sea", MATCHA).unwrap();
        assert_eq!(m.source, ThemeSource::BtopThemeFile);
        assert_eq!(m.palette.main_bg, None);
        assert_eq!(
            m.palette.cpu_gradient.mid,
            Some(RgbColor::from_u32(0xF8F8F2))
        );
        assert_eq!(m.palette.hi_fg, RgbColor::from_u32(0x2eb398));

        let a = parse_theme_file("adapta", ADAPTA).unwrap();
        assert_eq!(a.palette.main_bg, Some(RgbColor::new(0, 0, 0)));
        assert_eq!(a.palette.main_fg, RgbColor::new(0xd0, 0xd0, 0xd0));
        assert_eq!(a.palette.cpu_gradient.mid, None);
        assert_eq!(
            a.palette.cpu_gradient.end,
            Some(RgbColor::from_u32(0x008ec4))
        );
        assert_eq!(
            a.palette.used_gradient,
            ColorGradient::solid(RgbColor::from_u32(0xff0000))
        );

        let d = parse_theme_file("dec", DECIMAL).unwrap();
        assert_eq!(d.palette.main_bg, Some(RgbColor::new(40, 42, 54)));
    }

    #[test]
    fn garbage_never_panics() {
        for junk in [
            "theme[",
            "theme[main_fg]",
            "theme[main_fg]=\"#zzzzzz\"",
            "theme[main_fg]=\"unterminated",
            "[meta",
            "[colors]\nmain_fg = 5",
            "[colors]\nmain_fg = \"#12345\"",
            "[gradients]\nborder = []",
            "[gradients]\nborder = [\"#000000\",\"#000000\",\"#000000\",\"#000000\"]",
            "[ansi16]\nmain_fg = 16",
            "[ansi16]\nmain_fg = -3",
            "[colors]\nmain_fg = \"#00\\\"0000\"",
            "\u{0}\u{1}\u{2}",
        ] {
            assert!(
                parse_theme_file("junk", junk).is_err(),
                "{junk:?} should fail"
            );
        }
        // Empty / meta-only TOML is valid: defaults.
        assert!(parse_theme_file("empty", "").is_ok());
    }

    #[test]
    fn native_toml_example() {
        let src = r##"
[meta]
name = "Amber Phosphor"
[colors]
main_bg = "#0D0700"
main_fg = "#FFB000"
hi_fg   = "#FFD27F"
[gradients]
border  = ["#7A4A00", "#FFB000", "#FFE6A8"]
latency = ["#112233", "#445566"]
used = ["#abcdef"]
[ansi16]
main_bg = 0
main_fg = 3
border = [3, 11]
"##;
        let s = parse_theme_file("fallback", src).unwrap();
        assert_eq!(s.name, "Amber Phosphor");
        assert_eq!(s.source, ThemeSource::TomlFile);
        assert_eq!(s.palette.main_fg, RgbColor::from_u32(0xFFB000));
        assert_eq!(s.palette.graph_text, s.palette.main_fg);
        assert_eq!(
            s.palette.cpu_gradient,
            ColorGradient::new_3stop(
                RgbColor::from_u32(0x7A4A00),
                RgbColor::from_u32(0xFFB000),
                RgbColor::from_u32(0xFFE6A8)
            )
        );
        assert_eq!(
            s.palette.temp_gradient,
            ColorGradient::new_2stop(RgbColor::from_u32(0x112233), RgbColor::from_u32(0x445566))
        );
        assert_eq!(
            s.palette.used_gradient,
            ColorGradient::solid(RgbColor::from_u32(0xabcdef))
        );
        let a = s.palette.ansi16.unwrap();
        assert_eq!((a.main_bg, a.main_fg), (Some(0), 3));
        assert_eq!(a.cpu_gradient, Ansi16Gradient::new_2stop(3, 11));
    }

    #[test]
    fn toml_round_trip_all_builtins() {
        for spec in builtin_specs() {
            let text = to_theme_toml(&spec);
            let back = parse_theme_file("x", &text)
                .unwrap_or_else(|e| panic!("{}: {e}\n{text}", spec.name));
            assert_eq!(back.name, spec.name);
            assert_eq!(back.palette, spec.palette, "{}", spec.name);
        }
        let mut odd = ThemeSpec {
            name: "We\"ird \\ name".into(),
            ..ThemeSpec::default()
        };
        odd.palette.main_bg = None;
        odd.palette.free_gradient = ColorGradient::solid(RgbColor::new(1, 2, 3));
        odd.palette.cached_gradient =
            ColorGradient::new_2stop(RgbColor::new(1, 2, 3), RgbColor::new(4, 5, 6));
        let back = parse_theme_file("x", &to_theme_toml(&odd)).unwrap();
        assert_eq!(back.name, odd.name);
        assert_eq!(back.palette, odd.palette);
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let p = std::env::temp_dir().join(format!(
                "bbs-tui-theme-{tag}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn catalog_discovery_on_temp_dirs() {
        let root = TempDir::new("cat");
        let user = root.0.join("user");
        let btop1 = root.0.join("btop1");
        let btop2 = root.0.join("btop2");
        for d in [&user, &btop1, &btop2] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            user.join("mine.theme"),
            "[meta]\nname = \"Zed Custom\"\n[colors]\nmain_fg = \"#ffffff\"\n",
        )
        .unwrap();
        std::fs::write(user.join("alpha.theme"), MATCHA).unwrap();
        std::fs::write(user.join("nord.theme"), MATCHA).unwrap(); // shadowed by built-in Nord
        std::fs::write(user.join("broken.theme"), "theme[oops").unwrap();
        std::fs::write(user.join("ignored.txt"), MATCHA).unwrap();
        std::fs::write(btop1.join("matcha-dark-sea.theme"), MATCHA).unwrap();
        std::fs::write(btop2.join("matcha-dark-sea.theme"), ADAPTA).unwrap(); // dup: first wins
        std::fs::write(btop2.join("adapta.theme"), ADAPTA).unwrap();
        std::fs::write(
            btop2.join("huge.theme"),
            vec![b'#'; (MAX_THEME_FILE_BYTES + 10) as usize],
        )
        .unwrap();

        let dirs = ThemeDirs::new(vec![
            (user, ThemeOrigin::User),
            (root.0.join("missing"), ThemeOrigin::User),
            (btop1, ThemeOrigin::Btop),
            (btop2, ThemeOrigin::Btop),
        ]);
        let cat = ThemeCatalog::discover(&dirs);
        let labels: Vec<&str> = cat.entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(
            &labels[..10],
            &ThemeCatalog::builtin_only()
                .entries
                .iter()
                .map(|e| e.label.as_str())
                .collect::<Vec<_>>()[..]
        );
        assert_eq!(
            &labels[10..],
            [
                "alpha",
                "broken",
                "Zed Custom",
                "btop/adapta",
                "btop/huge",
                "btop/matcha-dark-sea"
            ]
        );

        let i = cat.position("btop/matcha-dark-sea").unwrap();
        let s = cat.load(i).unwrap();
        assert_eq!(s.name, "btop/matcha-dark-sea");
        assert_eq!(s.palette.main_bg, None); // MATCHA, not ADAPTA
        assert_eq!(cat.position(&s.name), Some(i));

        let z = cat.position("zed custom").unwrap();
        assert_eq!(cat.load(z).unwrap().name, "Zed Custom");
        assert_eq!(cat.position("mine"), Some(z)); // file stem also matches

        assert!(cat.load(cat.position("broken").unwrap()).is_err());
        assert!(matches!(
            cat.load(cat.position("btop/huge").unwrap()),
            Err(ThemeError::TooLarge(_))
        ));
        assert!(cat.load(999).is_err());
        assert_eq!(
            cat.load(cat.position("amber-phosphor").unwrap())
                .unwrap()
                .name,
            "Amber Phosphor"
        );
        assert_eq!(cat.position("does-not-exist"), None);
    }

    #[test]
    fn from_env_has_standard_dirs() {
        let d = ThemeDirs::from_env();
        assert!(
            d.dirs
                .iter()
                .any(|(p, o)| *o == ThemeOrigin::Btop && p == Path::new("/usr/share/btop/themes"))
        );
        assert!(d.dirs.iter().any(
            |(p, o)| *o == ThemeOrigin::Btop && p == Path::new("/usr/local/share/btop/themes")
        ));
    }

    #[test]
    fn host_btop_themes_load_if_present() {
        let cat = ThemeCatalog::discover(&ThemeDirs::new(vec![(
            PathBuf::from("/usr/share/btop/themes"),
            ThemeOrigin::Btop,
        )]));
        for i in 10..cat.len() {
            assert!(cat.load(i).is_ok(), "{}", cat.entries[i].label);
        }
    }
}
