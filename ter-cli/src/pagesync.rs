//! The lesson page's code and the exercise folder's, kept in step.
//!
//! "The set" is the code the lesson page edits and a page Run writes into
//! the folder: the files the site's starter has under `src/`. Nothing
//! outside it is read, sent or written here.
//!
//! ter records what it writes to the set in `.ter/page-sync.json` (a hash
//! per file), with the starter as the site sent it last, the lesson the
//! exercise is on and the last draft ter saved to the account. From that:
//!
//! - `ter serve`'s `/code` tells the page whether the folder's set was
//!   edited here since ter last wrote it (`local_edit`);
//! - a page Run that would replace code edited here copies the set to
//!   `.ter/backup/<UTC stamp>/` first, keeping the newest
//!   [`BACKUPS_KEPT`];
//! - a terminal `ter run` saves the set to the account's draft of the
//!   lesson, by the lesson page's own rules, unless it is unchanged since;
//! - `ter ex fetch` of a new folder puts the account's draft over the
//!   starter.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ter_sdk::project::{GITIGNORE, STATE_DIR, TerToml, learner_owned, safe_relative};
use ter_sdk::{Client, Draft, Exercise};

use crate::output::{CliError, debug};
use crate::session::Session;

/// What ter keeps about the set, under [`STATE_DIR`].
pub const PAGE_SYNC: &str = "page-sync.json";
/// Where a page Run or `--page-code` puts the code it replaces, under
/// [`STATE_DIR`].
pub const BACKUP_DIR: &str = "backup";
/// How many backups a folder keeps: the newest.
pub const BACKUPS_KEPT: usize = 10;

/// `.ter/page-sync.json`. Hashes are sha256, in hex, of a file's bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageSync {
    /// The lesson the exercise is on: the account's drafts are by lesson.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lesson: Option<String>,
    /// The lesson page's stamp of the starter the site sent last.
    #[serde(default)]
    pub starter: String,
    /// The set as the starter has it.
    #[serde(default)]
    pub starter_files: BTreeMap<String, String>,
    /// The set as ter last wrote it; `None` until ter has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<BTreeMap<String, String>>,
    /// The stamp the account's draft goes on carrying: the starter the
    /// code was first written over, as the lesson page keeps it until
    /// Reset to starter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_starter: Option<String>,
    /// The draft the account is known to hold, as [`draft_hash`]: saved by
    /// ter, or the code a page Run or a draft wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved: Option<String>,
}

fn sync_path(dir: &Path) -> PathBuf {
    dir.join(STATE_DIR).join(PAGE_SYNC)
}

fn io(what: &str, path: &Path, e: std::io::Error) -> CliError {
    CliError::new(
        "io_error",
        format!("Could not {what} {}: {e}", path.display()),
    )
}

impl PageSync {
    /// The record in `dir`, `None` when there is none or it does not read.
    pub fn load(dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(sync_path(dir)).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self, dir: &Path) -> Result<(), CliError> {
        let path = sync_path(dir);
        let parent = path.parent().expect("has a parent");
        std::fs::create_dir_all(parent).map_err(|e| io("create", parent, e))?;
        let body = serde_json::to_string_pretty(self).expect("the record serializes");
        std::fs::write(&path, body + "\n").map_err(|e| io("write", &path, e))
    }

    /// The paths of the set. Only safe paths under `src/` count, so a
    /// record edited by hand never points elsewhere.
    fn paths(&self) -> Vec<String> {
        self.starter_files
            .keys()
            .filter(|p| in_set(p))
            .cloned()
            .collect()
    }

    /// What a file of the set is measured against for a local edit: what
    /// ter wrote last, else the starter.
    fn baseline(&self) -> &BTreeMap<String, String> {
        self.written.as_ref().unwrap_or(&self.starter_files)
    }
}

/// Whether `path` can be in the set: a safe path under `src/`.
fn in_set(path: &str) -> bool {
    path.starts_with("src/") && safe_relative(path).is_some_and(|rel| learner_owned(&rel))
}

pub fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The lesson page's stamp of a starter (liveRun.ts `starterStamp`): FNV-1a
/// over the files sorted by path, each `path NUL content`, joined by
/// U+0001, in UTF-16 code units as JavaScript reads a string, and the count
/// of files in front. It only has to differ when the starter does.
pub fn starter_stamp<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut files: Vec<(&str, &str)> = files.into_iter().collect();
    files.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    let text = files
        .iter()
        .map(|(path, content)| format!("{path}\u{0}{content}"))
        .collect::<Vec<_>>()
        .join("\u{1}");
    let mut stamp: u32 = 0x811c_9dc5;
    for unit in text.encode_utf16() {
        stamp ^= u32::from(unit);
        stamp = stamp.wrapping_mul(0x0100_0193);
    }
    format!("{}-{stamp:x}", files.len())
}

/// The files the lesson page's editor holds for an exercise, as the site
/// gives them: `src/` and `Cargo.toml`. Only an exercise in the workspace
/// layout saves drafts, and that is what its editor shows.
fn editor_files(exercise: &Exercise) -> impl Iterator<Item = (&str, &str)> {
    exercise
        .files
        .iter()
        .filter(|f| f.path.starts_with("src/") || f.path == "Cargo.toml")
        .map(|f| (f.path.as_str(), f.content.as_str()))
}

/// The set as the site's starter has it: path to bytes.
pub fn starter_set(exercise: &Exercise) -> BTreeMap<String, Vec<u8>> {
    exercise
        .files
        .iter()
        .filter(|f| in_set(&f.path) && f.path != GITIGNORE)
        .filter_map(|f| Some((f.path.clone(), f.bytes().ok()?)))
        .collect()
}

/// Record the starter the site sent for the folder `dir` (and the lesson,
/// when known). `fetched`: ter just wrote the starter's set, so that is
/// what ter last wrote.
pub fn note_starter(
    dir: &Path,
    exercise: &Exercise,
    lesson: Option<&str>,
    fetched: bool,
) -> Result<(), CliError> {
    let mut sync = PageSync::load(dir).unwrap_or_default();
    sync.starter = starter_stamp(editor_files(exercise));
    sync.starter_files = starter_set(exercise)
        .iter()
        .map(|(p, b)| (p.clone(), hash(b)))
        .collect();
    if let Some(lesson) = lesson {
        sync.lesson = Some(lesson.to_string());
    }
    if fetched {
        sync.written = Some(sync.starter_files.clone());
        sync.draft_starter = None;
        sync.saved = None;
    }
    sync.save(dir)
}

/// The paths of the set in `dir`: from the record, else every file under
/// `src/` (a folder fetched before ter kept one).
fn set_paths(dir: &Path, sync: Option<&PageSync>) -> Vec<String> {
    if let Some(sync) = sync {
        return sync.paths();
    }
    let mut out = Vec::new();
    walk(dir, &dir.join("src"), &mut out);
    out.sort();
    out
}

fn walk(root: &Path, at: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(at) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            walk(root, &path, out);
        } else if kind.is_file()
            && let Ok(rel) = path.strip_prefix(root)
        {
            let rel = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if in_set(&rel) {
                out.push(rel);
            }
        }
    }
}

/// The files of the set that are in `dir`, path to bytes. A link is not
/// followed.
fn read_set(dir: &Path, paths: &[String]) -> BTreeMap<String, Vec<u8>> {
    paths
        .iter()
        .filter_map(|p| {
            let path = dir.join(p);
            let meta = std::fs::symlink_metadata(&path).ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((p.clone(), std::fs::read(&path).ok()?))
        })
        .collect()
}

fn hashes(files: &BTreeMap<String, Vec<u8>>) -> BTreeMap<String, String> {
    files.iter().map(|(p, b)| (p.clone(), hash(b))).collect()
}

/// When the set last changed: the newest mtime of its files, in ms since
/// the epoch; 0 for none.
pub fn changed_at_ms(dir: &Path, paths: &[String]) -> u64 {
    paths
        .iter()
        .filter_map(|p| std::fs::metadata(dir.join(p)).ok()?.modified().ok())
        .filter_map(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .max()
        .unwrap_or(0)
}

/// Whether the set in `dir` differs from what ter last wrote there. With
/// no record, whether the folder's source differs from what was fetched.
fn local_edit(dir: &Path, sync: Option<&PageSync>, current: &BTreeMap<String, Vec<u8>>) -> bool {
    match sync {
        Some(sync) => hashes(current) != *sync.baseline(),
        None => ter_sdk::project::source_sha256(dir)
            .ok()
            .zip(TerToml::load(dir).ok())
            .is_some_and(|(now, ter)| now != ter.fetched_sha256),
    }
}

/// `path` as the learner knows it: under `~` when it is in their home.
pub fn display_path(path: &Path) -> String {
    let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rel) if rel.as_os_str().is_empty() => "~".into(),
        Some(rel) => format!("~/{}", rel.display()),
        None => path.display().to_string(),
    }
}

/// `/code`'s answer for the folder an exercise was fetched to, `None` when
/// it was not: the set as text, when it last changed, and whether it was
/// edited here since ter last wrote it.
pub fn code_answer(dir: Option<&Path>) -> Value {
    let Some(dir) = dir.filter(|d| d.is_dir()) else {
        return json!({"folder": "", "exists": false, "files": {},
                      "changed_at_ms": 0, "local_edit": false});
    };
    let sync = PageSync::load(dir);
    let paths = set_paths(dir, sync.as_ref());
    let current = read_set(dir, &paths);
    let files: serde_json::Map<String, Value> = current
        .iter()
        .filter_map(|(p, b)| Some((p.clone(), Value::from(std::str::from_utf8(b).ok()?))))
        .collect();
    json!({
        "folder": display_path(dir),
        "exists": true,
        "files": files,
        "changed_at_ms": changed_at_ms(dir, &paths),
        "local_edit": local_edit(dir, sync.as_ref(), &current),
    })
}

/// Copy the set in `dir` to a new `.ter/backup/<UTC stamp>/` when writing
/// `incoming` (path to content) would replace code edited here since ter
/// last wrote it, and keep the newest [`BACKUPS_KEPT`]. Returns the backup.
/// Nothing is copied when no file that would change was edited here.
pub fn backup_if_lost(
    dir: &Path,
    incoming: &BTreeMap<String, Vec<u8>>,
    now: SystemTime,
) -> Result<Option<PathBuf>, CliError> {
    let sync = PageSync::load(dir);
    let mut paths = set_paths(dir, sync.as_ref());
    for p in incoming.keys().filter(|p| in_set(p)) {
        if !paths.contains(p) {
            paths.push(p.clone());
        }
    }
    let current = read_set(dir, &paths);
    let lost = incoming.iter().any(|(p, content)| {
        let Some(on_disk) = current.get(p) else {
            return false;
        };
        if on_disk == content {
            return false;
        }
        match &sync {
            Some(sync) => sync.baseline().get(p) != Some(&hash(on_disk)),
            // No record of what ter wrote: anything not as fetched counts.
            None => local_edit(dir, None, &current),
        }
    });
    if !lost {
        return Ok(None);
    }
    let root = dir.join(STATE_DIR).join(BACKUP_DIR);
    let stamp = utc_stamp(now);
    let mut to = root.join(&stamp);
    let mut n = 2;
    while to.exists() {
        to = root.join(format!("{stamp}-{n}"));
        n += 1;
    }
    for (p, bytes) in &current {
        let path = to.join(p);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io("create", parent, e))?;
        }
        std::fs::write(&path, bytes).map_err(|e| io("write", &path, e))?;
    }
    prune_backups(&root);
    Ok(Some(to))
}

/// Delete all but the newest [`BACKUPS_KEPT`] backups. The names sort by
/// time.
fn prune_backups(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_by(|a, b| backup_order(a).cmp(&backup_order(b)));
    let extra = names.len().saturating_sub(BACKUPS_KEPT);
    for name in &names[..extra] {
        let _ = std::fs::remove_dir_all(root.join(name));
    }
}

/// A backup's name in time order: its stamp, then its number within the
/// second (none is the first).
fn backup_order(name: &str) -> (&str, u32) {
    match name.split_once('-') {
        Some((stamp, n)) => (stamp, n.parse().unwrap_or(0)),
        None => (name, 1),
    }
}

/// `20261125T215505Z`: a time to the second, in UTC, fit for a file name.
pub fn utc_stamp(t: SystemTime) -> String {
    ter_telemetry::rfc3339_utc(t).replace(['-', ':'], "")
}

/// After ter wrote code into the set (a page Run, a draft): that is what
/// it last wrote, and what the account holds now.
pub fn wrote(dir: &Path) -> Result<(), CliError> {
    let Some(mut sync) = PageSync::load(dir) else {
        return Ok(());
    };
    let current = read_set(dir, &sync.paths());
    sync.written = Some(hashes(&current));
    let (files, stamp) = draft_of(&sync, &current);
    sync.saved = Some(draft_hash(&files, &stamp));
    sync.save(dir)
}

/// The draft the set makes, by the lesson page's rules: the files that
/// differ from the starter, and the stamp of the starter they were written
/// over (the current one once nothing differs). A file that is not text is
/// left out.
fn draft_of(
    sync: &PageSync,
    current: &BTreeMap<String, Vec<u8>>,
) -> (BTreeMap<String, String>, String) {
    let files: BTreeMap<String, String> = current
        .iter()
        .filter(|(p, b)| sync.starter_files.get(*p) != Some(&hash(b)))
        .filter_map(|(p, b)| Some((p.clone(), String::from_utf8(b.clone()).ok()?)))
        .collect();
    let stamp = match &sync.draft_starter {
        Some(kept) if !files.is_empty() => kept.clone(),
        _ => sync.starter.clone(),
    };
    (files, stamp)
}

/// A draft as the lesson page would save it, for telling one unchanged.
pub fn draft_hash(files: &BTreeMap<String, String>, stamp: &str) -> String {
    let stamp = if files.is_empty() { "" } else { stamp };
    hash(json!([files, stamp]).to_string().as_bytes())
}

/// The set the account's draft makes over the starter: each file of the
/// starter's set, the draft's copy where it has one (as the lesson page
/// does, a draft file the starter does not have is left out).
pub fn with_draft(starter: &BTreeMap<String, Vec<u8>>, draft: &Draft) -> BTreeMap<String, Vec<u8>> {
    starter
        .iter()
        .map(|(p, b)| {
            let content = draft
                .files
                .get(p)
                .map_or_else(|| b.clone(), |d| d.as_bytes().to_vec());
            (p.clone(), content)
        })
        .collect()
}

/// Whether the set in `dir` is other than `files`.
pub fn differs(dir: &Path, files: &BTreeMap<String, Vec<u8>>) -> bool {
    let paths: Vec<String> = files.keys().cloned().collect();
    read_set(dir, &paths) != *files
}

/// Write `files` into the set in `dir`, those that differ, and record the
/// draft they came from. Returns the paths written.
pub fn put_draft(
    dir: &Path,
    files: &BTreeMap<String, Vec<u8>>,
    draft: &Draft,
) -> Result<Vec<String>, CliError> {
    let mut written = Vec::new();
    for (p, bytes) in files {
        if !in_set(p) {
            continue;
        }
        let path = dir.join(p);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        if std::fs::read(&path).ok().as_deref() == Some(bytes.as_slice()) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io("create", parent, e))?;
        }
        std::fs::write(&path, bytes).map_err(|e| io("write", &path, e))?;
        written.push(p.clone());
    }
    if let Some(mut sync) = PageSync::load(dir) {
        sync.draft_starter = draft
            .starter
            .clone()
            .filter(|s| !s.is_empty() && !draft.files.is_empty());
        sync.save(dir)?;
    }
    wrote(dir)?;
    Ok(written)
}

/// The account's draft of the lesson the folder `dir` is on, for `ter ex
/// fetch`. `None`, quietly, when there is none or the site does not hand
/// it to ter.
pub async fn account_draft(client: &Client, dir: &Path) -> Option<Draft> {
    let lesson = lesson(client, dir).await?;
    match client.exercise_draft(&lesson).await {
        Ok(draft) => draft,
        Err(e) => {
            debug(format!("no draft of {lesson}: {e}"));
            None
        }
    }
}

/// The lesson the folder's exercise is on: recorded at fetch, else asked
/// of the site (and recorded).
async fn lesson(client: &Client, dir: &Path) -> Option<String> {
    let mut sync = PageSync::load(dir)?;
    if let Some(lesson) = &sync.lesson {
        return Some(lesson.clone());
    }
    let ter = TerToml::load(dir).ok()?;
    let enrollments = match client.enrollments().await {
        Ok(e) => e,
        Err(e) => {
            debug(format!("no lesson for {}: {e}", ter.exercise));
            return None;
        }
    };
    let Some(lesson) = enrollments.lesson_of(&ter.exercise) else {
        debug(format!("{} is on no lesson of this account", ter.exercise));
        return None;
    };
    sync.lesson = Some(lesson.lesson_id.clone());
    let _ = sync.save(dir);
    sync.lesson
}

/// How long a terminal run waits at its end for the draft save.
const SAVE_WAIT: Duration = Duration::from_secs(10);

/// Save the set in `dir` (an exercise folder, or the one the current
/// folder is in) to the account's draft of its lesson, alongside a
/// terminal run. Wait for it with [`finish`].
pub fn save_in_background(dir: Option<PathBuf>) -> Option<tokio::task::JoinHandle<()>> {
    let dir = crate::run::exercise_dir(dir).ok()?;
    let session = Session::open().ok()?;
    Some(tokio::spawn(async move {
        save_draft(&session.client, &dir).await
    }))
}

/// Wait a little for a save from [`save_in_background`]; one that takes
/// longer is left.
pub async fn finish(save: Option<tokio::task::JoinHandle<()>>) {
    if let Some(save) = save
        && tokio::time::timeout(SAVE_WAIT, save).await.is_err()
    {
        debug("the draft save took too long; left");
    }
}

/// Save the set in `dir` to the account's draft, unless it is the draft
/// the account is known to hold. A failure is one warning line; no network,
/// or a site that does not take drafts from ter, says nothing.
pub async fn save_draft(client: &Client, dir: &Path) {
    let Ok(ter) = TerToml::load(dir) else {
        return;
    };
    if ter.dev {
        return;
    }
    let Some(sync) = PageSync::load(dir) else {
        debug(format!("{} has no {PAGE_SYNC} yet", dir.display()));
        return;
    };
    let paths = sync.paths();
    let current = read_set(dir, &paths);
    let (files, stamp) = draft_of(&sync, &current);
    let saving = draft_hash(&files, &stamp);
    if sync.saved.as_deref() == Some(saving.as_str()) {
        debug("the draft is unchanged since the last save");
        return;
    }
    // At the starter and never saved: nothing of the learner's to keep,
    // and an empty draft would replace the page's.
    if files.is_empty() && sync.saved.is_none() {
        return;
    }
    let Some(lesson) = lesson(client, dir).await else {
        return;
    };
    let at = changed_at_ms(dir, &paths);
    match client
        .save_exercise_draft(&lesson, &files, &stamp, at)
        .await
    {
        Ok(answer) if answer.saved => {
            let Some(mut sync) = PageSync::load(dir) else {
                return;
            };
            sync.saved = Some(saving);
            sync.draft_starter = (!files.is_empty()).then_some(stamp);
            let _ = sync.save(dir);
        }
        Ok(_) => debug("the account holds a newer draft; not saved"),
        Err(e) if quiet(&e) => debug(format!("draft not saved: {e}")),
        Err(e) => eprintln!("warning: Could not save your code to your account: {e}"),
    }
}

/// A failure to say nothing about: no network, or a site that does not
/// take drafts from ter (the method missing, or refused to a device
/// token).
fn quiet(e: &ter_sdk::Error) -> bool {
    match e {
        ter_sdk::Error::Network(_) | ter_sdk::Error::NoToken => true,
        ter_sdk::Error::Server { http_status, .. } => *http_status == 404,
        ter_sdk::Error::Site { code, message, .. } => {
            code == "not_on_site"
                || (code == "token_invalid" && message.contains("only works for the ter command"))
        }
        _ => false,
    }
}

/// A draft's time as the learner reads it.
pub fn when(at_ms: u64) -> String {
    let t = UNIX_EPOCH + Duration::from_millis(at_ms);
    let text = ter_telemetry::rfc3339_utc(t);
    // 2026-11-25T21:55:05Z -> 2026-11-25 21:55 UTC
    format!("{} {} UTC", &text[..10], &text[11..16])
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const MAIN: &str = "src/main.rs";
    const STARTER: &str = "fn main() {}\n";

    fn exercise() -> Exercise {
        serde_json::from_value(json!({
            "exercise_id": "blinky", "target": "esp32c3",
            "files": [
                {"path": MAIN, "content": STARTER},
                {"path": "src/lib.rs", "content": "\n"},
                {"path": "Cargo.toml", "content": "[package]\n"},
                {"path": "check.yaml", "content": "steps: []\n"},
            ],
        }))
        .unwrap()
    }

    /// A folder as `ter ex fetch` leaves it: the starter, ter.toml and the
    /// record, on lesson `lesson-1`.
    fn fetched() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for f in &exercise().files {
            let path = dir.join(&f.path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, &f.content).unwrap();
        }
        TerToml {
            exercise: "blinky".into(),
            course: "c".into(),
            target: "esp32c3".into(),
            modes: vec!["simulation".into()],
            mode: "simulation".into(),
            venue: None,
            fetched_sha256: ter_sdk::project::source_sha256(dir).unwrap(),
            pins: Default::default(),
            dev: false,
        }
        .save(dir)
        .unwrap();
        note_starter(dir, &exercise(), Some("lesson-1"), true).unwrap();
        tmp
    }

    fn edit(dir: &Path, text: &str) {
        std::fs::write(dir.join(MAIN), text).unwrap();
    }

    fn incoming(text: &str) -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([(MAIN.to_string(), text.as_bytes().to_vec())])
    }

    fn at(s: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_790_000_000 + s)
    }

    fn backups(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.join(STATE_DIR).join(BACKUP_DIR))
            .map(|r| {
                r.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn the_starter_stamp_is_the_lesson_pages() {
        // From liveRun.ts's starterStamp, run under node.
        assert_eq!(
            starter_stamp([
                ("src/main.rs", "fn main() {}\n"),
                ("Cargo.toml", "[package]\nname = \"\u{e9}\u{1f600}\"\n"),
            ]),
            "2-1c83a79d"
        );
        assert_eq!(starter_stamp([]), "0-811c9dc5");
    }

    #[test]
    fn a_fetch_records_the_starters_hashes_as_written() {
        let tmp = fetched();
        let sync = PageSync::load(tmp.path()).unwrap();
        assert_eq!(sync.lesson.as_deref(), Some("lesson-1"));
        assert_eq!(
            sync.starter_files.keys().collect::<Vec<_>>(),
            ["src/lib.rs", MAIN]
        );
        assert_eq!(sync.starter_files[MAIN], hash(STARTER.as_bytes()));
        assert_eq!(sync.written.as_ref(), Some(&sync.starter_files));
        assert_eq!(
            sync.starter,
            starter_stamp([
                (MAIN, STARTER),
                ("src/lib.rs", "\n"),
                ("Cargo.toml", "[package]\n")
            ])
        );
        // A later refresh keeps what ter wrote.
        edit(tmp.path(), "page\n");
        wrote(tmp.path()).unwrap();
        note_starter(tmp.path(), &exercise(), None, false).unwrap();
        let sync = PageSync::load(tmp.path()).unwrap();
        assert_eq!(sync.written.unwrap()[MAIN], hash(b"page\n"));
        assert_eq!(sync.lesson.as_deref(), Some("lesson-1"));
    }

    #[test]
    fn code_says_whether_the_folder_was_edited_since_ter_wrote_it() {
        let tmp = fetched();
        let v = code_answer(Some(tmp.path()));
        assert_eq!(v["exists"], true);
        assert_eq!(v["local_edit"], false);
        assert_eq!(v["files"], json!({MAIN: STARTER, "src/lib.rs": "\n"}));
        assert!(v["changed_at_ms"].as_u64().unwrap() > 0);
        edit(tmp.path(), "mine\n");
        let v = code_answer(Some(tmp.path()));
        assert_eq!(v["local_edit"], true);
        assert_eq!(v["files"][MAIN], "mine\n");
        // What a page Run wrote is ter's.
        wrote(tmp.path()).unwrap();
        assert_eq!(code_answer(Some(tmp.path()))["local_edit"], false);

        let gone = tmp.path().join("nothing-here");
        for dir in [None, Some(gone.as_path())] {
            assert_eq!(
                code_answer(dir),
                json!({"folder": "", "exists": false, "files": {},
                       "changed_at_ms": 0, "local_edit": false})
            );
        }
    }

    #[test]
    fn a_backup_only_when_code_edited_here_would_be_lost() {
        let tmp = fetched();
        let dir = tmp.path();
        // Nothing edited here: the page's code replaces ter's own.
        assert_eq!(
            backup_if_lost(dir, &incoming("page\n"), at(0)).unwrap(),
            None
        );
        // Edited here, and the page sends the same code: nothing lost.
        edit(dir, "mine\n");
        assert_eq!(
            backup_if_lost(dir, &incoming("mine\n"), at(0)).unwrap(),
            None
        );
        // Edited here, and the page sends other code: kept.
        let backup = backup_if_lost(dir, &incoming("page\n"), at(0))
            .unwrap()
            .expect("a backup");
        assert_eq!(backup, dir.join(".ter/backup").join(utc_stamp(at(0))));
        assert_eq!(
            std::fs::read_to_string(backup.join(MAIN)).unwrap(),
            "mine\n"
        );
        assert_eq!(
            std::fs::read_to_string(backup.join("src/lib.rs")).unwrap(),
            "\n"
        );
        // Once the page's code is written, it is ter's.
        edit(dir, "page\n");
        wrote(dir).unwrap();
        assert_eq!(
            backup_if_lost(dir, &incoming("other\n"), at(1)).unwrap(),
            None
        );
        assert_eq!(backups(dir).len(), 1);
    }

    #[test]
    fn the_newest_ten_backups_are_kept() {
        let tmp = fetched();
        let dir = tmp.path();
        edit(dir, "mine\n");
        for s in 0..12 {
            assert!(
                backup_if_lost(dir, &incoming("page\n"), at(s))
                    .unwrap()
                    .is_some()
            );
        }
        // Two in one second.
        let twin = backup_if_lost(dir, &incoming("page\n"), at(11))
            .unwrap()
            .unwrap();
        assert_eq!(
            twin.file_name().unwrap().to_string_lossy(),
            format!("{}-2", utc_stamp(at(11)))
        );
        let kept = backups(dir);
        assert_eq!(kept.len(), BACKUPS_KEPT);
        assert_eq!(kept[0], utc_stamp(at(3)));
        assert!(kept.contains(&format!("{}-2", utc_stamp(at(11)))));
    }

    #[test]
    fn a_page_draft_goes_over_the_starter() {
        let tmp = fetched();
        let dir = tmp.path();
        let draft = Draft {
            at: 5,
            starter: Some("2-old".into()),
            files: BTreeMap::from([
                (MAIN.to_string(), "draft\n".to_string()),
                ("src/gone.rs".to_string(), "x".to_string()),
            ]),
        };
        let code = with_draft(&starter_set(&exercise()), &draft);
        assert_eq!(code.keys().collect::<Vec<_>>(), ["src/lib.rs", MAIN]);
        assert!(differs(dir, &code));
        assert_eq!(put_draft(dir, &code, &draft).unwrap(), [MAIN]);
        assert_eq!(std::fs::read_to_string(dir.join(MAIN)).unwrap(), "draft\n");
        assert!(!dir.join("src/gone.rs").exists());
        assert!(!differs(dir, &code));
        let sync = PageSync::load(dir).unwrap();
        // The draft goes on with the starter it was written over.
        assert_eq!(sync.draft_starter.as_deref(), Some("2-old"));
        let (files, stamp) = draft_of(&sync, &read_set(dir, &sync.paths()));
        assert_eq!(stamp, "2-old");
        assert_eq!(sync.saved, Some(draft_hash(&files, &stamp)));
        assert_eq!(code_answer(Some(dir))["local_edit"], false);
    }

    async fn site() -> (MockServer, Client) {
        let server = MockServer::start().await;
        let client = Client::new(&server.uri(), Some("good-token".into()), "0.1.10").unwrap();
        (server, client)
    }

    fn save_path() -> wiremock::matchers::PathExactMatcher {
        path(format!(
            "/api/method/{}",
            "ter_courses.exercise_drafts.save_exercise_draft"
        ))
    }

    #[tokio::test]
    async fn a_run_saves_the_draft_once_and_not_again_unchanged() {
        let (server, client) = site().await;
        Mock::given(method("POST"))
            .and(save_path())
            .and(body_partial_json(json!({
                "lesson": "lesson-1", "files": {MAIN: "mine\n"},
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"message": {"saved": true, "at": 1}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let tmp = fetched();
        let dir = tmp.path();
        // At the starter and never saved: nothing to save.
        save_draft(&client, dir).await;
        edit(dir, "mine\n");
        save_draft(&client, dir).await;
        // Unchanged since: not sent again.
        save_draft(&client, dir).await;
        let sync = PageSync::load(dir).unwrap();
        assert_eq!(sync.draft_starter.as_deref(), Some(sync.starter.as_str()));
        let sent = &server.received_requests().await.unwrap()[0];
        let body: Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(body["starter"], sync.starter);
        assert_eq!(body["at"], changed_at_ms(dir, &sync.paths()));
    }

    #[tokio::test]
    async fn back_at_the_starter_saves_an_empty_draft() {
        let (server, client) = site().await;
        Mock::given(method("POST"))
            .and(save_path())
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"message": {"saved": true, "at": 1}})),
            )
            .expect(2)
            .mount(&server)
            .await;
        let tmp = fetched();
        let dir = tmp.path();
        edit(dir, "mine\n");
        save_draft(&client, dir).await;
        edit(dir, STARTER);
        save_draft(&client, dir).await;
        let sent = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&sent[1].body).unwrap();
        assert_eq!(body["files"], json!({}));
        assert_eq!(PageSync::load(dir).unwrap().draft_starter, None);
    }

    #[tokio::test]
    async fn a_save_the_site_refuses_is_not_recorded() {
        let (server, client) = site().await;
        Mock::given(method("POST"))
            .and(save_path())
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "exc_type": "AuthenticationError",
                "message": "This token only works for the ter command.",
            })))
            .mount(&server)
            .await;
        let tmp = fetched();
        edit(tmp.path(), "mine\n");
        save_draft(&client, tmp.path()).await;
        assert_eq!(PageSync::load(tmp.path()).unwrap().saved, None);
    }

    #[tokio::test]
    async fn the_lesson_comes_from_the_site_when_not_recorded() {
        let (server, client) = site().await;
        Mock::given(method("GET"))
            .and(path("/api/method/ter_courses.api.enrollments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"message": {
                "courses": [{"course_id": "c", "title": "C", "lessons": [
                    {"lesson_id": "lesson-9", "title": "L",
                     "exercises": [{"exercise_id": "blinky", "title": "B"}]}]}]}})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/method/{}",
                "ter_courses.exercise_drafts.exercise_draft"
            )))
            .and(query_param("lesson", "lesson-9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"message": {
                "draft": {"at": 7, "starter": "1-a", "files": {MAIN: "d"}}}})))
            .mount(&server)
            .await;
        let tmp = fetched();
        let mut sync = PageSync::load(tmp.path()).unwrap();
        sync.lesson = None;
        sync.save(tmp.path()).unwrap();
        let draft = account_draft(&client, tmp.path()).await.unwrap();
        assert_eq!((draft.at, draft.files[MAIN].as_str()), (7, "d"));
        assert_eq!(
            PageSync::load(tmp.path()).unwrap().lesson.as_deref(),
            Some("lesson-9")
        );
    }
}
