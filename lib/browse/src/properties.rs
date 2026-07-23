//! The properties view model (`plans/NEW-FILEMANAGER.md` FM8): the structured,
//! display-ready summary of one selected node's metadata a file manager's
//! Properties panel shows.
//!
//! This is the pure model, host-provable ahead of the drawn panel exactly as
//! [`activate`](crate::activate) / [`execute`](crate::execute) landed their
//! decisions ahead of the app wiring. The `files.app` `Run` binary performs
//! the one capability-checked `fs_stat` under the user's own identity and
//! hands the resulting [`FileStat`] here; the model owns *no* filesystem
//! authority and reads nothing — it only turns the already-authorised metadata
//! into the display fields the panel renders, so composing it grants nothing
//! and the read-only picker builds the same view.
//!
//! Every field comes straight from `fs_stat`; none is fabricated. A timestamp
//! the backing does not keep is [`Time64::UNIX_EPOCH`](tairix_abi::time::Time64::UNIX_EPOCH),
//! which the shared
//! [`format_datetime`] renders blank rather than as a made-up wall time. The
//! permission spelling is the one shared [`tairix_abi::fs::mode_string`]
//! definition, so the panel and `ls -l` never disagree on what a mode means,
//! and the sizes use the same [`format_size`] the item view's column uses.

use alloc::string::String;

use tairix_abi::fs::{mode_string, FileKind, FileStat, FS_MODE_MASK};
use tairix_abi::NodeTimes;

use crate::entry::EntryKind;
use crate::format::{format_datetime, format_size};

/// The display-ready summary of one node's metadata for the Properties panel.
///
/// Built from the entry's name and browser [`EntryKind`] (which distinguishes
/// a sealed application `Bundle` from an ordinary directory) plus the node's
/// [`FileStat`]. The human *kind label* reads from the [`EntryKind`], while the
/// permission string's type indicator reads from the structural
/// [`FileStat::kind`] — a bundle is labelled "Application" yet is honestly a
/// directory on disk (`d…`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Properties {
    name: String,
    kind: EntryKind,
    file_kind: FileKind,
    size: u64,
    allocated: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    times: NodeTimes,
}

impl Properties {
    /// Build the summary from an entry's `name`, its browser `kind`, and the
    /// `stat` the app read for it.
    #[must_use]
    pub fn from_stat(name: impl Into<String>, kind: EntryKind, stat: &FileStat) -> Self {
        Self {
            name: name.into(),
            kind,
            file_kind: stat.kind,
            size: stat.size,
            allocated: stat.allocated,
            mode: stat.mode,
            uid: stat.uid,
            gid: stat.gid,
            times: stat.times,
        }
    }

    /// The node's name (a single path component).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The node's browser kind.
    #[must_use]
    pub const fn kind(&self) -> EntryKind {
        self.kind
    }

    /// A short human label for the node's kind: `Folder`, `File`, or
    /// `Application` (a `<Name>.app` bundle).
    #[must_use]
    pub const fn kind_label(&self) -> &'static str {
        match self.kind {
            EntryKind::Directory => "Folder",
            EntryKind::File => "File",
            EntryKind::Bundle => "Application",
        }
    }

    /// The apparent size in bytes.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// The apparent size rendered in binary units (`1.5 MiB`).
    #[must_use]
    pub fn size_display(&self) -> String {
        format_size(self.size)
    }

    /// Bytes of on-disk storage the node's data occupies, as the mounted
    /// format reports it (never derived from [`size`](Self::size)).
    #[must_use]
    pub const fn allocated(&self) -> u64 {
        self.allocated
    }

    /// The on-disk allocation rendered in binary units.
    #[must_use]
    pub fn allocated_display(&self) -> String {
        format_size(self.allocated)
    }

    /// The raw POSIX mode bits (the low 12 bits are meaningful).
    #[must_use]
    pub const fn mode(&self) -> u32 {
        self.mode
    }

    /// The mode's meaningful bits as a four-digit octal string (`0755`).
    #[must_use]
    pub fn mode_octal(&self) -> String {
        alloc::format!("{:04o}", self.mode & FS_MODE_MASK)
    }

    /// The ten-character permission string (`drwxr-xr-x`) — the shared
    /// [`tairix_abi::fs::mode_string`] spelling, with the type indicator taken
    /// from the structural [`FileStat::kind`].
    #[must_use]
    pub fn permissions(&self) -> String {
        mode_string(self.file_kind, self.mode)
            .iter()
            .map(|&b| b as char)
            .collect()
    }

    /// The owning user id.
    #[must_use]
    pub const fn uid(&self) -> u32 {
        self.uid
    }

    /// The owning group id.
    #[must_use]
    pub const fn gid(&self) -> u32 {
        self.gid
    }

    /// The node's four timestamps.
    #[must_use]
    pub const fn times(&self) -> NodeTimes {
        self.times
    }

    /// The creation instant rendered as `YYYY-MM-DD HH:MM:SS`, blank when the
    /// backing keeps no such stamp.
    #[must_use]
    pub fn created_display(&self) -> String {
        format_datetime(self.times.created)
    }

    /// The last contents-modification instant rendered as
    /// `YYYY-MM-DD HH:MM:SS`, blank when the backing keeps no such stamp.
    #[must_use]
    pub fn modified_display(&self) -> String {
        format_datetime(self.times.modified)
    }

    /// The last access instant rendered as `YYYY-MM-DD HH:MM:SS`, blank when
    /// the backing keeps no such stamp (ARXFS keeps none, so this is blank
    /// there).
    #[must_use]
    pub fn accessed_display(&self) -> String {
        format_datetime(self.times.accessed)
    }

    /// The last metadata-change instant rendered as `YYYY-MM-DD HH:MM:SS`,
    /// blank when the backing keeps no such stamp.
    #[must_use]
    pub fn changed_display(&self) -> String {
        format_datetime(self.times.changed)
    }
}

#[cfg(test)]
mod tests {
    use super::Properties;
    use crate::entry::EntryKind;
    use tairix_abi::fs::{FileId, FileKind, FileStat};
    use tairix_abi::time::Time64;
    use tairix_abi::NodeTimes;

    fn stat(kind: FileKind, mode: u32) -> FileStat {
        FileStat {
            kind,
            size: 1536,
            allocated: 4096,
            mode,
            uid: 1000,
            gid: 100,
            id: FileId::NONE,
            times: NodeTimes {
                created: Time64::from_secs(1_609_459_200),
                modified: Time64::from_secs(1_609_459_200 + 3661),
                accessed: Time64::UNIX_EPOCH,
                changed: Time64::from_secs(1_700_000_000),
            },
        }
    }

    #[test]
    fn a_regular_file_summarises_its_stat() {
        let s = stat(FileKind::Regular, 0o644);
        let p = Properties::from_stat("notes.txt", EntryKind::File, &s);
        assert_eq!(p.name(), "notes.txt");
        assert_eq!(p.kind(), EntryKind::File);
        assert_eq!(p.kind_label(), "File");
        assert_eq!(p.size(), 1536);
        assert_eq!(p.size_display(), "1.5 KiB");
        assert_eq!(p.allocated(), 4096);
        assert_eq!(p.allocated_display(), "4.0 KiB");
        assert_eq!(p.mode(), 0o644);
        assert_eq!(p.mode_octal(), "0644");
        assert_eq!(p.permissions(), "-rw-r--r--");
        assert_eq!(p.uid(), 1000);
        assert_eq!(p.gid(), 100);
    }

    #[test]
    fn a_directory_reads_as_a_folder_with_a_directory_permission_char() {
        let s = stat(FileKind::Directory, 0o755);
        let p = Properties::from_stat("Documents", EntryKind::Directory, &s);
        assert_eq!(p.kind_label(), "Folder");
        assert_eq!(p.permissions(), "drwxr-xr-x");
    }

    #[test]
    fn a_bundle_is_labelled_application_yet_is_a_directory_on_disk() {
        // A `<Name>.app` bundle: the human label is "Application", but the
        // structural stat kind is a directory, so the permission string still
        // honestly leads with `d`.
        let s = stat(FileKind::Directory, 0o755);
        let p = Properties::from_stat("Editor.app", EntryKind::Bundle, &s);
        assert_eq!(p.kind_label(), "Application");
        assert_eq!(p.permissions(), "drwxr-xr-x");
    }

    #[test]
    fn timestamps_render_and_an_unkept_stamp_is_blank() {
        let s = stat(FileKind::Regular, 0o600);
        let p = Properties::from_stat("f", EntryKind::File, &s);
        assert_eq!(p.created_display(), "2021-01-01 00:00:00");
        assert_eq!(p.modified_display(), "2021-01-01 01:01:01");
        // The backing kept no access time (epoch): blank, never fabricated.
        assert_eq!(p.accessed_display(), "");
        assert_eq!(p.changed_display(), "2023-11-14 22:13:20");
        assert_eq!(p.times().accessed, Time64::UNIX_EPOCH);
    }

    #[test]
    fn the_octal_mode_masks_off_bits_beyond_the_meaningful_twelve() {
        // A stray high bit outside the low 12 mode bits does not appear.
        let mut s = stat(FileKind::Regular, 0o644);
        s.mode = 0xFFFF_F000 | 0o755;
        let p = Properties::from_stat("f", EntryKind::File, &s);
        assert_eq!(p.mode_octal(), "0755");
    }
}
