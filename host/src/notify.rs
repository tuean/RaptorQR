//! Completion notifications (no extra dependencies).
//!
//! A transfer finishes on a background thread and the user is usually looking
//! at the sender, not at this window — so the save is announced with a system
//! notification and an in-app highlight.

/// Escape a Rust string for an AppleScript string literal (macOS only).
///
/// Filenames come from the **sender** and are therefore untrusted: without
/// escaping, a name like `x" & (do shell script "rm -rf ~") & "` would be
/// evaluated as AppleScript. Backslashes must be escaped first.
#[cfg(not(windows))]
fn applescript_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            // Newlines would end the statement early.
            '\n' | '\r' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Show a system notification, if notifications are enabled for this build.
///
/// macOS: `osascript`. Windows: not implemented yet — the console receiver is
/// the only feedback there, so this is a no-op instead of a failure.
///
/// Runs detached so the caller (capture/decode path) is never blocked, and
/// never fails the transfer: notification problems are printed in debug mode.
pub fn notify_transfer_received(filename: &str, path: &str, bytes: usize) {
    show(
        "RaptorQR — 文件已接收",
        format!("{filename} · {} → {path}", human_bytes(bytes)),
    );
}

/// Announce that a text transfer arrived and is on the clipboard.
///
/// Text is not saved to disk, so this is the only out-of-window signal that
/// something arrived — it carries the first line as a preview.
pub fn notify_text_received(text: &str) {
    show(
        "RaptorQR — 文本已复制到剪贴板",
        format!("{} · {} 字符", first_line(text), text.chars().count()),
    );
}

/// First line of `text`, trimmed and truncated for a notification body.
fn first_line(text: &str) -> String {
    const MAX_CHARS: usize = 80;
    let line = text.lines().next().unwrap_or("").trim();
    let mut chars = line.chars();
    let head: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Fire a system notification (no-op on Windows, where only the console reports).
///
/// Runs detached so the caller (capture/decode path) is never blocked, and
/// never fails the transfer: notification problems are printed in debug mode.
fn show(title: &str, body: String) {
    if std::env::var_os("RAPTORQR_NO_NOTIFY").is_some() {
        return;
    }

    if cfg!(windows) {
        if std::env::var_os("RAPTORQR_DEBUG").is_some() {
            eprintln!("[notify] {title}: {body} (no Windows notifier built in)");
        }
        return;
    }

    #[cfg(not(windows))]
    {
        let title = title.to_string();
        std::thread::spawn(move || {
            let script = format!(
                "display notification {} with title {} sound name \"Glass\"",
                applescript_literal(&body),
                applescript_literal(&title),
            );
            match std::process::Command::new("osascript")
                .arg("-e")
                .arg(&script)
                .output()
            {
                Ok(output) if output.status.success() => {}
                Ok(output) => {
                    if std::env::var_os("RAPTORQR_DEBUG").is_some() {
                        eprintln!(
                            "[notify] osascript failed: {}",
                            String::from_utf8_lossy(&output.stderr).trim()
                        );
                    }
                }
                Err(err) => {
                    if std::env::var_os("RAPTORQR_DEBUG").is_some() {
                        eprintln!("[notify] could not run osascript: {err}");
                    }
                }
            }
        });
    }
}

/// `1.4 MB` / `812 KB` / `313 B`
pub fn human_bytes(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KB * KB * KB {
        format!("{:.1} GB", value / (KB * KB * KB))
    } else if value >= KB * KB {
        format!("{:.1} MB", value / (KB * KB))
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(windows))]
    fn escapes_applescript_strings() {
        assert_eq!(applescript_literal("plain.txt"), "\"plain.txt\"");
        // A hostile filename must not be able to close the literal.
        assert_eq!(
            applescript_literal("a\" & (do shell script \"id\") & \""),
            "\"a\\\" & (do shell script \\\"id\\\") & \\\"\""
        );
        assert_eq!(applescript_literal("back\\slash"), "\"back\\\\slash\"");
        // Newlines cannot break out onto a new statement.
        assert_eq!(applescript_literal("two\nlines"), "\"two lines\"");
        assert_eq!(applescript_literal("中文名.txt"), "\"中文名.txt\"");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(human_bytes(313), "313 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(1024 * 1024 * 3 / 2), "1.5 MB");
    }

    /// The notification body for text is one line — a multi-line payload must
    /// not spill a thousand lines into a banner.
    #[test]
    fn text_preview_is_a_single_truncated_line() {
        assert_eq!(first_line("SELECT *\nFROM t\nWHERE x = 1"), "SELECT *");
        assert_eq!(first_line("  padded \nmore"), "padded");
        assert_eq!(first_line(""), "");
        assert_eq!(first_line("\n\n"), "");

        let long = "字".repeat(200);
        let preview = first_line(&long);
        assert_eq!(preview.chars().count(), 81, "80 chars + ellipsis");
        assert!(preview.ends_with('…'));
    }
}
