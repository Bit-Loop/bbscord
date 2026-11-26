//! Platform integrations: clipboard (`wl-copy`/`xclip`/OSC 52), safe external URL open, and `$EDITOR`.

use std::env;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use bbs_runtime_term::{ResumeNotification, TtyGuard, restore_terminal};

/// Validate that a URL uses an explicit `http://` or `https://` scheme and contains no control bytes.
#[must_use]
pub fn is_safe_http_url(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return false;
    }
    let has_scheme = trimmed.starts_with("https://") || trimmed.starts_with("http://");
    has_scheme && !trimmed.bytes().any(|b| b <= 0x20 || b == 0x7F)
}

/// Build an OSC 52 clipboard write escape sequence (`ESC ] 52 ; c ; <base64> BEL`).
#[must_use]
pub fn format_osc52_clipboard(text: &str) -> Vec<u8> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07").into_bytes()
}

/// Copy `text` to the system clipboard using `wl-copy` (Wayland), `xclip`/`xsel` (X11),
/// or OSC 52 escape sequence fallback when supported by the attached terminal.
pub fn copy_to_clipboard(text: &str, osc52_supported: bool) -> Result<()> {
    if try_pipe_command("wl-copy", &[], text) {
        return Ok(());
    }
    if try_pipe_command("xclip", &["-selection", "clipboard"], text) {
        return Ok(());
    }
    if try_pipe_command("xsel", &["--clipboard", "--input"], text) {
        return Ok(());
    }
    if osc52_supported {
        let seq = format_osc52_clipboard(text);
        let mut stdout = io::stdout();
        stdout.write_all(&seq)?;
        stdout.flush()?;
        return Ok(());
    }
    bail!("no clipboard helper (`wl-copy`, `xclip`, `xsel`) found and OSC 52 is disabled")
}

fn try_pipe_command(program: &str, args: &[&str], input: &str) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };

    if let Some(mut stdin) = child.stdin.take()
        && stdin.write_all(input.as_bytes()).is_err()
    {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    }

    child.wait().is_ok_and(|status| status.success())
}

/// Open an external `http://` or `https://` URL in the user's browser via `xdg-open` (detached, quiet).
pub fn open_external_url(url: &str) -> Result<()> {
    let trimmed = url.trim();
    if !is_safe_http_url(trimmed) {
        bail!("refusing to open non-HTTP(S) or unsafe URL");
    }

    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(not(target_os = "macos"))]
    let opener = "xdg-open";

    Command::new(opener)
        .arg(trimmed)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to launch `{opener}`"))?;
    Ok(())
}

/// Suspend the TUI cleanly, edit `initial` in `$VISUAL`/`$EDITOR` (fallback `vi`), and resume.
pub fn run_external_editor(
    tty_guard: &mut TtyGuard,
    initial: &str,
) -> Result<(Option<String>, ResumeNotification)> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_path = env::temp_dir().join(format!(
        "bbscord-compose-{}-{nanos}.md",
        std::process::id()
    ));

    {
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt as _;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp_path)
                .with_context(|| format!("failed to create editor temp file {}", tmp_path.display()))?
        };
        #[cfg(not(unix))]
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .with_context(|| format!("failed to create editor temp file {}", tmp_path.display()))?;

        file.write_all(initial.as_bytes())
            .with_context(|| format!("failed to seed editor temp file {}", tmp_path.display()))?;
    }

    let editor_cmd = env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env::var("EDITOR").ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| "vi".to_string());

    restore_terminal().context("failed to restore terminal before launching $EDITOR")?;

    let mut parts = editor_cmd.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    let status_res = Command::new(program).args(parts).arg(&tmp_path).status();

    let resume = tty_guard
        .resume_after_sigcont()
        .context("failed to re-enter raw TUI mode after $EDITOR exited")?;

    let edited = match status_res {
        Ok(status) if status.success() => fs::read_to_string(&tmp_path).ok(),
        _ => None,
    };
    let _ = fs::remove_file(&tmp_path);

    Ok((edited, resume))
}

/// Parse a `±HHMM` UTC offset string (such as `"+0900"` or `"-0530"`) into signed minutes.
#[must_use]
pub fn parse_utc_offset_hhmm(raw: &str) -> Option<i32> {
    let s = raw.trim();
    if s.len() != 5 || !s.is_ascii() {
        return None;
    }
    let sign = match s.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let hours: i32 = s[1..3].parse().ok()?;
    let mins: i32 = s[3..5].parse().ok()?;
    if hours > 14 || mins >= 60 {
        return None;
    }
    Some(sign * (hours * 60 + mins))
}

/// Detect the system's current local UTC offset in minutes (`+0900` -> `540`, `-0500` -> `-300`)
/// using `#![forbid(unsafe_code)]`-safe POSIX `date +%z`, falling back to `0` (UTC) if unavailable.
#[must_use]
pub fn detect_local_utc_offset_minutes() -> i32 {
    let Ok(output) = Command::new("date")
        .arg("+%z")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return 0;
    };
    if !output.status.success() {
        return 0;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_utc_offset_hhmm(&text).unwrap_or(0)
}

static CLIPBOARD_TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Decoded system clipboard content returned by [`read_clipboard_payload`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardPayload {
    /// Image bytes extracted from the system clipboard and saved to a `0o600` temp file.
    ImageFile(PathBuf),
    /// Existing local file paths from `text/uri-list`.
    Files(Vec<PathBuf>),
    /// Plain UTF-8 clipboard text.
    Text(String),
}

/// Returns `true` if `path` is a temporary clipboard image file created by this process.
#[must_use]
pub fn is_managed_clipboard_temp(path: &Path) -> bool {
    let prefix = format!("bbscord-clip-{}-", std::process::id());
    path.parent() == Some(env::temp_dir().as_path())
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(&prefix))
}

/// Remove any leftover `bbscord-clip-<pid>-*` temporary clipboard images on clean shutdown.
pub fn cleanup_managed_clipboard_temps_for_pid() {
    let prefix = format!("bbscord-clip-{}-", std::process::id());
    if let Ok(rd) = fs::read_dir(env::temp_dir()) {
        for entry in rd.flatten() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(&prefix))
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

fn write_clipboard_temp_image(bytes: &[u8], ext: &str) -> Result<PathBuf> {
    let seq = CLIPBOARD_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = env::temp_dir().join(format!(
        "bbscord-clip-{}-{seq}-{nanos}.{ext}",
        std::process::id()
    ));
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("failed to create clipboard temp image {}", path.display()))?
    };
    #[cfg(not(unix))]
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("failed to create clipboard temp image {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("failed to write clipboard temp image {}", path.display()))?;
    Ok(path)
}

fn run_capture(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        Some(output.stdout)
    } else {
        None
    }
}

fn preferred_image_mime(targets: &str) -> Option<(&'static str, &'static str)> {
    let candidates = [
        ("image/png", "png"),
        ("image/webp", "webp"),
        ("image/jpeg", "jpg"),
        ("image/gif", "gif"),
    ];
    for (mime, ext) in candidates {
        if targets.lines().any(|line| line.trim().eq_ignore_ascii_case(mime)) {
            return Some((mime, ext));
        }
    }
    None
}

fn parse_uri_list_files(raw: &str) -> Vec<PathBuf> {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut files = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let path = bbs_tui::modals::expand_user_path(trimmed, &cwd);
        if path.is_file() {
            files.push(path);
        }
    }
    files
}

/// Read an image, file URI list, or text payload from the system clipboard (`wl-paste` on Wayland,
/// `xclip` on X11, or `pngpaste`/`pbpaste` on macOS).
pub fn read_clipboard_payload() -> Result<Option<ClipboardPayload>> {
    // 1. Wayland (`wl-paste`).
    if env::var_os("WAYLAND_DISPLAY").is_some()
        && let Some(types_bytes) = run_capture("wl-paste", &["--list-types"])
    {
        let types = String::from_utf8_lossy(&types_bytes);
        if let Some((mime, ext)) = preferred_image_mime(&types)
            && let Some(img) = run_capture("wl-paste", &["--no-newline", "--type", mime])
            && !img.is_empty()
        {
            return Ok(Some(ClipboardPayload::ImageFile(
                write_clipboard_temp_image(&img, ext)?,
            )));
        }
        if types
            .lines()
            .any(|l| l.trim().eq_ignore_ascii_case("text/uri-list"))
            && let Some(uri_bytes) =
                run_capture("wl-paste", &["--no-newline", "--type", "text/uri-list"])
        {
            let files = parse_uri_list_files(&String::from_utf8_lossy(&uri_bytes));
            if !files.is_empty() {
                return Ok(Some(ClipboardPayload::Files(files)));
            }
        }
        if let Some(txt) = run_capture("wl-paste", &["--no-newline"]) {
            let s = String::from_utf8_lossy(&txt).into_owned();
            if !s.is_empty() {
                return Ok(Some(ClipboardPayload::Text(s)));
            }
        }
        return Ok(None);
    }

    // 2. X11 (`xclip`).
    if let Some(targets_bytes) =
        run_capture("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"])
    {
        let targets = String::from_utf8_lossy(&targets_bytes);
        if let Some((mime, ext)) = preferred_image_mime(&targets)
            && let Some(img) =
                run_capture("xclip", &["-selection", "clipboard", "-t", mime, "-o"])
            && !img.is_empty()
        {
            return Ok(Some(ClipboardPayload::ImageFile(
                write_clipboard_temp_image(&img, ext)?,
            )));
        }
        if targets
            .lines()
            .any(|l| l.trim().eq_ignore_ascii_case("text/uri-list"))
            && let Some(uri_bytes) = run_capture(
                "xclip",
                &["-selection", "clipboard", "-t", "text/uri-list", "-o"],
            )
        {
            let files = parse_uri_list_files(&String::from_utf8_lossy(&uri_bytes));
            if !files.is_empty() {
                return Ok(Some(ClipboardPayload::Files(files)));
            }
        }
        if let Some(txt) = run_capture("xclip", &["-selection", "clipboard", "-o"]) {
            let s = String::from_utf8_lossy(&txt).into_owned();
            if !s.is_empty() {
                return Ok(Some(ClipboardPayload::Text(s)));
            }
        }
        return Ok(None);
    }

    // 3. macOS (`pngpaste` / `pbpaste`).
    #[cfg(target_os = "macos")]
    {
        if let Some(img) = run_capture("pngpaste", &["-"])
            && !img.is_empty()
        {
            return Ok(Some(ClipboardPayload::ImageFile(
                write_clipboard_temp_image(&img, "png")?,
            )));
        }
        if let Some(txt) = run_capture("pbpaste", &[]) {
            let s = String::from_utf8_lossy(&txt).into_owned();
            if !s.is_empty() {
                return Ok(Some(ClipboardPayload::Text(s)));
            }
        }
        return Ok(None);
    }

    bail!("no clipboard reader (`wl-paste` or `xclip`) available")
}

fn split_shell_drop_tokens(raw: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => {
                quote = None;
            }
            Some('\'') => cur.push(c),
            Some('"') => {
                if c == '\\'
                    && let Some(&next) = chars.peek()
                    && matches!(next, '"' | '\\' | '$' | '`')
                {
                    let _ = chars.next();
                    cur.push(next);
                } else {
                    cur.push(c);
                }
            }
            Some(_) => cur.push(c),
            None => match c {
                '\'' | '"' => quote = Some(c),
                '\\' => {
                    if let Some(next) = chars.next() {
                        cur.push(next);
                    }
                }
                ch if ch.is_whitespace() => {
                    if !cur.is_empty() {
                        tokens.push(std::mem::take(&mut cur));
                    }
                }
                _ => cur.push(c),
            },
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Detect drag-and-drop local file paths inside a terminal bracketed-paste string.
///
/// Returns non-empty paths ONLY when every non-empty token looks like a filesystem path (absolute,
/// `~/...`, or `file://...`) and resolves to an existing regular file on disk, so normal text,
/// code snippets, and web URLs are never hijacked.
#[must_use]
pub fn extract_dropped_file_paths(pasted: &str) -> Vec<PathBuf> {
    let trimmed = pasted.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // Try single-line / full-string path first (handles unquoted paths containing spaces).
    if !trimmed.contains('\n')
        && (trimmed.starts_with('/')
            || trimmed.starts_with("~/")
            || trimmed.starts_with("file://")
            || trimmed.starts_with("'/")
            || trimmed.starts_with("\"/")
            || trimmed.starts_with("'file://")
            || trimmed.starts_with("\"file://"))
    {
        let single = bbs_tui::modals::expand_user_path(trimmed, &cwd);
        if single.is_file() {
            return vec![single];
        }
    }

    let tokens = split_shell_drop_tokens(trimmed);
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut resolved = Vec::with_capacity(tokens.len());
    for tok in &tokens {
        let looks_like_path =
            tok.starts_with('/') || tok.starts_with("~/") || tok.starts_with("file://");
        if !looks_like_path {
            return Vec::new();
        }
        let path = bbs_tui::modals::expand_user_path(tok, &cwd);
        if !path.is_file() {
            return Vec::new();
        }
        resolved.push(path);
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_offset_parser_handles_valid_and_invalid_inputs() {
        assert_eq!(parse_utc_offset_hhmm("+0900\n"), Some(540));
        assert_eq!(parse_utc_offset_hhmm("-0530"), Some(-330));
        assert_eq!(parse_utc_offset_hhmm("+0000"), Some(0));
        assert_eq!(parse_utc_offset_hhmm("-0600"), Some(-360));
        assert_eq!(parse_utc_offset_hhmm("+1400"), Some(840));
        assert_eq!(parse_utc_offset_hhmm("+2500"), None);
        assert_eq!(parse_utc_offset_hhmm("+0560"), None);
        assert_eq!(parse_utc_offset_hhmm("UTC"), None);
    }

    #[test]
    fn safe_http_url_validation() {
        assert!(is_safe_http_url("https://github.com/Bit-Loop/bbscord"));
        assert!(is_safe_http_url("http://example.invalid/path?a=1&b=2"));
        assert!(!is_safe_http_url("file:///etc/passwd"));
        assert!(!is_safe_http_url("javascript:alert(1)"));
        assert!(!is_safe_http_url("--option-injection"));
        assert!(!is_safe_http_url("https://example.invalid/\x1b]0;pwn\x07"));
    }

    #[test]
    fn osc52_encodes_clipboard_payload() {
        let seq = format_osc52_clipboard("bbscord");
        assert_eq!(seq, b"\x1b]52;c;YmJzY29yZA==\x07");
    }

    #[test]
    fn extract_dropped_file_paths_parses_quotes_escapes_uris_and_ignores_plain_text() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = env::temp_dir().join(format!("bbscord-drop-test-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("my photo.png");
        let f2 = dir.join("notes.txt");
        fs::write(&f1, b"png").unwrap();
        fs::write(&f2, b"txt").unwrap();

        // Single quoted path with space.
        let single_quoted = format!("'{}'", f1.display());
        assert_eq!(extract_dropped_file_paths(&single_quoted), vec![f1.clone()]);

        // file:// URI with %20 space encoding.
        let uri = format!("file://{}", f1.to_string_lossy().replace(' ', "%20"));
        assert_eq!(extract_dropped_file_paths(&uri), vec![f1.clone()]);

        // Multi-file drop with backslash-escaped space + second file.
        let multi = format!(
            "{} '{}'",
            f1.to_string_lossy().replace(' ', "\\ "),
            f2.display()
        );
        assert_eq!(extract_dropped_file_paths(&multi), vec![f1.clone(), f2.clone()]);

        // Normal prose and HTTP URLs MUST NOT be treated as file drops.
        assert!(extract_dropped_file_paths("hello world").is_empty());
        assert!(extract_dropped_file_paths("https://example.com/cat.png").is_empty());
        assert!(extract_dropped_file_paths(&format!("look at {}", f1.display())).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn managed_clipboard_temp_detection_matches_only_own_pid() {
        let own = env::temp_dir().join(format!("bbscord-clip-{}-1-99.png", std::process::id()));
        let other = env::temp_dir().join("bbscord-clip-999999999-1-99.png");
        let user_file = PathBuf::from("/tmp/not-ours/photo.png");
        assert!(is_managed_clipboard_temp(&own));
        assert!(!is_managed_clipboard_temp(&other));
        assert!(!is_managed_clipboard_temp(&user_file));
    }
}
