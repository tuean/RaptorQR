//! System clipboard access (no extra dependencies).
//!
//! The gpui receiver writes to the clipboard through gpui itself; the headless
//! receiver has no gpui, so it shells out to the platform tool — the same
//! approach [`crate::notify`] takes for notifications.
//!
//! Copying never fails a transfer: when no clipboard tool is available the
//! caller still shows/prints the text and reports that copying did not work.

use std::io::Write;
use std::process::{Command, Stdio};

/// Copy `text` to the system clipboard. `false` = no clipboard tool worked.
pub fn copy_text(text: &str) -> bool {
    // An empty payload would silently wipe whatever the user had copied.
    if text.is_empty() {
        return false;
    }

    if cfg!(target_os = "macos") {
        return pipe_to("pbcopy", &[], text);
    }
    if cfg!(windows) {
        // `clip.exe` would read the bytes in the OEM code page and mangle
        // anything non-ASCII, so force PowerShell's stdin to UTF-8 instead.
        return pipe_to(
            "powershell",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::InputEncoding=[Text.Encoding]::UTF8; $input | Set-Clipboard",
            ],
            text,
        );
    }
    // Linux: Wayland first, X11 as the fallback.
    pipe_to("wl-copy", &[], text) || pipe_to("xclip", &["-selection", "clipboard"], text)
}

/// Feed `text` to a tool on stdin and report whether it exited successfully.
fn pipe_to(program: &str, args: &[&str], text: &str) -> bool {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // `pbcopy` decodes stdin using the *current locale's* encoding, and under a
    // plain `C` locale (common when the receiver is started from a GUI app or a
    // minimal shell) it silently drops every non-ASCII byte — Chinese text would
    // reach the clipboard as an empty string. Our payload is always UTF-8, so
    // say so explicitly instead of inheriting whatever the caller had.
    #[cfg(target_os = "macos")]
    command.env("LC_CTYPE", "UTF-8");

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return false,
    };

    let wrote = match child.stdin.take() {
        // The handle is dropped at the end of this arm, closing the pipe so the
        // tool sees EOF and finishes.
        Some(mut stdin) => stdin.write_all(text.as_bytes()).is_ok(),
        None => false,
    };

    wrote && child.wait().map(|status| status.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read the clipboard back (macOS/Linux helpers for the test below).
    #[cfg(target_os = "macos")]
    fn read_pasteboard() -> String {
        let out = std::process::Command::new("pbpaste")
            .env("LC_CTYPE", "UTF-8")
            .output()
            .expect("pbpaste");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Real round-trip through the system clipboard.
    ///
    /// Ignored by default: it overwrites the clipboard of whoever runs the test
    /// suite. Run it explicitly with
    /// `cargo test --lib clipboard -- --ignored`.
    ///
    /// It guards a subtle one: `pbcopy` drops non-ASCII input unless the locale
    /// says UTF-8, which would turn received Chinese text into an empty paste.
    #[test]
    #[ignore = "overwrites the developer's clipboard"]
    #[cfg(target_os = "macos")]
    fn copies_non_ascii_text_to_the_system_clipboard() {
        let text = "中文测试 ABC more";
        assert!(copy_text(text), "pbcopy must be available");
        assert_eq!(read_pasteboard(), text);
    }

    #[test]
    fn empty_text_is_not_copied() {
        // Copying "" would silently wipe whatever the user had.
        assert!(!copy_text(""));
    }
}
