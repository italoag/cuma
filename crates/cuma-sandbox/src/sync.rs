//! Bringing an agent's work back from a sandbox that holds a copy of the
//! workspace.
//!
//! Before the agent starts, [`Snapshot::take`] records what every file in the
//! workspace contained and archives it for upload. Afterwards,
//! [`Snapshot::merge`] compares the sandbox's final copy with that record,
//! file by file:
//!
//! | In the sandbox | Here | Result |
//! |---|---|---|
//! | unchanged | anything | nothing to do |
//! | changed, created or deleted | as recorded | applied |
//! | changed, created or deleted | the same change | nothing to do |
//! | changed, created or deleted | changed otherwise | conflict |
//!
//! A single conflict applies nothing: the sandbox's copy is kept for a manual
//! merge and the task fails, rather than half of the agent's change landing.
//!
//! In a git work tree only what git tracks or would track is uploaded, so
//! dependencies and build output stay behind, and a new file the sandbox
//! created that git ignores is not brought back. `.git` travels, so the agent
//! can use git, but is never merged back: commits made in a sandbox stay
//! there, as nothing is committed on the user's behalf. CUMA's own `.cuma`
//! neither goes nor comes back.

use cuma_core::error::{MetaAgentError, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Component, Path, PathBuf};

/// What one file held.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// A regular file.
    File {
        /// SHA-256 of its contents.
        digest: [u8; 32],
        /// Whether any execute bit was set.
        executable: bool,
    },
    /// A symbolic link, and where it points.
    Link(PathBuf),
}

/// The workspace as it was handed to a sandbox.
#[derive(Debug)]
pub struct Snapshot {
    workspace: PathBuf,
    files: BTreeMap<PathBuf, Entry>,
    archive: Option<PathBuf>,
    git: bool,
}

/// What a merge changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeReport {
    /// Files created or changed here.
    pub written: usize,
    /// Files deleted here.
    pub deleted: usize,
}

impl std::fmt::Display for MergeReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} written, {} deleted", self.written, self.deleted)
    }
}

/// Unpack `archive` into `dir`, which must not exist yet. CUMA reads the
/// archive itself rather than through `tar`, so the names it judges are
/// exactly the names it writes, a newline included. Refused: an absolute
/// name or a `..`; a member below a file or link the archive made; a hard
/// link to anything but a file unpacked before it; anything that is not a
/// file, a directory or a link. Links are unpacked as they are: the merge
/// judges where they point.
fn unpack(archive: &Path, dir: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    // Made here, so nothing in it can predate the archive.
    std::fs::create_dir(dir).map_err(io("creating the unpack directory"))?;
    let file = std::fs::File::open(archive).map_err(io("reading the sandbox's copy"))?;
    let mut reader = std::io::BufReader::new(file);
    // Files and links: nothing may be unpacked below one.
    let mut leaves: BTreeSet<PathBuf> = BTreeSet::new();
    let mut files: BTreeSet<PathBuf> = BTreeSet::new();
    let (mut long_name, mut long_link) = (None::<Vec<u8>>, None::<Vec<u8>>);
    let mut pax: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut header = [0u8; 512];
    while read_block(&mut reader, &mut header)? {
        if header.iter().all(|b| *b == 0) {
            break;
        }
        if !checksum_ok(&header) {
            return Err(unreadable("a header's checksum is wrong"));
        }
        let kind = header[156];
        let own_size = header_number(&header[124..136])?;
        match kind {
            // GNU long names, and pax records, describe the next member.
            b'L' => long_name = Some(until_nul(read_data(&mut reader, own_size)?)),
            b'K' => long_link = Some(until_nul(read_data(&mut reader, own_size)?)),
            b'x' => pax = pax_records(&read_data(&mut reader, own_size)?),
            b'g' => skip(&mut reader, own_size + padding(own_size))?,
            _ => {}
        }
        if matches!(kind, b'L' | b'K' | b'x' | b'g') {
            continue;
        }
        let size = match pax.get("size") {
            Some(size) => decimal(size)?,
            None => own_size,
        };
        let raw = pax
            .remove("path")
            .or_else(|| long_name.take())
            .unwrap_or_else(|| ustar_name(&header));
        let link = pax
            .remove("linkpath")
            .or_else(|| long_link.take())
            .unwrap_or_else(|| field(&header[157..257]).to_vec());
        pax.clear();
        (long_name, long_link) = (None, None);
        let Some(name) = member_name(&raw)? else {
            skip(&mut reader, size + padding(size))?;
            continue;
        };
        if name
            .ancestors()
            .skip(1)
            .any(|parent| leaves.contains(parent))
        {
            return Err(refused(
                &name,
                "lies below a file or link in the same archive",
            ));
        }
        let target = dir.join(&name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(io("unpacking a directory"))?;
        }
        // `create_new` and `symlink` fail on anything already there, and
        // never follow a link at the name itself.
        let new_file = |mode: u32| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&target)
                .map_err(io("unpacking a file"))
        };
        match kind {
            b'5' => {
                std::fs::create_dir_all(&target).map_err(io("unpacking a directory"))?;
                skip(&mut reader, size + padding(size))?;
            }
            b'0' | 0 | b'7' => {
                let executable = header_number(&header[100..108]).is_ok_and(|m| m & 0o111 != 0);
                let mut out = new_file(if executable { 0o755 } else { 0o644 })?;
                let copied = std::io::copy(&mut (&mut reader).take(size), &mut out)
                    .map_err(io("unpacking a file"))?;
                if copied != size {
                    return Err(unreadable("it ends inside a file"));
                }
                skip(&mut reader, padding(size))?;
                files.insert(name.clone());
                leaves.insert(name);
            }
            b'2' => {
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&link), &target)
                    .map_err(io("unpacking a link"))?;
                skip(&mut reader, size + padding(size))?;
                leaves.insert(name);
            }
            b'1' => {
                let original = member_name(&link)?
                    .filter(|original| files.contains(original))
                    .ok_or_else(|| {
                        refused(&name, "is a hard link to no file unpacked before it")
                    })?;
                let mut from =
                    std::fs::File::open(dir.join(&original)).map_err(io("unpacking a file"))?;
                let mode = from
                    .metadata()
                    .map_err(io("unpacking a file"))?
                    .permissions()
                    .mode();
                std::io::copy(&mut from, &mut new_file(mode & 0o777)?)
                    .map_err(io("unpacking a file"))?;
                skip(&mut reader, size + padding(size))?;
                files.insert(name.clone());
                leaves.insert(name);
            }
            _ => return Err(refused(&name, "is not a file, a directory or a link")),
        }
    }
    Ok(())
}

fn refused(name: &Path, why: &str) -> MetaAgentError {
    MetaAgentError::Other(format!("the sandbox's copy was refused: {name:?} {why}"))
}

fn unreadable(why: &str) -> MetaAgentError {
    MetaAgentError::Other(format!(
        "the sandbox's copy is not a readable tar archive: {why}"
    ))
}

/// One 512-byte block; `false` at the end of the file.
fn read_block(reader: &mut impl Read, block: &mut [u8; 512]) -> Result<bool> {
    let mut filled = 0;
    while filled < block.len() {
        match reader.read(&mut block[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err(unreadable("it ends inside a header")),
            Ok(n) => filled += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(io("reading the sandbox's copy")(err)),
        }
    }
    Ok(true)
}

/// What follows `size` bytes of data to the next block.
fn padding(size: u64) -> u64 {
    (512 - size % 512) % 512
}

fn skip(reader: &mut impl Read, size: u64) -> Result<()> {
    let skipped = std::io::copy(&mut reader.take(size), &mut std::io::sink())
        .map_err(io("reading the sandbox's copy"))?;
    if skipped == size {
        Ok(())
    } else {
        Err(unreadable("it ends inside a member"))
    }
}

/// An extended header's data, bounded: names and pax records are small.
fn read_data(reader: &mut impl Read, size: u64) -> Result<Vec<u8>> {
    if size > 1 << 20 {
        return Err(unreadable("an extended header is over 1 MiB"));
    }
    let mut data = Vec::new();
    reader
        .take(size)
        .read_to_end(&mut data)
        .map_err(io("reading the sandbox's copy"))?;
    if u64::try_from(data.len()).ok() != Some(size) {
        return Err(unreadable("it ends inside an extended header"));
    }
    skip(reader, padding(size))?;
    Ok(data)
}

/// A NUL-terminated header field.
fn field(bytes: &[u8]) -> &[u8] {
    bytes.split(|b| *b == 0).next().unwrap_or_default()
}

fn until_nul(mut data: Vec<u8>) -> Vec<u8> {
    if let Some(end) = data.iter().position(|b| *b == 0) {
        data.truncate(end);
    }
    data
}

/// A numeric header field: octal digits, or GNU's base-256 for large values.
fn header_number(bytes: &[u8]) -> Result<u64> {
    if bytes.first().is_some_and(|b| b & 0x80 != 0) {
        return bytes[1..].iter().try_fold(0u64, |n, b| {
            n.checked_mul(256)
                .map(|n| n | u64::from(*b))
                .ok_or_else(|| unreadable("a size is too large"))
        });
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| unreadable("a header field is not a number"))?
        .trim_matches(|c: char| c == '\0' || c == ' ');
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, 8).map_err(|_| unreadable("a header field is not a number"))
}

fn decimal(bytes: &[u8]) -> Result<u64> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .ok_or_else(|| unreadable("a pax size is not a number"))
}

/// A header's checksum: its bytes summed with the checksum field as spaces,
/// unsigned as POSIX says or signed as some old writers did.
fn checksum_ok(header: &[u8; 512]) -> bool {
    let Ok(expected) = header_number(&header[148..156]) else {
        return false;
    };
    let byte = |i: usize, b: u8| if (148..156).contains(&i) { b' ' } else { b };
    let unsigned: u64 = header
        .iter()
        .enumerate()
        .map(|(i, b)| u64::from(byte(i, *b)))
        .sum();
    let signed: i64 = header
        .iter()
        .enumerate()
        .map(|(i, b)| i64::from(i8::from_ne_bytes([byte(i, *b)])))
        .sum();
    expected == unsigned || i64::try_from(expected).is_ok_and(|e| e == signed)
}

/// The name in a header: `prefix/name` when a POSIX ustar header sets a
/// prefix (GNU headers use those bytes for something else).
fn ustar_name(header: &[u8; 512]) -> Vec<u8> {
    let name = field(&header[0..100]);
    let prefix = field(&header[345..500]);
    if &header[257..263] == b"ustar\0" && !prefix.is_empty() {
        [prefix, b"/", name].concat()
    } else {
        name.to_vec()
    }
}

/// Pax extended header records: `<length> <key>=<value>\n`, the length
/// counting the whole record.
fn pax_records(data: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut records = BTreeMap::new();
    let mut rest = data;
    while let Some(space) = rest.iter().position(|b| *b == b' ') {
        let Some(length) = std::str::from_utf8(&rest[..space])
            .ok()
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|length| *length > space + 1 && *length <= rest.len())
        else {
            break;
        };
        let record = &rest[space + 1..length];
        let record = record.strip_suffix(b"\n").unwrap_or(record);
        if let Some(equals) = record.iter().position(|b| *b == b'=') {
            records.insert(
                String::from_utf8_lossy(&record[..equals]).into_owned(),
                record[equals + 1..].to_vec(),
            );
        }
        rest = &rest[length..];
    }
    records
}

/// A member's name as a path below the unpack directory, or `None` for the
/// directory itself. An absolute name or a `..` is refused.
fn member_name(raw: &[u8]) -> Result<Option<PathBuf>> {
    use std::os::unix::ffi::OsStrExt as _;
    let path = Path::new(std::ffi::OsStr::from_bytes(raw));
    let mut name = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => name.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(refused(path, "would be written outside the workspace"));
            }
        }
    }
    Ok((!name.as_os_str().is_empty()).then_some(name))
}

/// Whether a link at `link` (relative to the workspace) pointing to `target`
/// leads out of it: an absolute target, or more `..` than the link is deep.
fn link_escapes(link: &Path, target: &Path) -> bool {
    let mut depth = link
        .parent()
        .map_or(0, |parent| parent.components().count());
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => match depth.checked_sub(1) {
                Some(up) => depth = up,
                None => return true,
            },
            Component::RootDir | Component::Prefix(_) => return true,
        }
    }
    false
}

/// A hidden file beside `target` that only this merge made: `make` is
/// retried under fresh random names while the name it is given exists.
fn scratch(
    target: &Path,
    suffix: &str,
    make: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<PathBuf> {
    let (parent, prefix) = scratch_place(target);
    tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(suffix)
        .make_in(parent, make)
        .map_err(io("making a file beside a target"))?
        .keep()
        .map(|(_, path)| path)
        .map_err(|err| MetaAgentError::Other(format!("keeping a file beside a target: {err}")))
}

/// The directory and name prefix of `target`'s scratch files.
fn scratch_place(target: &Path) -> (&Path, String) {
    let name: String = target
        .file_name()
        .map(|name| name.to_string_lossy().chars().take(64).collect())
        .unwrap_or_default();
    (
        target.parent().unwrap_or(Path::new(".")),
        format!(".{name}.cuma-"),
    )
}

/// Keep what is at `target` under a scratch name, without moving it: a hard
/// link to a file, a copy of a link. `None` when nothing is there.
fn set_aside_copy(target: &Path) -> Result<Option<PathBuf>> {
    match target.symlink_metadata() {
        Ok(meta) if meta.file_type().is_symlink() => {
            let points_to = std::fs::read_link(target).map_err(io("reading a link"))?;
            scratch(target, ".old", |path| {
                std::os::unix::fs::symlink(&points_to, path)
            })
            .map(Some)
        }
        Ok(_) => scratch(target, ".old", |path| std::fs::hard_link(target, path)).map(Some),
        Err(_) => Ok(None),
    }
}

/// Run blocking file work — hashing, archiving, merging — off the async
/// runtime's workers.
async fn off_runtime<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| MetaAgentError::Other(format!("workspace transfer interrupted: {e}")))?
}

impl Snapshot {
    /// [`take`](Self::take), without blocking the async runtime.
    pub async fn take_async(workspace: &Path, dir: &Path) -> Result<std::sync::Arc<Self>> {
        let (workspace, dir) = (workspace.to_path_buf(), dir.to_path_buf());
        off_runtime(move || Self::take(&workspace, &dir).map(std::sync::Arc::new)).await
    }

    /// [`record`](Self::record), without blocking the async runtime.
    pub async fn record_async(workspace: &Path) -> Result<std::sync::Arc<Self>> {
        let workspace = workspace.to_path_buf();
        off_runtime(move || Self::record(&workspace).map(std::sync::Arc::new)).await
    }

    /// [`merge_archive`](Self::merge_archive), without blocking the async
    /// runtime.
    pub async fn merge_archive_async(
        self: std::sync::Arc<Self>,
        archive: PathBuf,
        dir: PathBuf,
        keep: PathBuf,
    ) -> Result<MergeReport> {
        off_runtime(move || self.merge_archive(&archive, &dir, &keep)).await
    }

    /// [`merge`](Self::merge), without blocking the async runtime.
    pub async fn merge_async(
        self: std::sync::Arc<Self>,
        result: PathBuf,
        keep: PathBuf,
    ) -> Result<MergeReport> {
        off_runtime(move || self.merge(&result, &keep)).await
    }

    /// Record the workspace and archive it as `dir/workspace.tar`.
    pub fn take(workspace: &Path, dir: &Path) -> Result<Self> {
        let mut snapshot = Self::record(workspace)?;
        let list = dir.join("files.list");
        let mut names: Vec<u8> = Vec::new();
        for path in snapshot.files.keys() {
            names.extend_from_slice(path.as_os_str().as_encoded_bytes());
            names.push(0);
        }
        if snapshot.workspace.join(".git").exists() {
            names.extend_from_slice(b".git\0");
        }
        std::fs::write(&list, names).map_err(io("writing the archive list"))?;

        let archive = dir.join("workspace.tar");
        let status = std::process::Command::new("tar")
            .arg("-c")
            .arg("-f")
            .arg(&archive)
            .arg("-C")
            .arg(&snapshot.workspace)
            .arg("--null")
            .arg("-T")
            .arg(&list)
            .env("COPYFILE_DISABLE", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(io("running tar"))?;
        if !status.status.success() {
            return Err(MetaAgentError::Other(format!(
                "archiving {} failed: {}",
                snapshot.workspace.display(),
                String::from_utf8_lossy(&status.stderr).trim()
            )));
        }
        snapshot.archive = Some(archive);
        Ok(snapshot)
    }

    /// Record the workspace without archiving it, for a sandbox that copies
    /// it by itself.
    pub fn record(workspace: &Path) -> Result<Self> {
        let workspace = std::fs::canonicalize(workspace).map_err(io("reading the workspace"))?;
        let git = is_git_work_tree(&workspace);
        let candidates = if git {
            git_listed(&workspace)?
        } else {
            let mut found = Vec::new();
            walk(&workspace, Path::new(""), &mut found)?;
            found
        };
        let mut files = BTreeMap::new();
        for relative in candidates {
            if excluded(&relative) {
                continue;
            }
            let full = workspace.join(&relative);
            if full.is_dir() && !full.is_symlink() {
                // A submodule, listed by git as one path.
                let mut inner = Vec::new();
                walk(&full, &relative, &mut inner)?;
                for path in inner {
                    if let Some(entry) = entry_of(&workspace.join(&path))? {
                        files.insert(path, entry);
                    }
                }
            } else if let Some(entry) = entry_of(&full)? {
                files.insert(relative, entry);
            }
        }
        Ok(Self {
            workspace,
            files,
            archive: None,
            git,
        })
    }

    /// The archive to upload, when [`take`](Self::take) made one.
    pub fn archive(&self) -> Option<&Path> {
        self.archive.as_deref()
    }

    /// How many files were handed over.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether nothing was handed over.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Unpack `archive` — the sandbox's final copy — into `dir`, then
    /// [`merge`](Self::merge) it.
    pub fn merge_archive(&self, archive: &Path, dir: &Path, keep: &Path) -> Result<MergeReport> {
        // The sandbox wrote this archive: nothing in it may reach outside
        // `dir` while it is unpacked, before the merge checks anything.
        unpack(archive, dir)?;
        self.merge(dir, keep)
    }

    /// Merge the sandbox's final copy of the workspace, unpacked in
    /// `result`. On a conflict nothing is applied, `result` is moved to
    /// `keep`, and the error lists the conflicting files.
    pub fn merge(&self, result: &Path, keep: &Path) -> Result<MergeReport> {
        let mut theirs = BTreeMap::new();
        let mut found = Vec::new();
        walk(result, Path::new(""), &mut found)?;
        for relative in found.into_iter().filter(|p| !excluded(p)) {
            if let Some(entry) = entry_of(&result.join(&relative))? {
                theirs.insert(relative, entry);
            }
        }

        let created: Vec<&PathBuf> = theirs
            .keys()
            .filter(|p| !self.files.contains_key(*p))
            .collect();
        let ignored = if self.git {
            git_ignored(&self.workspace, &created)?
        } else {
            Vec::new()
        };

        let mut writes: Vec<(&PathBuf, &Entry)> = Vec::new();
        let mut deletes: Vec<&PathBuf> = Vec::new();
        let mut conflicts: Vec<&PathBuf> = Vec::new();

        for (path, entry) in &theirs {
            let base = self.files.get(path);
            if base == Some(entry) || (base.is_none() && ignored.contains(path)) {
                continue;
            }
            let ours = entry_of(&self.workspace.join(path))?;
            if ours.as_ref() == Some(entry) {
                continue;
            }
            if ours.as_ref() == base {
                writes.push((path, entry));
            } else {
                conflicts.push(path);
            }
        }
        for (path, base) in &self.files {
            if theirs.contains_key(path) {
                continue;
            }
            match entry_of(&self.workspace.join(path))? {
                None => {}
                Some(ours) if &ours == base => deletes.push(path),
                Some(_) => conflicts.push(path),
            }
        }

        if !conflicts.is_empty() {
            keep_copy(result, keep)?;
            let shown: Vec<String> = conflicts
                .iter()
                .take(10)
                .map(|p| p.display().to_string())
                .collect();
            return Err(MetaAgentError::Other(format!(
                "{} file(s) changed both in the sandbox and in the workspace ({}{}); nothing was \
                 applied, and the sandbox's copy is kept in {} for a manual merge",
                conflicts.len(),
                shown.join(", "),
                if conflicts.len() > shown.len() {
                    ", …"
                } else {
                    ""
                },
                keep.display()
            )));
        }

        // Every target is checked before anything is written, so a refused
        // path cannot leave half of the change applied.
        for path in writes
            .iter()
            .map(|(path, _)| *path)
            .chain(deletes.iter().copied())
        {
            self.inside(path)?;
        }
        // Nothing here may lead outside the workspace on the sandbox's word:
        // a link it made or changed that points out of it is not applied.
        let outward: Vec<String> = writes
            .iter()
            .filter_map(|(path, entry)| match entry {
                Entry::Link(points_to) if link_escapes(path, points_to) => {
                    Some(format!("{} -> {}", path.display(), points_to.display()))
                }
                _ => None,
            })
            .collect();
        if !outward.is_empty() {
            keep_copy(result, keep)?;
            return Err(MetaAgentError::Security(format!(
                "nothing was applied: the sandbox made links that point outside the workspace \
                 ({}); its copy is kept in {}",
                outward.join(", "),
                keep.display()
            )));
        }
        // Every new file is first written beside its target, so a failure
        // while writing — a full disk, a read-only directory — changes
        // nothing. Then each is renamed into place, what it replaces and what
        // is deleted set aside until every step has succeeded, and put back
        // if one fails.
        let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
        if let Err(err) = self.stage(result, &writes, &mut staged) {
            for (stage, _) in &staged {
                let _ = std::fs::remove_file(stage);
            }
            return Err(err);
        }
        let mut placed: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
        let mut set_aside: Vec<(PathBuf, PathBuf)> = Vec::new();
        if let Err(err) = self.commit(&staged, &deletes, &mut placed, &mut set_aside) {
            for (target, old) in set_aside.iter().rev() {
                let _ = std::fs::rename(old, target);
            }
            for (target, replaced) in placed.iter().rev() {
                let _ = std::fs::remove_file(target);
                if let Some(old) = replaced {
                    let _ = std::fs::rename(old, target);
                }
            }
            for (stage, _) in &staged {
                let _ = std::fs::remove_file(stage);
            }
            return Err(err);
        }
        for old in placed
            .iter()
            .filter_map(|(_, replaced)| replaced.as_ref())
            .chain(set_aside.iter().map(|(_, old)| old))
        {
            if let Err(err) = std::fs::remove_file(old) {
                tracing::warn!(path = %old.display(), error = %err, "removing a replaced file failed");
            }
        }
        Ok(MergeReport {
            written: writes.len(),
            deleted: deletes.len(),
        })
    }

    /// Rename each staged file into place and set each deleted one aside,
    /// recording every step in `placed` and `set_aside` so a failure can
    /// undo them.
    fn commit(
        &self,
        staged: &[(PathBuf, PathBuf)],
        deletes: &[&PathBuf],
        placed: &mut Vec<(PathBuf, Option<PathBuf>)>,
        set_aside: &mut Vec<(PathBuf, PathBuf)>,
    ) -> Result<()> {
        // What each target held is kept under a name this merge made, never
        // moved over anything: the rename onto the target then replaces it
        // in one step, and undoing it is a rename back.
        for (stage, target) in staged {
            let replaced = set_aside_copy(target)?;
            placed.push((target.clone(), replaced));
            std::fs::rename(stage, target).map_err(io("writing a file"))?;
        }
        for path in deletes {
            let target = self.inside(path)?;
            if let Some(old) = set_aside_copy(&target)? {
                std::fs::remove_file(&target).map_err(io("deleting a file"))?;
                set_aside.push((target, old));
            }
        }
        Ok(())
    }

    /// Write each of `writes` beside its target, under a name only this merge
    /// made, recording each in `staged` so a failure can remove it. A file
    /// interrupted while written is removed with its handle.
    fn stage(
        &self,
        result: &Path,
        writes: &[(&PathBuf, &Entry)],
        staged: &mut Vec<(PathBuf, PathBuf)>,
    ) -> Result<()> {
        for (path, entry) in writes {
            let target = self.inside(path)?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(io("creating a directory"))?;
                self.check_inside(parent)?;
            }
            if target.is_dir() && !target.is_symlink() {
                return Err(MetaAgentError::Other(format!(
                    "{} is a directory here and a file in the sandbox",
                    path.display()
                )));
            }
            let stage = match entry {
                Entry::File { executable, .. } => {
                    let mut source = std::fs::File::open(result.join(path))
                        .map_err(io("reading the sandbox's copy"))?;
                    let (parent, prefix) = scratch_place(&target);
                    // Created exclusively under a fresh name: nothing that
                    // was there is opened, followed or replaced.
                    let mut file = tempfile::Builder::new()
                        .prefix(&prefix)
                        .suffix(".new")
                        .tempfile_in(parent)
                        .map_err(io("writing a file"))?;
                    std::io::copy(&mut source, file.as_file_mut()).map_err(io("writing a file"))?;
                    let mode = if *executable { 0o755 } else { 0o644 };
                    file.as_file()
                        .set_permissions(std::fs::Permissions::from_mode(mode))
                        .map_err(io("setting permissions"))?;
                    file.keep().map(|(_, path)| path).map_err(|err| {
                        MetaAgentError::Other(format!("keeping a staged file: {err}"))
                    })?
                }
                Entry::Link(points_to) => scratch(&target, ".new", |path| {
                    std::os::unix::fs::symlink(points_to, path)
                })?,
            };
            staged.push((stage, target));
        }
        Ok(())
    }

    /// `relative` within the workspace, refusing anything that would leave it.
    fn inside(&self, relative: &Path) -> Result<PathBuf> {
        if relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(MetaAgentError::Security(format!(
                "the sandbox's copy names {}, outside the workspace",
                relative.display()
            )));
        }
        let target = self.workspace.join(relative);
        // The nearest directory that exists decides where the rest would be
        // created: a link anywhere along the way is caught here.
        if let Some(existing) = target.ancestors().skip(1).find(|p| p.exists()) {
            self.check_inside(existing)?;
        }
        Ok(target)
    }

    /// Refuse a directory that resolves outside the workspace — reached
    /// through a symbolic link, say.
    fn check_inside(&self, directory: &Path) -> Result<()> {
        let resolved = std::fs::canonicalize(directory).map_err(io("resolving a directory"))?;
        if resolved.starts_with(&self.workspace) {
            Ok(())
        } else {
            Err(MetaAgentError::Security(format!(
                "{} resolves outside the workspace",
                directory.display()
            )))
        }
    }
}

/// CUMA's state and git's directory never take part in a merge.
fn excluded(relative: &Path) -> bool {
    matches!(
        relative.components().next(),
        Some(Component::Normal(first)) if first == ".cuma" || first == ".git"
    )
}

fn entry_of(path: &Path) -> Result<Option<Entry>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(MetaAgentError::Other(format!("{}: {err}", path.display()))),
    };
    if metadata.file_type().is_symlink() {
        return Ok(Some(Entry::Link(
            std::fs::read_link(path).map_err(io("reading a link"))?,
        )));
    }
    if !metadata.is_file() {
        return Ok(None);
    }
    let mut file = std::fs::File::open(path).map_err(io("reading a file"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io("reading a file"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&hasher.finalize());
    Ok(Some(Entry::File {
        digest,
        executable: metadata.permissions().mode() & 0o111 != 0,
    }))
}

/// Every file and link under `dir`, relative to the walk's root, without
/// following links.
fn walk(dir: &Path, relative: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(io("listing a directory"))?;
    for entry in entries {
        let entry = entry.map_err(io("listing a directory"))?;
        let name = relative.join(entry.file_name());
        if excluded(&name) {
            continue;
        }
        let kind = entry.file_type().map_err(io("listing a directory"))?;
        if kind.is_dir() {
            walk(&entry.path(), &name, out)?;
        } else if kind.is_file() || kind.is_symlink() {
            out.push(name);
        }
    }
    Ok(())
}

fn is_git_work_tree(workspace: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .is_ok_and(|out| out.status.success() && out.stdout.starts_with(b"true"))
}

/// What git tracks or would track: tracked files and untracked ones it does
/// not ignore, as they exist on disk.
fn git_listed(workspace: &Path) -> Result<Vec<PathBuf>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .map_err(io("running git"))?;
    if !out.status.success() {
        return Err(MetaAgentError::Other(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut paths: Vec<PathBuf> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| PathBuf::from(String::from_utf8_lossy(name).into_owned()))
        .filter(|p| workspace.join(p).symlink_metadata().is_ok())
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Which of `paths` git ignores.
fn git_ignored(workspace: &Path, paths: &[&PathBuf]) -> Result<Vec<PathBuf>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_os_str().as_encoded_bytes());
        input.push(0);
    }
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(io("running git"))?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write as _;
        stdin.write_all(&input).map_err(io("running git"))?;
    }
    let out = child.wait_with_output().map_err(io("running git"))?;
    // Exit 1 means "none ignored"; anything above is an error, which is
    // treated as "none ignored" — bringing a file back is the safe side.
    Ok(out
        .stdout
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| PathBuf::from(String::from_utf8_lossy(name).into_owned()))
        .collect())
}

/// Keep `result` at `keep`, moving it when possible.
fn keep_copy(result: &Path, keep: &Path) -> Result<()> {
    if let Some(parent) = keep.parent() {
        std::fs::create_dir_all(parent).map_err(io("keeping the sandbox's copy"))?;
    }
    if std::fs::rename(result, keep).is_ok() {
        return Ok(());
    }
    let status = std::process::Command::new("cp")
        .arg("-R")
        .arg(result)
        .arg(keep)
        .status()
        .map_err(io("keeping the sandbox's copy"))?;
    if status.success() {
        Ok(())
    } else {
        Err(MetaAgentError::Other(format!(
            "could not keep the sandbox's copy in {}",
            keep.display()
        )))
    }
}

fn io(what: &'static str) -> impl Fn(std::io::Error) -> MetaAgentError {
    move |err| MetaAgentError::Other(format!("{what}: {err}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn write(root: &Path, path: &str, text: &str) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }

    fn read(root: &Path, path: &str) -> Option<String> {
        std::fs::read_to_string(root.join(path)).ok()
    }

    /// A workspace, its snapshot, and the sandbox's copy unpacked from the
    /// snapshot's archive — what a copied sandbox starts from.
    fn handed_over(
        setup: impl Fn(&Path),
    ) -> (tempfile::TempDir, Snapshot, tempfile::TempDir, PathBuf) {
        let ws = tempfile::tempdir().unwrap();
        setup(ws.path());
        let work = tempfile::tempdir().unwrap();
        let snapshot = Snapshot::take(ws.path(), work.path()).unwrap();
        let sandbox = work.path().join("sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        let ok = std::process::Command::new("tar")
            .arg("-xf")
            .arg(snapshot.archive().unwrap())
            .arg("-C")
            .arg(&sandbox)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        (ws, snapshot, work, sandbox)
    }

    fn plain(root: &Path) {
        write(root, "keep.txt", "unchanged\n");
        write(root, "edit.txt", "before\n");
        write(root, "gone.txt", "delete me\n");
        write(root, ".cuma/runtime.db", "state");
    }

    #[test]
    fn work_done_in_the_sandbox_is_applied() {
        let (ws, snapshot, work, sandbox) = handed_over(plain);
        assert!(
            !sandbox.join(".cuma").exists(),
            "CUMA's state is not uploaded"
        );
        write(&sandbox, "edit.txt", "after\n");
        write(&sandbox, "src/new.rs", "fn main() {}\n");
        std::fs::remove_file(sandbox.join("gone.txt")).unwrap();

        let report = snapshot.merge(&sandbox, &work.path().join("keep")).unwrap();

        assert_eq!(
            report,
            MergeReport {
                written: 2,
                deleted: 1
            }
        );
        assert_eq!(read(ws.path(), "edit.txt").unwrap(), "after\n");
        assert_eq!(read(ws.path(), "src/new.rs").unwrap(), "fn main() {}\n");
        assert_eq!(read(ws.path(), "gone.txt"), None);
        assert_eq!(read(ws.path(), "keep.txt").unwrap(), "unchanged\n");
        assert_eq!(read(ws.path(), ".cuma/runtime.db").unwrap(), "state");
    }

    #[test]
    fn a_file_changed_in_both_places_applies_nothing_and_keeps_the_sandbox_copy() {
        let (ws, snapshot, work, sandbox) = handed_over(plain);
        write(&sandbox, "edit.txt", "agent's version\n");
        write(&sandbox, "src/new.rs", "new\n");
        write(ws.path(), "edit.txt", "user's version\n");
        let keep = ws.path().join(".cuma/sandbox-results/s1");

        let err = snapshot.merge(&sandbox, &keep).unwrap_err().to_string();

        assert!(err.contains("edit.txt"), "{err}");
        assert_eq!(read(ws.path(), "edit.txt").unwrap(), "user's version\n");
        assert_eq!(read(ws.path(), "src/new.rs"), None, "all or nothing");
        assert_eq!(read(&keep, "edit.txt").unwrap(), "agent's version\n");
        drop(work);
    }

    #[test]
    fn the_same_change_in_both_places_is_not_a_conflict() {
        let (ws, snapshot, work, sandbox) = handed_over(plain);
        write(&sandbox, "edit.txt", "same\n");
        write(ws.path(), "edit.txt", "same\n");
        let report = snapshot.merge(&sandbox, &work.path().join("keep")).unwrap();
        assert_eq!(report, MergeReport::default());
    }

    #[test]
    fn a_file_deleted_in_the_sandbox_but_changed_here_is_a_conflict() {
        let (ws, snapshot, work, sandbox) = handed_over(plain);
        std::fs::remove_file(sandbox.join("gone.txt")).unwrap();
        write(ws.path(), "gone.txt", "edited here\n");
        assert!(snapshot.merge(&sandbox, &work.path().join("keep")).is_err());
        assert_eq!(read(ws.path(), "gone.txt").unwrap(), "edited here\n");
    }

    /// One member of a hand-written archive.
    struct Member {
        name: Vec<u8>,
        kind: u8,
        link: Vec<u8>,
        data: Vec<u8>,
        mode: &'static [u8],
    }

    fn member(name: &[u8], kind: u8, link: &[u8], data: &[u8]) -> Member {
        Member {
            name: name.to_vec(),
            kind,
            link: link.to_vec(),
            data: data.to_vec(),
            mode: b"0000644\0",
        }
    }

    fn file(name: &[u8], data: &[u8]) -> Member {
        member(name, b'0', b"", data)
    }

    /// A ustar archive written by hand: what a hostile sandbox could hand
    /// back, byte for byte.
    fn ustar(members: &[Member]) -> Vec<u8> {
        let mut out = Vec::new();
        for m in members {
            let mut header = [0u8; 512];
            let field = |header: &mut [u8; 512], at: usize, value: &[u8]| {
                header[at..at + value.len()].copy_from_slice(value);
            };
            field(&mut header, 0, &m.name[..m.name.len().min(100)]);
            field(&mut header, 100, m.mode);
            field(&mut header, 108, b"0000000\0");
            field(&mut header, 116, b"0000000\0");
            field(
                &mut header,
                124,
                format!("{:011o}\0", m.data.len()).as_bytes(),
            );
            field(&mut header, 136, b"00000000000\0");
            field(&mut header, 148, b"        ");
            header[156] = m.kind;
            field(&mut header, 157, &m.link[..m.link.len().min(100)]);
            field(&mut header, 257, b"ustar\x0000");
            let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
            field(&mut header, 148, format!("{sum:06o}\0 ").as_bytes());
            out.extend_from_slice(&header);
            out.extend_from_slice(&m.data);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
        out.resize(out.len() + 1024, 0);
        out
    }

    /// Unpack hand-written `members` into a fresh `out` under `dir`.
    fn unpacked(dir: &Path, case: &str, members: &[Member]) -> Result<PathBuf> {
        let archive = dir.join(format!("{case}.tar"));
        std::fs::write(&archive, ustar(members)).unwrap();
        let out = dir.join(case);
        unpack(&archive, &out).map(|()| out)
    }

    #[test]
    fn an_archive_reaching_outside_where_it_is_unpacked_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let refusal = |case: &str, members: &[Member]| {
            unpacked(dir.path(), case, members).unwrap_err().to_string()
        };
        // A link the archive plants, then a file written through it.
        let through = refusal(
            "through",
            &[
                member(b"evil", b'2', b"/tmp", b""),
                file(b"evil/pwned", b"x\n"),
            ],
        );
        assert!(through.contains("below a file or link"), "{through}");
        assert!(refusal("up", &[file(b"../escape", b"x\n")]).contains("outside"));
        assert!(refusal("abs", &[file(b"/tmp/cuma-abs", b"x\n")]).contains("outside"));
        // A hard link may only repeat a file unpacked before it.
        let hard = refusal("hard", &[member(b"passwd", b'1', b"../../etc/passwd", b"")]);
        assert!(hard.contains("outside"), "{hard}");
        let unknown = refusal("unknown", &[member(b"copy", b'1', b"later.txt", b"")]);
        assert!(unknown.contains("hard link"), "{unknown}");
        let device = refusal("device", &[member(b"null", b'3', b"", b"")]);
        assert!(device.contains("not a file"), "{device}");
        // Nothing that was already there is unpacked into.
        std::fs::create_dir_all(dir.path().join("taken")).unwrap();
        assert!(unpacked(dir.path(), "taken", &[file(b"a", b"x")]).is_err());

        // An ordinary copy is written as it was.
        let mut tool = file(b"./bin/tool", b"#!/bin/sh\n");
        tool.mode = b"0000755\0";
        let out = unpacked(
            dir.path(),
            "ok",
            &[
                member(b"./src/", b'5', b"", b""),
                file(b"./src/main.rs", b"fn main() {}\n"),
                member(b"./src/again.rs", b'1', b"./src/main.rs", b""),
                member(b"./latest", b'2', b"src/main.rs", b""),
                tool,
            ],
        )
        .unwrap();
        assert_eq!(read(&out, "src/main.rs").unwrap(), "fn main() {}\n");
        assert_eq!(read(&out, "src/again.rs").unwrap(), "fn main() {}\n");
        assert_eq!(
            std::fs::read_link(out.join("latest")).unwrap(),
            Path::new("src/main.rs")
        );
        let mode = std::fs::metadata(out.join("bin/tool"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "the executable bit comes along");
    }

    #[test]
    fn a_name_with_a_newline_is_judged_whole() {
        use std::os::unix::ffi::OsStrExt as _;
        let dir = tempfile::tempdir().unwrap();
        // Read line by line, as `tar -t` prints it, this is two names.
        let out = unpacked(dir.path(), "newline", &[file(b"notes\nevil", b"x\n")]).unwrap();
        let names: Vec<Vec<u8>> = std::fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name().as_bytes().to_vec())
            .collect();
        assert_eq!(names, [b"notes\nevil".to_vec()]);
        // A climb after a newline is still a climb.
        let climb = unpacked(dir.path(), "climb", &[file(b"safe\n/../../escape", b"x")]);
        assert!(climb.unwrap_err().to_string().contains("outside"));
    }

    #[test]
    fn long_names_and_pax_paths_are_read_as_tar_writes_them() {
        let dir = tempfile::tempdir().unwrap();
        let long = format!("{}/file.rs", "deep".repeat(40));
        let mut gnu_name = long.clone().into_bytes();
        gnu_name.push(0);
        let pax_path = format!("pax/{}/named.rs", "p".repeat(150));
        let record = |key: &str, value: &str| {
            let body = format!(" {key}={value}\n");
            let mut length = body.len() + 1;
            while length.to_string().len() + body.len() != length {
                length += 1;
            }
            format!("{length}{body}")
        };
        let out = unpacked(
            dir.path(),
            "long",
            &[
                member(b"././@LongLink", b'L', b"", &gnu_name),
                file(&long.as_bytes()[..100], b"gnu\n"),
                member(
                    b"PaxHeader/named",
                    b'x',
                    b"",
                    record("path", &pax_path).as_bytes(),
                ),
                file(b"truncated-name.rs", b"pax\n"),
            ],
        )
        .unwrap();
        assert_eq!(read(&out, &long).unwrap(), "gnu\n");
        assert_eq!(read(&out, &pax_path).unwrap(), "pax\n");
        assert!(!out.join("truncated-name.rs").exists());
    }

    #[test]
    fn an_archive_the_system_tar_writes_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let long = format!("{}/leaf name ç.txt", "nested".repeat(20));
        write(&src, &long, "long\n");
        write(&src, "a b/c.txt", "spaced\n");
        write(&src, "run.sh", "#!/bin/sh\n");
        std::fs::set_permissions(src.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::os::unix::fs::symlink("a b/c.txt", src.join("link")).unwrap();
        let archive = dir.path().join("system.tar");
        let made = std::process::Command::new("tar")
            .env("COPYFILE_DISABLE", "1")
            .arg("-cf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg(".")
            .status()
            .unwrap();
        assert!(made.success());

        let out = dir.path().join("out");
        unpack(&archive, &out).unwrap();

        assert_eq!(read(&out, &long).unwrap(), "long\n");
        assert_eq!(read(&out, "a b/c.txt").unwrap(), "spaced\n");
        let mode = std::fs::metadata(out.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111);
        assert_eq!(
            std::fs::read_link(out.join("link")).unwrap(),
            Path::new("a b/c.txt")
        );
    }

    #[test]
    fn a_link_pointing_outside_the_workspace_is_not_applied() {
        for target in ["/etc/passwd", "../../outside", "sub/../../outside"] {
            let (ws, snapshot, work, sandbox) = handed_over(plain);
            std::os::unix::fs::symlink(target, sandbox.join("escape")).unwrap();
            write(&sandbox, "edit.txt", "after\n");
            let keep = work.path().join("keep");

            let err = snapshot.merge(&sandbox, &keep).unwrap_err().to_string();

            assert!(err.contains("outside the workspace"), "{target}: {err}");
            assert!(
                ws.path().join("escape").symlink_metadata().is_err(),
                "{target}"
            );
            assert_eq!(
                read(ws.path(), "edit.txt").unwrap(),
                "before\n",
                "nothing applied"
            );
            assert!(
                keep.join("escape").symlink_metadata().is_ok(),
                "the copy is kept"
            );
        }
        // A link that stays inside is applied like any change.
        let (ws, snapshot, work, sandbox) = handed_over(plain);
        std::os::unix::fs::symlink("keep.txt", sandbox.join("alias")).unwrap();
        snapshot.merge(&sandbox, &work.path().join("keep")).unwrap();
        assert_eq!(
            std::fs::read_link(ws.path().join("alias")).unwrap(),
            Path::new("keep.txt")
        );
    }

    #[test]
    fn a_merge_never_touches_a_file_it_did_not_create() {
        let (ws, snapshot, work, sandbox) = handed_over(plain);
        write(&sandbox, "edit.txt", "after\n");
        std::fs::remove_file(sandbox.join("gone.txt")).unwrap();
        // Names a merge with predictable scratch names would have used: a
        // link to a file outside, and files of the user's own.
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, "precious\n").unwrap();
        let pid = std::process::id();
        let planted = [
            (format!(".edit.txt.cuma-{pid}-0.new"), None),
            (format!(".edit.txt.cuma-{pid}-0.old"), Some("mine\n")),
            (format!(".gone.txt.cuma-{pid}-1.old"), Some("mine too\n")),
        ];
        for (name, content) in &planted {
            match content {
                Some(text) => write(ws.path(), name, text),
                None => std::os::unix::fs::symlink(&victim, ws.path().join(name)).unwrap(),
            }
        }

        snapshot.merge(&sandbox, &work.path().join("keep")).unwrap();

        assert_eq!(read(ws.path(), "edit.txt").unwrap(), "after\n");
        assert!(!ws.path().join("gone.txt").exists());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious\n");
        for (name, content) in &planted {
            match content {
                Some(text) => assert_eq!(read(ws.path(), name).unwrap(), *text, "{name}"),
                None => assert!(ws.path().join(name).is_symlink(), "{name}"),
            }
        }
        let scratch: Vec<String> = std::fs::read_dir(ws.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".cuma-"))
            .collect();
        assert_eq!(
            scratch.len(),
            planted.len(),
            "nothing of the merge's is left: {scratch:?}"
        );
    }

    #[test]
    fn the_executable_bit_comes_back() {
        let (ws, snapshot, work, sandbox) = handed_over(plain);
        write(&sandbox, "run.sh", "#!/bin/sh\n");
        std::fs::set_permissions(
            sandbox.join("run.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        snapshot.merge(&sandbox, &work.path().join("keep")).unwrap();
        let mode = std::fs::metadata(ws.path().join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0);
    }

    fn git(root: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn a_merge_that_cannot_write_everything_writes_nothing() {
        let (ws, snapshot, work, sandbox) = handed_over(|root| {
            write(root, "a.txt", "v1\n");
            write(root, "locked/b.txt", "v1\n");
            write(root, "gone.txt", "delete me\n");
        });
        write(&sandbox, "a.txt", "v2\n");
        write(&sandbox, "locked/b.txt", "v2\n");
        std::fs::remove_file(sandbox.join("gone.txt")).unwrap();
        // The second write cannot happen: its directory is read-only here.
        let locked = ws.path().join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::write(locked.join("probe"), "").is_ok() {
            // Running as root: permissions cannot make a write fail.
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let merged = snapshot.merge(&sandbox, &work.path().join("keep"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(merged.is_err());
        assert_eq!(read(ws.path(), "a.txt").unwrap(), "v1\n", "nothing applied");
        assert_eq!(read(ws.path(), "gone.txt").unwrap(), "delete me\n");
        let mut left: Vec<String> = std::fs::read_dir(ws.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["a.txt", "gone.txt", "locked"],
            "no staged file is left"
        );
    }

    #[test]
    fn a_workspace_below_the_repository_root_is_copied_whole() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        write(repo.path(), "top.txt", "outside the workspace\n");
        write(repo.path(), "sub/a.txt", "tracked\n");
        write(repo.path(), "sub/inner/c.txt", "tracked deeper\n");
        write(repo.path(), "sub/.gitignore", "ignored.log\n");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-qm", "init"]);
        write(repo.path(), "sub/b.txt", "untracked\n");
        write(repo.path(), "sub/ignored.log", "ignored\n");
        let work = tempfile::tempdir().unwrap();

        let snapshot = Snapshot::take(&repo.path().join("sub"), work.path()).unwrap();

        let listed = std::process::Command::new("tar")
            .arg("-tf")
            .arg(snapshot.archive().unwrap())
            .output()
            .unwrap();
        let names: Vec<String> = String::from_utf8_lossy(&listed.stdout)
            .lines()
            .map(|name| name.trim_start_matches("./").to_owned())
            .collect();
        for present in ["a.txt", "b.txt", "inner/c.txt", ".gitignore"] {
            assert!(
                names.iter().any(|n| n == present),
                "{present} missing: {names:?}"
            );
        }
        for absent in ["ignored.log", "top.txt", "sub/a.txt"] {
            assert!(
                !names.iter().any(|n| n == absent),
                "{absent} copied: {names:?}"
            );
        }
    }

    #[test]
    fn in_a_repository_ignored_files_neither_go_nor_come_back_and_git_is_not_merged() {
        let (ws, snapshot, work, sandbox) = handed_over(|root| {
            git(root, &["init", "-q"]);
            write(root, ".gitignore", "node_modules/\n");
            write(root, "app.js", "v1\n");
            write(root, "untracked.txt", "not added yet\n");
            write(root, "node_modules/dep/index.js", "huge\n");
            git(root, &["add", ".gitignore", "app.js"]);
            git(root, &["commit", "-qm", "init"]);
        });
        assert!(
            sandbox.join("untracked.txt").exists(),
            "untracked files travel"
        );
        assert!(
            !sandbox.join("node_modules").exists(),
            "ignored files stay behind"
        );
        assert!(sandbox.join(".git").exists(), "the agent can use git");

        write(&sandbox, "app.js", "v2\n");
        write(
            &sandbox,
            "node_modules/other/index.js",
            "installed in the sandbox\n",
        );
        git(&sandbox, &["commit", "-qam", "agent commit"]);

        let report = snapshot.merge(&sandbox, &work.path().join("keep")).unwrap();

        assert_eq!(
            report,
            MergeReport {
                written: 1,
                deleted: 0
            }
        );
        assert_eq!(read(ws.path(), "app.js").unwrap(), "v2\n");
        assert!(!ws.path().join("node_modules/other").exists());
        let log = std::process::Command::new("git")
            .arg("-C")
            .arg(ws.path())
            .args(["log", "--oneline"])
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&log.stdout).contains("agent commit"),
            "commits made in the sandbox stay there"
        );
    }

    #[test]
    fn a_path_that_would_leave_the_workspace_is_refused() {
        let (ws, snapshot, work, sandbox) = handed_over(plain);
        // A directory here that is really a link to somewhere else.
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("escape")).unwrap();
        write(&sandbox, "escape/payload.txt", "x\n");
        // The sandbox's copy has a real directory where the host has a link.
        let err = snapshot.merge(&sandbox, &work.path().join("keep"));
        assert!(err.is_err());
        assert!(!outside.path().join("payload.txt").exists());
    }
}
