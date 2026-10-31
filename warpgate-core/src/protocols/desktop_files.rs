//! Protocol-agnostic clipboard file transfer.
//!
//! Files are never sent eagerly. An offer only lists names and sizes; the receiving side
//! pulls byte ranges of the files it actually wants, and each range travels as one
//! request/reply pair through Warpgate, which checks it against the offer it relayed. This
//! is the shape of MS-RDPECLIP's delayed rendering (`FileGroupDescriptorW` +
//! `FileContentsRequest`), so an RDP peer on either side maps onto it one PDU per message,
//! and the browser desktop speaks the same pull model over its WebSocket.
//!
//! Each side numbers the offers and requests it originates. An offer id is only ever
//! resolved by the side that minted it, as is a request id, so the two directions never
//! share an id space.

use bytes::Bytes;

/// The largest byte range one request may ask for. A peer asking for more is served a
/// short read, which every file contents consumer already handles as end-of-chunk.
pub const MAX_FILE_CHUNK_BYTES: u32 = 4 * 1024 * 1024;

/// The most entries a single offer may list. Windows Explorer flattens a copied folder
/// into one entry per file and directory, so this bounds both memory and the descriptor
/// PDU a peer has to parse.
pub const MAX_OFFERED_FILES: usize = 1024;

/// Longest relative path accepted in an offer, in UTF-16 code units. MS-RDPECLIP's
/// `cFileName` holds 260 including the terminator.
pub const MAX_FILE_PATH_UNITS: usize = 259;

/// One entry of a file offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardFile {
    /// Relative path inside the offer, `/`-separated, already sanitised by
    /// [`sanitize_file_path`].
    pub path: String,
    /// Declared size in bytes; `0` for a directory.
    pub size: u64,
    pub directory: bool,
}

impl ClipboardFile {
    /// The last path component.
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// Files a peer copied. Replaces any previous offer from the same side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardFileOffer {
    pub offer: u32,
    pub files: Vec<ClipboardFile>,
}

/// A byte range of one offered file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileContentsRequest {
    /// Minted by the requesting side, echoed in the [`FileContentsReply`].
    pub id: u32,
    pub offer: u32,
    pub index: u32,
    pub offset: u64,
    pub length: u32,
}

/// The answer to a [`FileContentsRequest`]. `None` means the range could not be served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileContentsReply {
    pub id: u32,
    pub data: Option<Bytes>,
}

/// One step of a clipboard file transfer, in either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardTransfer {
    Offer(ClipboardFileOffer),
    Request(FileContentsRequest),
    Reply(FileContentsReply),
}

/// Which way a file moved, from the target's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileTransferDirection {
    /// From the viewer to the target.
    Upload,
    /// From the target to the viewer.
    Download,
}

/// An audit record of a file being read across the session. Never carries file contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTransferRecord {
    pub direction: FileTransferDirection,
    pub path: String,
    pub size: u64,
}

/// What the target connection allows, announced once it is up so a viewer only offers
/// the features that will actually work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DesktopCapabilities {
    /// The per-copy size cap when clipboard file transfer is enabled.
    pub clipboard_files_max_bytes: Option<u64>,
}

/// Normalise a peer-supplied relative path to `/`-separated components, or `None` if it
/// could escape the offer or would not survive as a file name on the receiving system:
/// empty, `.` or `..` components, drive or stream colons, control characters, Windows
/// reserved characters and device names, trailing dots or spaces.
pub fn sanitize_file_path(path: &str) -> Option<String> {
    let mut components = Vec::new();
    for component in path.split(['/', '\\']) {
        if component.is_empty() {
            // A leading or doubled separator: absolute or UNC paths are not relative.
            return None;
        }
        if component == "." || component == ".." {
            return None;
        }
        if component
            .chars()
            .any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        {
            return None;
        }
        if component.ends_with('.') || component.ends_with(' ') {
            return None;
        }
        let stem = component.split('.').next().unwrap_or(component);
        if is_windows_device_name(stem) {
            return None;
        }
        components.push(component);
    }
    let joined = components.join("/");
    if joined.encode_utf16().count() > MAX_FILE_PATH_UNITS {
        return None;
    }
    Some(joined)
}

fn is_windows_device_name(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    match upper.as_bytes() {
        [b'C', b'O', b'M', digit] | [b'L', b'P', b'T', digit] => {
            digit.is_ascii_digit() && *digit != b'0'
        }
        _ => false,
    }
}

/// Apply Warpgate's limits to an offer a peer made: entries with unusable paths are
/// dropped, and files are kept in order only while the running total stays within
/// `max_bytes` and the entry count within [`MAX_OFFERED_FILES`]. Directories cost nothing
/// against the byte cap. Returns the kept entries and how many were dropped.
pub fn limit_file_offer(files: Vec<ClipboardFile>, max_bytes: u64) -> (Vec<ClipboardFile>, usize) {
    let total = files.len();
    let mut used = 0u64;
    let mut kept = Vec::new();
    for mut file in files {
        if kept.len() >= MAX_OFFERED_FILES {
            break;
        }
        let Some(path) = sanitize_file_path(&file.path) else {
            continue;
        };
        file.path = path;
        if file.directory {
            file.size = 0;
        } else {
            match used.checked_add(file.size) {
                Some(next) if next <= max_bytes => used = next,
                _ => continue,
            }
        }
        kept.push(file);
    }
    let dropped = total - kept.len();
    (kept, dropped)
}

/// Check a range request against the entry it names: in bounds, a file rather than a
/// directory, and no longer than [`MAX_FILE_CHUNK_BYTES`] or the bytes left in the file.
/// Returns the length to actually request, which is `0` only at end of file.
pub fn clamp_file_range(file: &ClipboardFile, offset: u64, length: u32) -> Option<u32> {
    if file.directory || offset > file.size {
        return None;
    }
    let remaining = file.size - offset;
    let length = u64::from(length.min(MAX_FILE_CHUNK_BYTES)).min(remaining);
    u32::try_from(length).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64) -> ClipboardFile {
        ClipboardFile {
            path: path.into(),
            size,
            directory: false,
        }
    }

    #[test]
    fn relative_paths_are_normalised_to_slashes() {
        assert_eq!(
            sanitize_file_path(r"dir\sub/file.txt").as_deref(),
            Some("dir/sub/file.txt")
        );
        assert_eq!(
            sanitize_file_path("report.pdf").as_deref(),
            Some("report.pdf")
        );
    }

    #[test]
    fn paths_that_could_escape_or_misbehave_are_refused() {
        for bad in [
            "",
            "/etc/passwd",
            r"\\server\share\x",
            "a//b",
            "../x",
            r"dir\..\..\x",
            "./x",
            "C:x",
            "file.txt:stream",
            "con",
            "LPT1.txt",
            "dir/aux.log",
            "trailing.",
            "trailing ",
            "new\nline",
            "wild*card",
        ] {
            assert_eq!(sanitize_file_path(bad), None, "{bad:?}");
        }
        // Lookalikes of device names are ordinary names.
        assert!(sanitize_file_path("console.txt").is_some());
        assert!(sanitize_file_path("COM0").is_some());
        assert!(sanitize_file_path("COM10").is_some());
    }

    #[test]
    fn overlong_paths_are_refused() {
        assert!(sanitize_file_path(&"a".repeat(MAX_FILE_PATH_UNITS)).is_some());
        assert!(sanitize_file_path(&"a".repeat(MAX_FILE_PATH_UNITS + 1)).is_none());
    }

    #[test]
    fn an_offer_is_cut_at_the_byte_cap_in_order() {
        let (kept, dropped) = limit_file_offer(
            vec![
                file("a", 60),
                ClipboardFile {
                    path: "dir".into(),
                    size: 999,
                    directory: true,
                },
                file("b", 50),
                file("c", 40),
                file("../evil", 1),
            ],
            100,
        );
        let names: Vec<_> = kept.iter().map(|f| f.path.as_str()).collect();
        // `b` would overrun the cap; `c` still fits after it is skipped.
        assert_eq!(names, ["a", "dir", "c"]);
        assert_eq!(kept[1].size, 0);
        assert_eq!(dropped, 2);
    }

    #[test]
    fn a_declared_size_cannot_overflow_the_running_total() {
        let (kept, _) = limit_file_offer(vec![file("a", 10), file("b", u64::MAX)], u64::MAX);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn an_offer_is_cut_at_the_entry_cap() {
        let files = (0..MAX_OFFERED_FILES + 5)
            .map(|i| file(&format!("f{i}"), 0))
            .collect();
        let (kept, dropped) = limit_file_offer(files, u64::MAX);
        assert_eq!(kept.len(), MAX_OFFERED_FILES);
        assert_eq!(dropped, 5);
    }

    #[test]
    fn ranges_stay_inside_the_declared_file() {
        let f = file("a", 10);
        assert_eq!(clamp_file_range(&f, 0, 4), Some(4));
        assert_eq!(clamp_file_range(&f, 8, 4), Some(2));
        assert_eq!(clamp_file_range(&f, 10, 4), Some(0));
        assert_eq!(clamp_file_range(&f, 11, 4), None);
        let big = file("b", u64::MAX);
        assert_eq!(
            clamp_file_range(&big, 0, u32::MAX),
            Some(MAX_FILE_CHUNK_BYTES)
        );
        let dir = ClipboardFile {
            path: "d".into(),
            size: 0,
            directory: true,
        };
        assert_eq!(clamp_file_range(&dir, 0, 1), None);
    }
}
