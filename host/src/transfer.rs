//! Transfer decode pipeline: feed raw QR bytes (transport packets), drive the
//! RaptorQ session, and hand back a finished file/text payload.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::protocol::{parse_packet, RAPTORQ_SYMBOL_INDEX};
use crate::raptorq::{unwrap_payload, RaptorQSession, SessionKey};

/// A fully decoded transfer.
#[derive(Debug, Clone)]
pub struct CompletedTransfer {
    pub filename: String,
    pub body: Vec<u8>,
    pub is_text: bool,
}

impl CompletedTransfer {
    /// Where this transfer is supposed to go.
    ///
    /// Text is meant to be pasted, not filed: writing it to
    /// `~/Downloads/raptorqr-text.txt` only makes the user open a file to copy
    /// its contents out. Text therefore goes to the clipboard (and the UI), and
    /// only binary payloads are written to disk.
    pub fn delivery(&self) -> Delivery {
        if self.is_text {
            Delivery::Clipboard
        } else {
            Delivery::File
        }
    }

    /// The text of a text transfer (lossily decoded); `None` for files.
    pub fn as_text(&self) -> Option<String> {
        match self.delivery() {
            Delivery::Clipboard => Some(String::from_utf8_lossy(&self.body).into_owned()),
            Delivery::File => None,
        }
    }
}

/// How a finished transfer reaches the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Hand the decoded text to the clipboard — never written to disk.
    Clipboard,
    /// Write the payload to a file.
    File,
}

/// Display label for text transfers (there is no filename to show).
pub const TEXT_LABEL: &str = "文本";

/// A transfer that finished, as shown by the UI.
#[derive(Debug, Clone)]
pub struct TransferStatus {
    pub key: SessionKey,
    pub progress: f64,
    pub blocks: Vec<(u8, u32, u32)>,
    /// Set once the transfer is complete: (label, saved path, bytes, delivery).
    /// Text transfers are never written, so their path stays empty.
    pub complete: Option<(String, PathBuf, usize, Delivery)>,
}

/// How long after the last packet of a finished transfer the same
/// (length, payload-size) key is still treated as the same looping stream.
/// A longer silence means the sender stopped, so a manual re-send is allowed.
const STREAM_IDLE_RESET: Duration = Duration::from_secs(5);

struct CompletedState {
    key: SessionKey,
    filename: String,
    path: PathBuf,
    bytes: usize,
    blocks: Vec<(u8, u32, u32)>,
    delivery: Delivery,
    last_seen: Instant,
}

/// Outcome of feeding one decoded QR symbol's bytes.
pub enum FeedResult {
    /// Not a valid RaptorQ transport packet (wrong magic/CRC/symbol index).
    Ignored,
    /// A valid packet was accepted; the transfer is still in progress.
    Progress,
    /// The transfer completed.
    Completed(CompletedTransfer),
    /// The transfer completed again with exactly the same content: the sender
    /// keeps looping the same QR stream, so this must not be saved twice.
    Duplicate,
}

/// Tracks the current transfer and decodes symbols as they arrive.
pub struct TransferTracker {
    session: Option<(SessionKey, RaptorQSession)>,
    /// Monotonic id of the session in progress, for diagnostics.
    session_seq: u64,
    /// Last fully decoded transfer, used to suppress re-saving the same file
    /// when the sender's stream loops.
    last_completed: Option<(SessionKey, Vec<u8>)>,
    /// The finished transfer whose stream is still looping: its packets are
    /// ignored instead of being decoded all over again.
    completed: Option<CompletedState>,
}

impl TransferTracker {
    pub fn new() -> Self {
        TransferTracker {
            session: None,
            session_seq: 0,
            last_completed: None,
            completed: None,
        }
    }

    /// Feed raw bytes decoded from one QR symbol.
    pub fn feed(&mut self, raw: &[u8]) -> FeedResult {
        let packet = match parse_packet(raw) {
            Ok(p) => p,
            Err(_) => return FeedResult::Ignored,
        };
        if packet.header.symbol_index != RAPTORQ_SYMBOL_INDEX {
            return FeedResult::Ignored;
        }
        if packet.payload.len() <= 4 {
            return FeedResult::Ignored;
        }

        let key = SessionKey {
            data_length: packet.header.data_length,
            transport_payload_size: packet.payload.len(),
        };

        // A finished transfer keeps looping on the sender: ignore its packets
        // (the file is already received) instead of decoding it again.
        if let Some(done) = &mut self.completed {
            if done.key == key && done.last_seen.elapsed() < STREAM_IDLE_RESET {
                done.last_seen = Instant::now();
                return FeedResult::Duplicate;
            }
            self.completed = None;
        }

        // Start a new session when metadata differs from the current one.
        let needs_new = match &self.session {
            Some((k, _)) => *k != key,
            None => true,
        };
        if needs_new {
            if std::env::var_os("RAPTORQR_DEBUG").is_some() {
                eprintln!(
                    "[recv] new session #{} key={key:?} (previous={:?})",
                    self.session_seq + 1,
                    self.session.as_ref().map(|(k, _)| *k)
                );
            }
            self.session_seq += 1;
            self.session = Some((
                key,
                RaptorQSession::new(key.data_length as u64, key.transport_payload_size),
            ));
        }

        // Decode inside a scope so the session borrow ends before it is
        // replaced below.
        let (decoded, blocks) = {
            let (_, session) = self.session.as_mut().unwrap();
            match session.push(&packet.payload) {
                Some(decoded) => (decoded, session.per_block_progress()),
                None => return FeedResult::Progress,
            }
        };

        {
            let (filename, body) = unwrap_payload(
                decoded,
                packet.header.compressed,
                packet.header.is_text,
            );
            let filename = if filename.is_empty() {
                if packet.header.is_text {
                    TEXT_LABEL.to_string()
                } else {
                    "raptorqr-file.bin".to_string()
                }
            } else {
                filename
            };
            self.session = None; // next differing packet starts a fresh session

            // The sender loops its stream, so the very same transfer decodes
            // again on every pass — only report it once.
            let repeated = self
                .last_completed
                .as_ref()
                .is_some_and(|(done_key, done_body)| *done_key == key && done_body == &body);
            self.last_completed = Some((key, body.clone()));

            // Stop decoding this transfer: mark it done before reporting, so
            // the rest of the loop pass is skipped without any FEC work.
            self.completed = Some(CompletedState {
                key,
                filename: filename.clone(),
                path: PathBuf::new(),
                bytes: 0,
                blocks,
                delivery: if packet.header.is_text {
                    Delivery::Clipboard
                } else {
                    Delivery::File
                },
                last_seen: Instant::now(),
            });

            if repeated {
                return FeedResult::Duplicate;
            }

            return FeedResult::Completed(CompletedTransfer {
                filename,
                body,
                is_text: packet.header.is_text,
            });
        }
    }

    /// Progress of the transfer in flight — or of the one that just finished,
    /// so the UI keeps showing 100% while the sender loops.
    pub fn status(&self) -> Option<TransferStatus> {
        if let Some((key, session)) = &self.session {
            return Some(TransferStatus {
                key: *key,
                progress: session.progress(),
                blocks: session.per_block_progress(),
                complete: None,
            });
        }
        self.completed.as_ref().map(|done| TransferStatus {
            key: done.key,
            progress: 1.0,
            blocks: done.blocks.clone(),
            complete: Some((
                done.filename.clone(),
                done.path.clone(),
                done.bytes,
                done.delivery,
            )),
        })
    }

    /// Record where a finished transfer was saved (called by the receiver).
    pub fn mark_saved(&mut self, path: PathBuf, bytes: usize) {
        if let Some(done) = &mut self.completed {
            done.path = path;
            done.bytes = bytes;
        }
    }

    /// Key of the transfer in progress, for diagnostics.
    pub fn session_key(&self) -> Option<SessionKey> {
        self.session.as_ref().map(|(key, _)| *key)
    }

    /// Id of the session in progress (increments whenever it is recreated).
    pub fn session_seq(&self) -> u64 {
        self.session_seq
    }

    /// Accepted (block, ESI) pairs of the transfer in progress.
    pub fn received_esis(&self) -> Vec<(u8, u32)> {
        self.session
            .as_ref()
            .map(|(_, session)| session.received_esis())
            .unwrap_or_default()
    }

    pub fn reset(&mut self) {
        self.session = None;
        self.completed = None;
    }
}

impl Default for TransferTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Save `body` into the user's Downloads directory, never overwriting.
///
/// If a file with this name already exists **with identical content** it is
/// left alone (and its path returned) instead of piling up `file (1)`,
/// `file (2)`, … This also keeps restarts idempotent.
pub fn save_to_downloads(filename: &str, body: &[u8]) -> (PathBuf, bool) {
    save_in(&downloads_dir(), filename, body)
}

/// [`save_to_downloads`] into an explicit directory — used by the headless
/// receiver's `--out DIR`.
pub fn save_to_dir(dir: &std::path::Path, filename: &str, body: &[u8]) -> (PathBuf, bool) {
    save_in(dir, filename, body)
}

/// Persist a finished transfer — unless it is text.
///
/// This is the only place receiver front-ends write received payloads, so the
/// "text never touches the disk" rule lives here instead of in every caller:
/// text returns `None` and the caller hands [`CompletedTransfer::as_text`] to the
/// clipboard / the UI instead. Files return the path and whether new bytes were
/// written (`false` = an identical file was already there).
pub fn save_completed(
    transfer: &CompletedTransfer,
    dir: Option<&std::path::Path>,
) -> Option<(PathBuf, bool)> {
    if transfer.delivery() == Delivery::Clipboard {
        return None;
    }
    Some(match dir {
        Some(dir) => save_to_dir(dir, &transfer.filename, &transfer.body),
        None => save_to_downloads(&transfer.filename, &transfer.body),
    })
}

/// [`save_to_downloads`] against an explicit directory (testable).
/// Returns the path and whether a new file was actually written.
fn save_in(dir: &std::path::Path, filename: &str, body: &[u8]) -> (PathBuf, bool) {
    let dir = dir.to_path_buf();
    let filename = match sanitize_filename(filename) {
        name if name.is_empty() => "raptorqr-file.bin".to_string(),
        name => name,
    };
    let existing = dir.join(&filename);
    if fs::read(&existing).is_ok_and(|bytes| bytes == body) {
        return (existing, false);
    }
    let path = unique_path(&dir, &filename);
    match fs::write(&path, body) {
        Ok(()) => (path, true),
        Err(err) => {
            // A failed write must never take the receiver thread down with it:
            // the transfer is already decoded, losing the thread loses every
            // later transfer too. Retry once under a name we fully control.
            let fallback = dir.join(format!("raptorqr-recv-{}.bin", std::process::id()));
            match fs::write(&fallback, body) {
                Ok(()) => {
                    eprintln!(
                        "[recv] cannot write {} ({err}) — saved as {} instead",
                        path.display(),
                        fallback.display()
                    );
                    (fallback, true)
                }
                Err(retry) => {
                    eprintln!(
                        "[recv] cannot write {} ({err}) nor {} ({retry})",
                        path.display(),
                        fallback.display()
                    );
                    (path, false)
                }
            }
        }
    }
}

/// Longest path component we will ever hand to the filesystem. APFS/HFS+ allow
/// 255 bytes per component; stay below that to leave room for ` (1)` suffixes.
const MAX_FILENAME_BYTES: usize = 200;

/// Make a sender-supplied name safe to use as a single path component.
///
/// The name comes from the sender (untrusted), and in the worst case is a whole
/// payload: keep only the last component so it cannot escape `dir`, drop control
/// characters, and cap the byte length while preserving the extension. Returns
/// an empty string when nothing usable is left, so callers can substitute a
/// fixed fallback name.
fn sanitize_filename(name: &str) -> String {
    let name = name.rsplit(|c| c == '/' || c == '\\').next().unwrap_or("");
    let name: String = name.chars().filter(|c| !c.is_control()).collect();
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." {
        return String::new();
    }
    if name.len() <= MAX_FILENAME_BYTES {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        // Only treat a short tail as an extension (`report.tar.gz` → `.gz`).
        Some(dot) if dot > 0 && name.len() - dot <= 16 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    let budget = MAX_FILENAME_BYTES.saturating_sub(ext.len());
    let mut cut = budget.min(stem.len());
    while cut > 0 && !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{}", &stem[..cut], ext)
}

fn downloads_dir() -> PathBuf {
    // macOS/Linux use $HOME; Windows only sets %USERPROFILE%.
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    let candidate = PathBuf::from(&home).join("Downloads");
    if candidate.is_dir() {
        candidate
    } else {
        PathBuf::from(&home)
    }
}

/// Append ` (1)`, ` (2)`, … before the extension until the path is free.
pub fn unique_path(dir: &PathBuf, filename: &str) -> PathBuf {
    let direct = dir.join(filename);
    if !direct.exists() {
        return direct;
    }
    let stem = std::path::Path::new(filename)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let ext = std::path::Path::new(filename)
        .extension()
        .map(|s| s.to_string_lossy().into_owned());
    for i in 1..10_000 {
        let candidate = match &ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        let p = dir.join(candidate);
        if !p.exists() {
            return p;
        }
    }
    dir.join(format!("{stem}-{}.bin", std::process::id()))
}

/// Pure decode helper for tests: returns (filename, body, is_text) once the
/// transfer completes across the given transport packets.
pub fn decode_packets(packets: &[Vec<u8>]) -> Option<CompletedTransfer> {
    let mut tracker = TransferTracker::new();
    for p in packets {
        if let FeedResult::Completed(t) = tracker.feed(p) {
            return Some(t);
        }
    }
    None
}

/// Like [`decode_packets`], but reports how many times the (looping) stream
/// produced a complete transfer.
pub fn decode_packets_counting_repeats(packets: &[Vec<u8>]) -> (Option<CompletedTransfer>, usize) {
    let mut tracker = TransferTracker::new();
    let mut first = None;
    let mut repeats = 0usize;
    for packet in packets {
        match tracker.feed(packet) {
            FeedResult::Completed(t) => {
                if first.is_none() {
                    first = Some(t);
                }
            }
            FeedResult::Duplicate => repeats += 1,
            _ => {}
        }
    }
    (first, repeats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "raptorqr-{tag}-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The sender loops its stream, so the receiver decodes the same transfer
    /// again and again — it must be saved exactly once.
    #[test]
    fn identical_resend_is_not_saved_again() {
        let dir = temp_dir("resend");
        let body = b"the same file over and over";

        let (first, wrote_first) = save_in(&dir, "loop.bin", body);
        let (second, wrote_second) = save_in(&dir, "loop.bin", body);
        let (third, wrote_third) = save_in(&dir, "loop.bin", body);

        assert!(wrote_first, "the first save must write the file");
        assert!(!wrote_second && !wrote_third, "repeats must not rewrite");
        assert_eq!(first, second);
        assert_eq!(second, third);
        assert!(!dir.join("loop (1).bin").exists(), "a duplicate was written");

        let entries: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(entries.len(), 1, "expected exactly one file on disk");

        // Different content under the same name still gets its own file.
        let (changed, wrote_changed) = save_in(&dir, "loop.bin", b"a different file");
        assert!(wrote_changed);
        assert_eq!(changed.file_name().unwrap(), "loop (1).bin");
        assert_eq!(fs::read(&changed).unwrap(), b"a different file");
        assert_eq!(fs::read(&first).unwrap(), body);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unique_path_never_overwrites() {
        let dir = temp_dir("unique-path");
        let first = unique_path(&dir, "report.txt");
        assert_eq!(first.file_name().unwrap(), "report.txt");
        fs::write(&first, b"one").unwrap();

        let second = unique_path(&dir, "report.txt");
        assert_eq!(second.file_name().unwrap(), "report (1).txt");
        fs::write(&second, b"two").unwrap();

        let third = unique_path(&dir, "report.txt");
        assert_eq!(third.file_name().unwrap(), "report (2).txt");

        // Suffix goes before the extension, and stays inside the directory.
        assert_eq!(third.parent().unwrap(), dir.as_path());
        assert_eq!(fs::read(&first).unwrap(), b"one");

        fs::remove_dir_all(&dir).unwrap();
    }

    /// Sender-supplied names are untrusted: only the last component survives,
    /// control characters go away, and the length stays inside one component.
    #[test]
    fn filename_is_sanitized_before_touching_the_filesystem() {
        assert_eq!(sanitize_filename("report.txt"), "report.txt");
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(
            sanitize_filename("C:\\Users\\me\\secrets.bin"),
            "secrets.bin"
        );
        assert_eq!(sanitize_filename("a\u{0}b\nc.txt"), "abc.txt");
        assert_eq!(sanitize_filename(""), "");
        assert_eq!(sanitize_filename(".."), "");

        // A whole text payload as the name: truncated, still one component with
        // its extension kept.
        let long = format!("{}.txt", "登".repeat(500));
        let clean = sanitize_filename(&long);
        assert!(clean.len() <= MAX_FILENAME_BYTES, "{}", clean.len());
        assert!(clean.ends_with(".txt"));
        assert!(std::str::from_utf8(clean.as_bytes()).is_ok());
        assert!(!clean.contains('/'));
    }

    /// A text transfer saves as `raptorqr-text.txt`; a 1 KB body used to reach
    /// `fs::write` as the filename and panic with ENAMETOOLONG, which killed the
    /// receive thread and silently stopped all later transfers.
    #[test]
    fn long_body_is_never_used_as_a_filename() {
        let dir = temp_dir("long-name");
        let body = "登录口令: hunter2\n".repeat(64);
        let (path, written) = save_in(&dir, "", body.as_bytes());

        assert!(written);
        assert_eq!(path.file_name().unwrap(), "raptorqr-file.bin");
        assert_eq!(fs::read(&path).unwrap(), body.as_bytes());

        // Even a name that cannot be used must not panic, and must not land
        // outside the output directory.
        let (escaped, _) = save_in(&dir, "../../../etc/hosts-escape", b"payload");
        assert_eq!(escaped.parent().unwrap(), dir.as_path());

        fs::remove_dir_all(&dir).unwrap();
    }

    /// Text is delivered as text — no file, payload available for the clipboard.
    /// Files still go to disk.
    #[test]
    fn text_is_delivered_without_touching_the_disk() {
        let dir = temp_dir("delivery");
        let text = CompletedTransfer {
            filename: TEXT_LABEL.to_string(),
            body: "SELECT * FROM t".as_bytes().to_vec(),
            is_text: true,
        };
        assert_eq!(text.delivery(), Delivery::Clipboard);
        assert_eq!(text.as_text().as_deref(), Some("SELECT * FROM t"));
        assert!(
            save_completed(&text, Some(&dir)).is_none(),
            "text must never be saved"
        );
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "no file may appear for a text transfer"
        );

        let file = CompletedTransfer {
            filename: "report.bin".to_string(),
            body: b"binary".to_vec(),
            is_text: false,
        };
        assert_eq!(file.delivery(), Delivery::File);
        assert_eq!(file.as_text(), None);
        let (path, written) = save_completed(&file, Some(&dir)).expect("files are saved");
        assert!(written);
        assert_eq!(path.file_name().unwrap(), "report.bin");
        assert_eq!(fs::read(&path).unwrap(), b"binary");

        fs::remove_dir_all(&dir).unwrap();
    }
}
