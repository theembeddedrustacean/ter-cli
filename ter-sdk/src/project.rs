//! Exercises on the learner's disk.
//!
//! One folder per course under the courses root, one Cargo project per
//! exercise inside it, and a `ter.toml` in each exercise saying what it is:
//!
//! ```text
//! <courses-root>/<course>/
//!   .target/            shared build output for the course (CARGO_TARGET_DIR)
//!   .embuild/           shared ESP-IDF tools for the course
//!   <exercise>/
//!     ter.toml
//!     Cargo.toml, src/, check.yaml, ...   as fetched
//!     .runs/            one recording per run
//! ```

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::exercise::Exercise;

pub const TER_TOML: &str = "ter.toml";
/// Recordings of runs, inside an exercise.
pub const RUNS_DIR: &str = ".runs";
/// Shared build output, inside a course.
pub const COURSE_TARGET_DIR: &str = ".target";
/// Shared ESP-IDF tools, inside a course.
pub const COURSE_EMBUILD_DIR: &str = ".embuild";
/// What `ter` remembers about an exercise, such as its last posted run.
/// Not build output: `ter ex clean` keeps it.
pub const STATE_DIR: &str = ".ter";

/// Build output inside an exercise: cargo's own `target/` when something
/// builds it without `ter`, and the run recordings.
const EXERCISE_BUILD_OUTPUT: [&str; 2] = ["target", RUNS_DIR];

/// Top-level names left out of the edit check: ter's own files, build
/// output, and the lock file cargo writes on the first build.
const NOT_SOURCE: [&str; 5] = [TER_TOML, STATE_DIR, "target", RUNS_DIR, "Cargo.lock"];

/// A failure on the learner's disk, with the stable code `ter` exits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectError {
    pub code: &'static str,
    pub message: String,
}

impl ProjectError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn io(what: &str, path: &Path, e: std::io::Error) -> Self {
        Self::new(
            "io_error",
            format!("Could not {what} {}: {e}", path.display()),
        )
    }

    /// The scaffold the site sent cannot be used as it is. The fix belongs
    /// in the curriculum, not on the learner's machine.
    fn authoring(exercise_id: &str, problem: impl fmt::Display) -> Self {
        Self::new(
            "bad_scaffold",
            format!(
                "Exercise {exercise_id} cannot be fetched: {problem}. This is a problem with the exercise, not your setup; please report it."
            ),
        )
    }
}

impl fmt::Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProjectError {}

type Result<T> = std::result::Result<T, ProjectError>;

/// `ter.toml`: what an exercise folder holds and how a bare `ter run` runs
/// it. The learner may edit `mode` and `venue`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerToml {
    pub exercise: String,
    pub course: String,
    pub target: String,
    pub modes: Vec<String>,
    /// What bare `ter run` uses: one of `modes`.
    pub mode: String,
    /// Where it runs within the mode; the mode's default when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venue: Option<String>,
    /// Of the source as fetched, to tell an edited exercise from a fresh one.
    pub fetched_sha256: String,
    /// The target's pin names and their GPIOs, from the site at fetch.
    /// Checks name pins; a simulator wires GPIOs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pins: BTreeMap<String, String>,
}

const TER_TOML_HEADER: &str = "\
# Written by `ter ex fetch`. `mode` is what a bare `ter run` uses, one of
# `modes`; `--sim` and `--hw` override it for one run. `venue` is optional.
";

impl TerToml {
    pub fn load(exercise_dir: &Path) -> Result<Self> {
        let path = exercise_dir.join(TER_TOML);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProjectError::new(
                    "not_an_exercise",
                    format!(
                        "{} is not an exercise folder (no {TER_TOML}).",
                        exercise_dir.display()
                    ),
                )
            } else {
                ProjectError::io("read", &path, e)
            }
        })?;
        toml::from_str(&text).map_err(|e| {
            ProjectError::new(
                "config_error",
                format!("{} is not valid: {}", path.display(), e.message()),
            )
        })
    }

    pub fn save(&self, exercise_dir: &Path) -> Result<()> {
        let path = exercise_dir.join(TER_TOML);
        let body = toml::to_string(self).expect("ter.toml serialises");
        std::fs::write(&path, format!("{TER_TOML_HEADER}{body}"))
            .map_err(|e| ProjectError::io("write", &path, e))
    }
}

/// Where courses and exercises go under the courses root.
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn course_dir(&self, course: &str) -> PathBuf {
        self.root.join(course)
    }

    pub fn exercise_dir(&self, course: &str, exercise: &str) -> PathBuf {
        self.course_dir(course).join(exercise)
    }

    /// Every fetched exercise under the root, by course then exercise.
    pub fn exercises(&self) -> Result<Vec<Fetched>> {
        let mut found = Vec::new();
        for course in subdirs(&self.root)? {
            for dir in subdirs(&course)? {
                if let Ok(ter) = TerToml::load(&dir) {
                    found.push(Fetched { dir, ter });
                }
            }
        }
        Ok(found)
    }

    /// Course folders under the root: those holding an exercise or a
    /// shared cache.
    pub fn course_dirs(&self) -> Result<Vec<PathBuf>> {
        let mut courses = Vec::new();
        for dir in subdirs(&self.root)? {
            let has_cache = [COURSE_TARGET_DIR, COURSE_EMBUILD_DIR]
                .iter()
                .any(|c| dir.join(c).exists());
            if has_cache || subdirs(&dir)?.iter().any(|d| d.join(TER_TOML).is_file()) {
                courses.push(dir);
            }
        }
        Ok(courses)
    }

    /// The folder of a fetched exercise, by id.
    pub fn find(&self, exercise: &str) -> Result<Fetched> {
        self.exercises()?
            .into_iter()
            .find(|f| f.ter.exercise == exercise)
            .ok_or_else(|| {
                ProjectError::new(
                    "not_fetched",
                    format!(
                        "Exercise {exercise} is not fetched under {}. Run `ter ex fetch {exercise}`.",
                        self.root.display()
                    ),
                )
            })
    }

    /// The environment `ter` builds an exercise with, so every exercise in
    /// a course shares one build cache and, on ESP-IDF, one set of tools.
    /// The cache is always the course folder under the root, wherever the
    /// exercise itself was fetched to.
    pub fn cargo_env(&self, ter: &TerToml, exercise_dir: &Path) -> Vec<(&'static str, OsString)> {
        let course = self.course_dir(&ter.course);
        let mut idf_tools = OsString::from("custom:");
        idf_tools.push(course.join(COURSE_EMBUILD_DIR));
        vec![
            ("CARGO_TARGET_DIR", course.join(COURSE_TARGET_DIR).into()),
            ("ESP_IDF_TOOLS_INSTALL_DIR", idf_tools),
            // embuild finds the project from here once the target dir has
            // moved out of it.
            ("CARGO_WORKSPACE_DIR", exercise_dir.into()),
        ]
    }
}

/// An exercise folder on disk and its `ter.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub dir: PathBuf,
    pub ter: TerToml,
}

impl Fetched {
    /// Whether the source differs from what was fetched: a file changed,
    /// added or deleted. Build output and `ter.toml` do not count.
    pub fn is_edited(&self) -> Result<bool> {
        Ok(source_sha256(&self.dir)? != self.ter.fetched_sha256)
    }
}

/// The folder an exercise command acts on when none is named: the nearest
/// one at or above `start` with a `ter.toml`.
pub fn enclosing_exercise(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|d| d.join(TER_TOML).is_file())
        .map(Path::to_path_buf)
}

/// A scaffold checked and decoded, ready to write.
#[derive(Debug)]
pub struct Scaffold {
    pub exercise_id: String,
    pub target: String,
    pub files: Vec<(PathBuf, Vec<u8>)>,
    /// The `runner` its `.cargo/config.toml` sets.
    pub runner: String,
    pub pins: BTreeMap<String, String>,
}

impl Scaffold {
    /// Refuse a scaffold `ter` cannot write safely, or one with no cargo
    /// runner: free play is plain `cargo run`, so every scaffold must set
    /// one, and `ter` never edits the files it fetched.
    pub fn check(ex: &Exercise) -> Result<Self> {
        let id = &ex.exercise_id;
        let mut files = Vec::with_capacity(ex.files.len());
        let mut runner = None;
        for f in &ex.files {
            let path = safe_relative(&f.path)
                .ok_or_else(|| ProjectError::authoring(id, format!("unsafe path {:?}", f.path)))?;
            if NOT_SOURCE.contains(&top_level(&path)) && path != Path::new("Cargo.lock") {
                return Err(ProjectError::authoring(
                    id,
                    format!("{:?} is a name ter keeps for itself", f.path),
                ));
            }
            let bytes = match f.encoding.as_str() {
                "utf8" | "" => f.content.clone().into_bytes(),
                "base64" => base64::engine::general_purpose::STANDARD
                    .decode(&f.content)
                    .map_err(|e| ProjectError::authoring(id, format!("{}: {e}", f.path)))?,
                other => {
                    return Err(ProjectError::authoring(
                        id,
                        format!("{}: unknown encoding {other:?}", f.path),
                    ));
                }
            };
            if path == Path::new(".cargo/config.toml") {
                runner = find_runner(&String::from_utf8_lossy(&bytes));
            }
            if files.iter().any(|(p, _)| *p == path) {
                return Err(ProjectError::authoring(id, format!("{} twice", f.path)));
            }
            files.push((path, bytes));
        }
        let runner = runner.ok_or_else(|| {
            ProjectError::authoring(id, "its .cargo/config.toml sets no runner for `cargo run`")
        })?;
        Ok(Self {
            exercise_id: id.clone(),
            target: ex.target.clone(),
            files,
            runner,
            pins: ex.pins.clone(),
        })
    }

    /// Write the scaffold and its `ter.toml` to `dest`, which must not
    /// exist yet (or be an empty folder). All or nothing: it is written
    /// next to `dest` and moved into place at the end.
    pub fn write(&self, dest: &Path, course: &str, modes: Vec<String>) -> Result<Fetched> {
        if modes.is_empty() {
            return Err(ProjectError::authoring(
                &self.exercise_id,
                "it allows neither hardware nor simulation",
            ));
        }
        if let Ok(ter) = TerToml::load(dest) {
            return Err(ProjectError::new(
                "already_fetched",
                format!(
                    "{} already holds exercise {}. `ter ex remove {}` first to fetch it again.",
                    dest.display(),
                    ter.exercise,
                    ter.exercise
                ),
            ));
        }
        if dest.exists() && !is_empty_dir(dest) {
            return Err(ProjectError::new(
                "dir_not_empty",
                format!("{} exists and is not empty.", dest.display()),
            ));
        }
        let parent = dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| ProjectError::io("create", parent, e))?;
        let name = dest.file_name().unwrap_or_default().to_string_lossy();
        let staging = parent.join(format!(".{name}.fetching"));
        let _ = std::fs::remove_dir_all(&staging);

        let written = self.write_into(&staging, course, modes).and_then(|ter| {
            if dest.exists() {
                std::fs::remove_dir(dest).map_err(|e| ProjectError::io("replace", dest, e))?;
            }
            std::fs::rename(&staging, dest).map_err(|e| ProjectError::io("create", dest, e))?;
            Ok(ter)
        });
        if written.is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        Ok(Fetched {
            dir: dest.to_path_buf(),
            ter: written?,
        })
    }

    fn write_into(&self, dir: &Path, course: &str, modes: Vec<String>) -> Result<TerToml> {
        for (rel, bytes) in &self.files {
            let path = dir.join(rel);
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p).map_err(|e| ProjectError::io("create", p, e))?;
            }
            std::fs::write(&path, bytes).map_err(|e| ProjectError::io("write", &path, e))?;
        }
        let ter = TerToml {
            exercise: self.exercise_id.clone(),
            course: course.to_string(),
            target: self.target.clone(),
            mode: modes[0].clone(),
            modes,
            venue: None,
            fetched_sha256: source_sha256(dir)?,
            pins: self.pins.clone(),
        };
        ter.save(dir)?;
        Ok(ter)
    }
}

/// The `runner` set for any target in a `.cargo/config.toml`.
pub fn find_runner(config_toml: &str) -> Option<String> {
    let config: toml::Table = toml::from_str(config_toml).ok()?;
    config
        .get("target")?
        .as_table()?
        .values()
        .filter_map(|t| t.get("runner"))
        .find_map(|r| match r {
            toml::Value::String(s) => Some(s.trim().to_string()),
            toml::Value::Array(parts) => Some(
                parts
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        })
        .filter(|r| !r.is_empty())
}

/// sha256 of an exercise's source: every file under `dir` except
/// `ter.toml`, `Cargo.lock` and build output, by path in a fixed order.
pub fn source_sha256(dir: &Path) -> Result<String> {
    let mut files = Vec::new();
    collect_files(dir, dir, &mut files)?;
    files.sort();
    let mut hash = Sha256::new();
    for rel in files {
        let path = dir.join(&rel);
        let bytes = std::fs::read(&path).map_err(|e| ProjectError::io("read", &path, e))?;
        let name = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| ProjectError::io("read", dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| ProjectError::io("read", dir, e))?;
        let path = entry.path();
        let rel = path.strip_prefix(root).expect("under root").to_path_buf();
        if dir == root && NOT_SOURCE.contains(&top_level(&rel)) {
            continue;
        }
        let kind = entry
            .file_type()
            .map_err(|e| ProjectError::io("read", &path, e))?;
        if kind.is_dir() {
            collect_files(root, &path, out)?;
        } else {
            out.push(rel);
        }
    }
    Ok(())
}

/// A new, empty recording folder for a run: `.runs/<n>/` in the exercise,
/// numbered on from the highest there. Returns the number and the folder.
pub fn new_recording(exercise_dir: &Path) -> Result<(u32, PathBuf)> {
    let runs = exercise_dir.join(RUNS_DIR);
    std::fs::create_dir_all(&runs).map_err(|e| ProjectError::io("create", &runs, e))?;
    let mut n = subdirs(&runs)?
        .iter()
        .filter_map(|d| d.file_name()?.to_str()?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    loop {
        n += 1;
        let dir = runs.join(n.to_string());
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok((n, dir)),
            // Another run took this number first.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(ProjectError::io("create", &dir, e)),
        }
    }
}

/// What cleaning freed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Freed {
    pub bytes: u64,
    pub dirs: u32,
}

impl std::ops::AddAssign for Freed {
    fn add_assign(&mut self, other: Self) {
        self.bytes += other.bytes;
        self.dirs += other.dirs;
    }
}

/// Drop an exercise's own build output and recordings; the source stays.
pub fn clean_exercise(exercise_dir: &Path) -> Result<Freed> {
    let mut freed = Freed::default();
    for name in EXERCISE_BUILD_OUTPUT {
        freed += remove_tree(&exercise_dir.join(name))?;
    }
    Ok(freed)
}

/// Drop a course's shared build cache and every exercise's build output.
pub fn clean_course(course_dir: &Path) -> Result<Freed> {
    let mut freed = Freed::default();
    for name in [COURSE_TARGET_DIR, COURSE_EMBUILD_DIR] {
        freed += remove_tree(&course_dir.join(name))?;
    }
    for dir in subdirs(course_dir)? {
        if dir.join(TER_TOML).is_file() {
            freed += clean_exercise(&dir)?;
        }
    }
    Ok(freed)
}

/// Delete an exercise folder. Refused with `source_edited` when the source
/// differs from what was fetched, unless `force`.
pub fn remove_exercise(fetched: &Fetched, force: bool) -> Result<Freed> {
    if !force && fetched.is_edited()? {
        return Err(ProjectError::new(
            "source_edited",
            format!(
                "{} has changes since it was fetched. `ter ex remove {} --force` deletes them too.",
                fetched.dir.display(),
                fetched.ter.exercise
            ),
        ));
    }
    remove_tree(&fetched.dir)
}

/// Bytes on disk under `path`; 0 when it does not exist.
pub fn disk_usage(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| disk_usage(&e.path()))
                .sum()
        })
        .unwrap_or(0)
}

fn remove_tree(path: &Path) -> Result<Freed> {
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(Freed::default());
    }
    let bytes = disk_usage(path);
    std::fs::remove_dir_all(path).map_err(|e| ProjectError::io("remove", path, e))?;
    Ok(Freed { bytes, dirs: 1 })
}

fn subdirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(ProjectError::io("read", dir, e)),
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    Ok(dirs)
}

fn is_empty_dir(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut d| d.next().is_none())
}

/// A path from the site as a relative path with only normal parts, or
/// `None` for anything that could land outside the exercise folder.
fn safe_relative(path: &str) -> Option<PathBuf> {
    if path.is_empty() || path.contains('\\') {
        return None;
    }
    let p = Path::new(path);
    p.components()
        .all(|c| matches!(c, Component::Normal(_)))
        .then(|| p.to_path_buf())
}

fn top_level(rel: &Path) -> &str {
    rel.components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exercise::{ExerciseFile, default_modes};

    const CONFIG: &str = "[target.riscv32imc-unknown-none-elf]\nrunner = \"espflash flash --monitor --chip esp32c3\"\n";

    fn file(path: &str, content: &str) -> ExerciseFile {
        ExerciseFile {
            path: path.into(),
            content: content.into(),
            encoding: "utf8".into(),
        }
    }

    fn exercise(files: Vec<ExerciseFile>) -> Exercise {
        Exercise {
            exercise_id: "gpio-blinky--xiao-esp32c3-nostd".into(),
            title: "Heartbeat".into(),
            target: "xiao-esp32c3-nostd".into(),
            runner: "cargo".into(),
            kind: "exercise".into(),
            runner_args: serde_json::json!({}),
            timeout_seconds: 180,
            toolchain: serde_json::json!({}),
            instructions_md: String::new(),
            files,
            modes: None,
            pins: BTreeMap::from([("user_led".into(), "GPIO3".into())]),
        }
    }

    fn scaffold() -> Exercise {
        exercise(vec![
            file(".cargo/config.toml", CONFIG),
            file("Cargo.toml", "[package]\nname = \"exercise\"\n"),
            file("src/bin/main.rs", "fn main() {}\n"),
            file("check.yaml", "checks: []\n"),
        ])
    }

    fn fetch(layout: &Layout, ex: &Exercise) -> Fetched {
        let dest = layout.exercise_dir("esp-gpio", &ex.exercise_id);
        Scaffold::check(ex)
            .unwrap()
            .write(&dest, "esp-gpio", default_modes())
            .unwrap()
    }

    #[test]
    fn fetch_lays_out_course_and_exercise() {
        let root = tempfile::tempdir().unwrap();
        let layout = Layout::new(root.path());
        let f = fetch(&layout, &scaffold());
        let dir = root.path().join("esp-gpio/gpio-blinky--xiao-esp32c3-nostd");
        assert_eq!(f.dir, dir);
        assert_eq!(
            std::fs::read_to_string(dir.join("src/bin/main.rs")).unwrap(),
            "fn main() {}\n"
        );
        assert!(dir.join("check.yaml").is_file());
        assert_eq!(f.ter.course, "esp-gpio");
        assert_eq!(f.ter.target, "xiao-esp32c3-nostd");
        assert_eq!(f.ter.modes, ["hardware", "simulation"]);
        assert_eq!(f.ter.mode, "hardware");
        assert_eq!(f.ter.pins["user_led"], "GPIO3", "the pin map is kept");
        assert!(!f.is_edited().unwrap());
        // No staging folder is left behind.
        let names: Vec<_> = std::fs::read_dir(root.path().join("esp-gpio"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["gpio-blinky--xiao-esp32c3-nostd"]);
        assert_eq!(layout.find(&f.ter.exercise).unwrap(), f);
    }

    #[test]
    fn modes_from_the_site_set_the_default_mode() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("ex");
        let f = Scaffold::check(&scaffold())
            .unwrap()
            .write(&dest, "c", vec!["simulation".into()])
            .unwrap();
        assert_eq!(f.ter.modes, ["simulation"]);
        assert_eq!(f.ter.mode, "simulation");
    }

    #[test]
    fn an_exercise_that_allows_no_mode_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("ex");
        let err = Scaffold::check(&scaffold())
            .unwrap()
            .write(&dest, "c", vec![])
            .unwrap_err();
        assert_eq!(err.code, "bad_scaffold");
        assert!(!dest.exists());
    }

    #[test]
    fn ter_toml_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let ter = TerToml {
            exercise: "gpio-button-led".into(),
            course: "esp-nostd-gpio".into(),
            target: "xiao-esp32c3".into(),
            modes: vec!["hardware".into(), "simulation".into()],
            mode: "simulation".into(),
            venue: Some("local".into()),
            fetched_sha256: "ab".repeat(32),
            pins: BTreeMap::from([
                ("user_button".into(), "GPIO5".into()),
                ("user_led".into(), "GPIO3".into()),
            ]),
        };
        ter.save(dir.path()).unwrap();
        assert_eq!(TerToml::load(dir.path()).unwrap(), ter);
        let text = std::fs::read_to_string(dir.path().join(TER_TOML)).unwrap();
        assert!(text.starts_with("# Written by `ter ex fetch`."), "{text}");
        assert!(text.contains("exercise = \"gpio-button-led\""), "{text}");
        assert!(text.contains("[pins]\nuser_button = \"GPIO5\""), "{text}");

        let no_venue = TerToml { venue: None, ..ter };
        no_venue.save(dir.path()).unwrap();
        let text = std::fs::read_to_string(dir.path().join(TER_TOML)).unwrap();
        assert!(!text.contains("venue ="), "{text}");
        assert_eq!(TerToml::load(dir.path()).unwrap(), no_venue);
    }

    #[test]
    fn a_learner_edit_to_mode_survives_a_load() {
        let root = tempfile::tempdir().unwrap();
        let f = fetch(&Layout::new(root.path()), &scaffold());
        let path = f.dir.join(TER_TOML);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            text.replace("mode = \"hardware\"", "mode = \"simulation\""),
        )
        .unwrap();
        assert_eq!(TerToml::load(&f.dir).unwrap().mode, "simulation");
        assert!(!f.is_edited().unwrap(), "ter.toml is not source");
    }

    #[test]
    fn edits_are_detected_and_build_output_is_not_an_edit() {
        let root = tempfile::tempdir().unwrap();
        let f = fetch(&Layout::new(root.path()), &scaffold());

        std::fs::create_dir_all(f.dir.join("target/debug")).unwrap();
        std::fs::write(f.dir.join("target/debug/exercise"), "elf").unwrap();
        std::fs::create_dir_all(f.dir.join(".runs/1")).unwrap();
        std::fs::write(f.dir.join(".runs/1/build.log"), "ok").unwrap();
        std::fs::write(f.dir.join("Cargo.lock"), "# lock").unwrap();
        assert!(!f.is_edited().unwrap());

        let main = f.dir.join("src/bin/main.rs");
        std::fs::write(&main, "fn main() { loop {} }\n").unwrap();
        assert!(f.is_edited().unwrap(), "changed file");
        std::fs::write(&main, "fn main() {}\n").unwrap();
        assert!(!f.is_edited().unwrap(), "changed back");

        std::fs::write(f.dir.join("src/extra.rs"), "").unwrap();
        assert!(f.is_edited().unwrap(), "added file");
        std::fs::remove_file(f.dir.join("src/extra.rs")).unwrap();

        std::fs::remove_file(f.dir.join("check.yaml")).unwrap();
        assert!(f.is_edited().unwrap(), "deleted file");
    }

    #[test]
    fn a_renamed_file_is_an_edit() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("x.rs"), "same").unwrap();
        std::fs::write(b.path().join("y.rs"), "same").unwrap();
        assert_ne!(
            source_sha256(a.path()).unwrap(),
            source_sha256(b.path()).unwrap()
        );
    }

    #[test]
    fn a_scaffold_with_no_runner_is_refused_and_nothing_is_written() {
        let root = tempfile::tempdir().unwrap();
        for config in [
            None,
            Some("[build]\ntarget = \"riscv32imc-unknown-none-elf\"\n"),
            Some("[target.riscv32imc-unknown-none-elf]\nrunner = \"  \"\n"),
        ] {
            let mut files = vec![file("Cargo.toml", "[package]\n")];
            if let Some(c) = config {
                files.push(file(".cargo/config.toml", c));
            }
            let err = Scaffold::check(&exercise(files)).unwrap_err();
            assert_eq!(err.code, "bad_scaffold");
            assert!(err.message.contains("no runner"), "{}", err.message);
        }
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn runner_is_found_for_any_target_form() {
        assert_eq!(
            find_runner(CONFIG).as_deref(),
            Some("espflash flash --monitor --chip esp32c3")
        );
        let std_config = "[target.'cfg(target_os = \"espidf\")']\nlinker = \"ldproxy\"\nrunner = \"espflash flash --monitor\"\n";
        assert_eq!(
            find_runner(std_config).as_deref(),
            Some("espflash flash --monitor")
        );
        let array = "[target.thumbv7em-none-eabihf]\nrunner = [\"probe-rs\", \"run\", \"--chip\", \"STM32F401RETx\"]\n";
        assert_eq!(
            find_runner(array).as_deref(),
            Some("probe-rs run --chip STM32F401RETx")
        );
        assert_eq!(find_runner("not toml ["), None);
    }

    #[test]
    fn unsafe_paths_are_refused() {
        for bad in [
            "../escape.rs",
            "/etc/passwd",
            "src/../../x",
            "",
            "a\\b",
            "./x",
        ] {
            let ex = exercise(vec![file(".cargo/config.toml", CONFIG), file(bad, "x")]);
            let err = Scaffold::check(&ex).unwrap_err();
            assert_eq!(err.code, "bad_scaffold", "{bad}");
        }
        for reserved in ["ter.toml", ".runs/1/x", "target/x", ".ter/last-run.json"] {
            let ex = exercise(vec![
                file(".cargo/config.toml", CONFIG),
                file(reserved, "x"),
            ]);
            assert_eq!(Scaffold::check(&ex).unwrap_err().code, "bad_scaffold");
        }
    }

    #[test]
    fn base64_files_are_decoded() {
        let mut ex = scaffold();
        ex.files.push(ExerciseFile {
            path: "blob.bin".into(),
            content: "AAEC/w==".into(),
            encoding: "base64".into(),
        });
        let root = tempfile::tempdir().unwrap();
        let f = fetch(&Layout::new(root.path()), &ex);
        assert_eq!(
            std::fs::read(f.dir.join("blob.bin")).unwrap(),
            [0, 1, 2, 255]
        );
    }

    #[test]
    fn fetching_over_an_exercise_or_a_non_empty_folder_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let layout = Layout::new(root.path());
        let f = fetch(&layout, &scaffold());
        let s = Scaffold::check(&scaffold()).unwrap();
        assert_eq!(
            s.write(&f.dir, "esp-gpio", default_modes())
                .unwrap_err()
                .code,
            "already_fetched"
        );

        let other = root.path().join("mine");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("notes.txt"), "keep me").unwrap();
        assert_eq!(
            s.write(&other, "c", default_modes()).unwrap_err().code,
            "dir_not_empty"
        );
        assert_eq!(
            std::fs::read_to_string(other.join("notes.txt")).unwrap(),
            "keep me"
        );

        let empty = root.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        s.write(&empty, "c", default_modes()).unwrap();
        assert!(empty.join(TER_TOML).is_file());
    }

    #[test]
    fn clean_keeps_source_and_drops_build_output() {
        let root = tempfile::tempdir().unwrap();
        let layout = Layout::new(root.path());
        let f = fetch(&layout, &scaffold());
        let course = layout.course_dir("esp-gpio");
        for d in [
            course.join(".target/debug"),
            course.join(".embuild/tools"),
            f.dir.join(".runs/1"),
            f.dir.join("target"),
        ] {
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("blob"), vec![0u8; 100]).unwrap();
        }

        let freed = clean_exercise(&f.dir).unwrap();
        assert_eq!(
            freed,
            Freed {
                bytes: 200,
                dirs: 2
            }
        );
        assert!(!f.dir.join(".runs").exists() && !f.dir.join("target").exists());
        assert!(
            course.join(".target").exists(),
            "the course cache is shared"
        );
        assert!(!f.is_edited().unwrap());

        std::fs::create_dir_all(f.dir.join(".runs/2")).unwrap();
        let freed = clean_course(&course).unwrap();
        assert_eq!(freed.dirs, 3);
        assert_eq!(freed.bytes, 200);
        assert!(!course.join(".target").exists() && !course.join(".embuild").exists());
        assert!(!f.dir.join(".runs").exists());
        assert!(f.dir.join("src/bin/main.rs").is_file());
        assert!(f.dir.join(TER_TOML).is_file());

        assert_eq!(clean_course(&course).unwrap(), Freed::default());
    }

    #[test]
    fn course_dirs_include_a_cache_with_no_exercises_left() {
        let root = tempfile::tempdir().unwrap();
        let layout = Layout::new(root.path());
        fetch(&layout, &scaffold());
        std::fs::create_dir_all(root.path().join("emptied/.target")).unwrap();
        std::fs::create_dir_all(root.path().join("unrelated/stuff")).unwrap();
        assert_eq!(
            layout.course_dirs().unwrap(),
            [root.path().join("emptied"), root.path().join("esp-gpio")]
        );
    }

    #[test]
    fn remove_refuses_an_edited_exercise_unless_forced() {
        let root = tempfile::tempdir().unwrap();
        let layout = Layout::new(root.path());
        let f = fetch(&layout, &scaffold());
        std::fs::write(f.dir.join("src/bin/main.rs"), "fn main() { todo!() }").unwrap();
        let err = remove_exercise(&f, false).unwrap_err();
        assert_eq!(err.code, "source_edited");
        assert!(f.dir.join("src/bin/main.rs").exists());

        remove_exercise(&f, true).unwrap();
        assert!(!f.dir.exists());
        assert_eq!(
            layout.find(&f.ter.exercise).unwrap_err().code,
            "not_fetched"
        );
    }

    #[test]
    fn remove_deletes_an_unedited_exercise() {
        let root = tempfile::tempdir().unwrap();
        let f = fetch(&Layout::new(root.path()), &scaffold());
        std::fs::create_dir_all(f.dir.join("target")).unwrap();
        remove_exercise(&f, false).unwrap();
        assert!(!f.dir.exists());
        assert!(root.path().join("esp-gpio").is_dir(), "the course stays");
    }

    #[test]
    fn cargo_env_points_at_the_course_cache() {
        let layout = Layout::new("/courses");
        let ter = TerToml {
            exercise: "e".into(),
            course: "c".into(),
            target: "t".into(),
            modes: vec!["hardware".into()],
            mode: "hardware".into(),
            venue: None,
            fetched_sha256: String::new(),
            pins: BTreeMap::new(),
        };
        let env = layout.cargo_env(&ter, Path::new("/elsewhere/e"));
        assert_eq!(
            env,
            [
                ("CARGO_TARGET_DIR", OsString::from("/courses/c/.target")),
                (
                    "ESP_IDF_TOOLS_INSTALL_DIR",
                    OsString::from("custom:/courses/c/.embuild")
                ),
                ("CARGO_WORKSPACE_DIR", OsString::from("/elsewhere/e")),
            ]
        );
    }

    #[test]
    fn enclosing_exercise_walks_up() {
        let root = tempfile::tempdir().unwrap();
        let f = fetch(&Layout::new(root.path()), &scaffold());
        assert_eq!(
            enclosing_exercise(&f.dir.join("src/bin")).as_deref(),
            Some(f.dir.as_path())
        );
        assert_eq!(enclosing_exercise(root.path()), None);
    }

    #[test]
    fn recordings_number_on_and_state_survives_clean_and_edit_check() {
        let root = tempfile::tempdir().unwrap();
        let f = fetch(&Layout::new(root.path()), &scaffold());
        let (n1, d1) = new_recording(&f.dir).unwrap();
        let (n2, d2) = new_recording(&f.dir).unwrap();
        assert_eq!((n1, n2), (1, 2));
        assert_eq!(d2, f.dir.join(".runs/2"));
        assert!(d1.is_dir() && d2.is_dir());

        std::fs::create_dir_all(f.dir.join(STATE_DIR)).unwrap();
        std::fs::write(f.dir.join(STATE_DIR).join("last-run.json"), "{}").unwrap();
        assert!(
            !f.is_edited().unwrap(),
            "state and recordings are not edits"
        );
        clean_exercise(&f.dir).unwrap();
        assert!(!f.dir.join(RUNS_DIR).exists());
        assert!(f.dir.join(STATE_DIR).join("last-run.json").is_file());
        assert_eq!(new_recording(&f.dir).unwrap().0, 1);
    }
}
