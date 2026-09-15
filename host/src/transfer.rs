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

/// A transfer that finished, as shown by the UI.
#[derive(Debug, Clone)]
pub struct TransferStatus {
    pub key: SessionKey,
    pub progress: f64,
    pub blocks: Vec<(u8, u32, u32)>,
    /// Set once the transfer is complete: (filename, saved path, bytes).
    pub complete: Option<(String, PathBuf, usize)>,
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
                    "raptorqr-text.txt".to_string()
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
            complete: Some((done.filename.clone(), done.path.clone(), done.bytes)),
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

/// [`save_to_downloads`] against an explicit directory (testable).
/// Returns the path and whether a new file was actually written.
fn save_in(dir: &std::path::Path, filename: &str, body: &[u8]) -> (PathBuf, bool) {
    let dir = dir.to_path_buf();
    let existing = dir.join(filename);
    if fs::read(&existing).is_ok_and(|bytes| bytes == body) {
        return (existing, false);
    }
    let path = unique_path(&dir, filename);
    fs::write(&path, body).expect("failed to write received file");
    (path, true)
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
}
