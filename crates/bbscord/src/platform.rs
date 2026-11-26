//! Platform integrations: clipboard (`wl-copy`/`xclip`/OSC 52), safe external URL open, and `$EDITOR`.

use std::env;
use std::fs;
use std::io::{self, Write as _};
use std::process::{Command, Stdio};
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
