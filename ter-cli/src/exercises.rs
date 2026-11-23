//! `ter ex`: list, fetch, clean and remove exercises.

use std::path::{Path, PathBuf};

use serde::Serialize;
use ter_sdk::exercise::default_modes;
use ter_sdk::project::{self, Fetched, Freed, Layout, Scaffold};
use ter_sdk::{Exercise, dev};
use ter_telemetry::sim86;

use crate::config::Config;
use crate::output::{CliError, print_json};
use crate::session::Session;

/// Where exercises go when the account can fetch one that is not in any of
/// its enrolled courses (staff pass the enrolment check everywhere).
const UNLISTED_COURSE: &str = "unlisted";

fn layout() -> Result<Layout, CliError> {
    Ok(Layout::new(Config::load()?.courses_root))
}

#[derive(Serialize)]
struct ListedCourse<'a> {
    course_id: &'a str,
    title: &'a str,
    exercises: Vec<Listed<'a>>,
}

#[derive(Serialize)]
struct Listed<'a> {
    exercise_id: &'a str,
    lesson_id: &'a str,
    lesson_title: &'a str,
    target: &'a str,
    kind: &'a str,
    locked: bool,
    completed: bool,
    /// The folder it was fetched to, if it has been.
    fetched: Option<PathBuf>,
}

pub async fn list(session: &Session, course: Option<&str>, json: bool) -> Result<(), CliError> {
    let enrollments = session.client.enrollments().await?;
    if let Some(wanted) = course
        && !enrollments.courses.iter().any(|c| c.course_id == wanted)
    {
        let enrolled: Vec<_> = enrollments
            .courses
            .iter()
            .map(|c| c.course_id.as_str())
            .collect();
        return Err(CliError::new(
            "not_enrolled",
            format!(
                "You are not enrolled in {wanted}. Your courses: {}.",
                if enrolled.is_empty() {
                    "none".into()
                } else {
                    enrolled.join(", ")
                }
            ),
        ));
    }
    let fetched = layout()?.exercises().unwrap_or_default();
    let fetched_dir = |id: &str| {
        fetched
            .iter()
            .find(|f| f.ter.exercise == id)
            .map(|f| f.dir.clone())
    };

    let courses: Vec<ListedCourse> = enrollments
        .courses
        .iter()
        .filter(|c| course.is_none_or(|wanted| c.course_id == wanted))
        .map(|c| ListedCourse {
            course_id: &c.course_id,
            title: &c.title,
            exercises: c
                .lessons
                .iter()
                .flat_map(|l| l.all_exercises().map(move |e| (l, e)))
                .map(|(l, e)| Listed {
                    exercise_id: &e.exercise_id,
                    lesson_id: &l.lesson_id,
                    lesson_title: &l.title,
                    target: &e.target,
                    kind: &e.kind,
                    locked: l.locked,
                    completed: l.completed,
                    fetched: fetched_dir(&e.exercise_id),
                })
                .collect(),
        })
        .collect();

    if json {
        print_json(&serde_json::json!({ "courses": courses }));
        return Ok(());
    }
    if courses.is_empty() {
        println!("You are not enrolled in any course with exercises.");
    }
    for (i, c) in courses.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!("{} ({})", c.title, c.course_id);
        if c.exercises.is_empty() {
            println!("  no exercises");
        }
        let width = c
            .exercises
            .iter()
            .map(|e| e.exercise_id.len())
            .max()
            .unwrap_or(0);
        for e in &c.exercises {
            let state = if e.completed {
                "done"
            } else if e.locked {
                "locked"
            } else {
                "open"
            };
            let here = if e.fetched.is_some() { "  fetched" } else { "" };
            println!(
                "  {state:<6}  {:<width$}  {}{here}",
                e.exercise_id, e.lesson_title
            );
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct FetchedOut<'a> {
    exercise: &'a str,
    title: &'a str,
    course: &'a str,
    target: &'a str,
    path: &'a Path,
    modes: &'a [String],
    mode: &'a str,
    runner: &'a str,
    files: usize,
    dev: bool,
}

/// From the site, or with `dev` from a curriculum checkout.
pub async fn fetch(
    id: &str,
    dir: Option<PathBuf>,
    dev: Option<&Path>,
    json: bool,
) -> Result<(), CliError> {
    let (exercise, course, modes) = match dev {
        Some(checkout) => {
            let d = dev::load(checkout, id)?;
            (d.exercise, d.course, d.modes)
        }
        None => {
            let session = Session::open()?;
            fetch_from_site(&session, id)
                .await
                .map_err(|e| session.forget_dead_token(e))?
        }
    };
    let scaffold = Scaffold::check(&exercise)?;
    let dest = match dir {
        Some(d) => d,
        None => layout()?.exercise_dir(&course, &exercise.exercise_id),
    };
    let Fetched { dir, ter } = scaffold.write(&dest, &course, modes)?;

    if json {
        print_json(&FetchedOut {
            exercise: &ter.exercise,
            title: &exercise.title,
            course: &ter.course,
            target: &ter.target,
            path: &dir,
            modes: &ter.modes,
            mode: &ter.mode,
            runner: &scaffold.runner,
            files: scaffold.files.len(),
            dev: dev.is_some(),
        });
        return Ok(());
    }
    println!("Fetched {} ({})", exercise.title, ter.exercise);
    println!("  into    {}", dir.display());
    println!("  course  {}", ter.course);
    println!("  target  {}", ter.target);
    println!("  mode    {} (allowed: {})", ter.mode, ter.modes.join(", "));
    println!(
        "`cargo run` in that folder builds it and runs `{}`.",
        scaffold.runner
    );
    Ok(())
}

/// Fetch `id` from the site into its course folder, as `ter ex fetch <id>`
/// does, and return the folder.
pub async fn fetch_into_courses(session: &Session, id: &str) -> Result<PathBuf, CliError> {
    let (exercise, course, modes) = fetch_from_site(session, id).await?;
    let scaffold = Scaffold::check(&exercise)?;
    let dest = layout()?.exercise_dir(&course, &exercise.exercise_id);
    Ok(scaffold.write(&dest, &course, modes)?.dir)
}

/// The folder `id` was fetched to, if it was.
pub fn fetched_dir(id: &str) -> Result<Option<PathBuf>, CliError> {
    Ok(layout()?
        .exercises()
        .unwrap_or_default()
        .into_iter()
        .find(|f| f.ter.exercise == id)
        .map(|f| f.dir))
}

async fn fetch_from_site(
    session: &Session,
    id: &str,
) -> Result<(Exercise, String, Vec<String>), CliError> {
    // The site decides whether this account may have it (`not_enrolled`,
    // `locked`), so ask for the exercise before anything else.
    let exercise = session.client.exercise(id).await?;
    let enrollments = session.client.enrollments().await?;
    let course = enrollments
        .course_of(&exercise.exercise_id)
        .map(|c| c.course_id.clone())
        .unwrap_or_else(|| UNLISTED_COURSE.into());
    // A site that does not say which modes an exercise allows: allow both
    // and let the site's `mode_not_allowed` on `run` be the check.
    let modes = exercise.modes.clone().unwrap_or_else(default_modes);
    Ok((exercise, course, modes))
}

/// What to clean: one exercise, one course, or everything.
pub enum CleanScope {
    Here,
    Exercise(String),
    Course(String),
    All,
}

#[derive(Serialize)]
struct CleanedOut {
    cleaned: Vec<PathBuf>,
    freed_bytes: u64,
    removed_dirs: u32,
}

pub fn clean(scope: CleanScope, json: bool) -> Result<(), CliError> {
    let layout = layout()?;
    let mut freed = Freed::default();
    let mut cleaned = Vec::new();
    let mut note = None;
    match scope {
        CleanScope::Here | CleanScope::Exercise(_) => {
            let dir = match scope {
                CleanScope::Exercise(id) => layout.find(&id)?.dir,
                _ => here()?,
            };
            freed += project::clean_exercise(&dir)?;
            let course = project::TerToml::load(&dir)?.course;
            note = Some(format!(
                "The course's shared build cache stays; `ter ex clean --course {course}` drops it."
            ));
            cleaned.push(dir);
        }
        CleanScope::Course(course) => {
            let dir = layout.course_dir(&course);
            if !dir.is_dir() {
                return Err(CliError::new(
                    "not_fetched",
                    format!("No course folder {}.", dir.display()),
                ));
            }
            freed += project::clean_course(&dir)?;
            cleaned.push(dir);
        }
        CleanScope::All => {
            for dir in layout.course_dirs()? {
                freed += project::clean_course(&dir)?;
                cleaned.push(dir);
            }
        }
    }

    if json {
        print_json(&CleanedOut {
            cleaned,
            freed_bytes: freed.bytes,
            removed_dirs: freed.dirs,
        });
        return Ok(());
    }
    if cleaned.is_empty() {
        println!("Nothing fetched under {}.", layout.root.display());
    } else if freed.dirs == 0 {
        println!("Nothing to clean.");
    } else {
        println!("Freed {}.", human_bytes(freed.bytes));
    }
    if let Some(note) = note {
        println!("{note}");
    }
    Ok(())
}

pub fn remove(id: &str, force: bool, json: bool) -> Result<(), CliError> {
    let fetched = layout()?.find(id)?;
    let edited = fetched.is_edited()?;
    let freed = project::remove_exercise(&fetched, force)?;
    if json {
        print_json(&serde_json::json!({
            "removed": fetched.dir,
            "edited": edited,
            "freed_bytes": freed.bytes,
        }));
    } else {
        println!(
            "Removed {} ({}).",
            fetched.dir.display(),
            human_bytes(freed.bytes)
        );
    }
    Ok(())
}

fn here() -> Result<PathBuf, CliError> {
    let cwd = std::env::current_dir()
        .map_err(|e| CliError::new("io_error", format!("No current directory: {e}")))?;
    project::enclosing_exercise(&cwd).ok_or_else(|| {
        CliError::new(
            "not_an_exercise",
            "This is not an exercise folder. Run it in one, or name what to clean: `ter ex clean <id>`, `--course <course>` or `--all`.",
        )
    })
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    #[test]
    fn bytes_read_like_a_file_manager() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}

/// Where `ter ex show` found the exercise.
enum Shown {
    /// A fetched folder: read offline, its check.yaml as fetched.
    Folder {
        dir: PathBuf,
        ter: project::TerToml,
        check: Option<(PathBuf, String)>,
    },
    /// Not fetched: one `exercise` call to the site, nothing written.
    Site {
        exercise: Exercise,
        check: Option<String>,
    },
}

#[derive(Serialize)]
struct ShowOut {
    exercise: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    course: Option<String>,
    target: String,
    modes: Vec<String>,
    /// `folder` or `site`.
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
    /// The check.yaml read, for a fetched folder: the copy as fetched.
    #[serde(skip_serializing_if = "Option::is_none")]
    check_yaml: Option<PathBuf>,
    has_check: bool,
    checks: Vec<ShownCheck>,
    /// The modes a run can pass it in: `hardware`, `simulation`, both or
    /// neither.
    passes_in: Vec<&'static str>,
    /// The simulators that can run it: `wokwi`, `sim86`, both or neither.
    sim_venues: Vec<&'static str>,
    summary: String,
}

#[derive(Serialize)]
struct ShownCheck {
    id: String,
    kind: &'static str,
    /// Whether a bare board can grade it (serial only, no `setup:`).
    board_grades: bool,
    graded: String,
}

/// `ter ex show [ID_OR_DIR]`: what an exercise checks and which run can
/// pass it. Grades nothing and runs nothing.
pub async fn show(what: Option<String>, json: bool) -> Result<(), CliError> {
    let shown = match what {
        None => from_folder(here_for("show")?)?,
        Some(arg) => {
            let path = PathBuf::from(&arg);
            if let Some(dir) = path
                .is_dir()
                .then(|| project::enclosing_exercise(&path))
                .flatten()
            {
                from_folder(dir)?
            } else if let Some(dir) = fetched_dir(&arg)? {
                from_folder(dir)?
            } else {
                let session = Session::open()?;
                let exercise = session
                    .client
                    .exercise(&arg)
                    .await
                    .map_err(|e| session.forget_dead_token(e.into()))?;
                let check = exercise
                    .files
                    .iter()
                    .find(|f| f.path == crate::run::CHECK_YAML)
                    .map(|f| {
                        f.bytes()
                            .map(|b| String::from_utf8_lossy(&b).into_owned())
                            .map_err(|e| {
                                CliError::new(
                                    "bad_check",
                                    format!("{}'s check.yaml: {e}", exercise.exercise_id),
                                )
                            })
                    })
                    .transpose()?;
                Shown::Site { exercise, check }
            }
        }
    };
    let out = describe(shown)?;
    if json {
        print_json(&out);
    } else {
        print_show(&out);
    }
    Ok(())
}

fn from_folder(dir: PathBuf) -> Result<Shown, CliError> {
    let ter = project::TerToml::load(&dir)?;
    let path = dir.join(crate::run::CHECK_YAML);
    let check = match std::fs::read_to_string(&path) {
        Ok(text) => Some((path, text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(CliError::new(
                "io_error",
                format!("Could not read {}: {e}", path.display()),
            ));
        }
    };
    Ok(Shown::Folder { dir, ter, check })
}

fn describe(shown: Shown) -> Result<ShowOut, CliError> {
    // A simulation run needs the exercise's circuit, whatever its modes say.
    let circuit = match &shown {
        Shown::Folder { dir, .. } => crate::sim::has_circuit(dir),
        Shown::Site { exercise, .. } => exercise
            .files
            .iter()
            .any(|f| f.path == ter_telemetry::wokwi::RUN_DIAGRAM),
    };
    // A sim86 run needs the exercise's sim86 board and pin map.
    let sim86_files = match &shown {
        Shown::Folder { dir, .. } => {
            let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
            read(sim86::BOARD_FILE).zip(read(sim86::PINS_FILE))
        }
        Shown::Site { exercise, .. } => {
            let read = |name: &str| {
                let file = exercise.files.iter().find(|f| f.path == name)?;
                Some(String::from_utf8_lossy(&file.bytes().ok()?).into_owned())
            };
            read(sim86::BOARD_FILE).zip(read(sim86::PINS_FILE))
        }
    };
    let (mut out, text) = match shown {
        Shown::Folder { dir, ter, check } => {
            let (check_yaml, text) = check.unzip();
            (
                ShowOut {
                    exercise: ter.exercise,
                    title: None,
                    course: Some(ter.course),
                    target: ter.target,
                    modes: ter.modes,
                    source: "folder",
                    path: Some(dir),
                    check_yaml,
                    has_check: false,
                    checks: vec![],
                    passes_in: vec![],
                    sim_venues: vec![],
                    summary: String::new(),
                },
                text,
            )
        }
        Shown::Site { exercise, check } => (
            ShowOut {
                modes: exercise.modes.clone().unwrap_or_else(default_modes),
                exercise: exercise.exercise_id,
                title: Some(exercise.title).filter(|t| !t.is_empty()),
                course: None,
                target: exercise.target,
                source: "site",
                path: None,
                check_yaml: None,
                has_check: false,
                checks: vec![],
                passes_in: vec![],
                sim_venues: vec![],
                summary: String::new(),
            },
            check,
        ),
    };
    let hardware = out.modes.iter().any(|m| m == "hardware");
    let sim_mode = out.modes.iter().any(|m| m == "simulation");
    let Some(text) = text else {
        out.summary = "No check: nothing to pass.".into();
        return Ok(out);
    };
    let where_ = out
        .check_yaml
        .clone()
        .unwrap_or_else(|| PathBuf::from(crate::run::CHECK_YAML));
    let file =
        ter_check::CheckFile::parse(&text).map_err(|e| crate::telemetry::bad_check(&where_, e))?;
    // sim86 runs it when its board shows every check: each watched pin on
    // a wired net, each pressed one on a button.
    let on_sim86 = sim86_files
        .as_ref()
        .map(|(board, pins)| crate::sim86::wiring(board, pins, &file).map_err(|e| e.to_string()));
    if sim_mode && circuit {
        out.sim_venues.push("wokwi");
    }
    if sim_mode && matches!(on_sim86, Some(Ok(_))) {
        out.sim_venues.push("sim86");
    }
    let simulation = !out.sim_venues.is_empty();
    out.has_check = true;
    out.checks = file
        .checks
        .iter()
        .map(|c| {
            let board_grades = file.board_grades(c);
            ShownCheck {
                id: c.id.clone(),
                kind: c.kind_name(),
                board_grades,
                graded: graded_where(board_grades, c.pin().is_some(), hardware, simulation),
            }
        })
        .collect();
    let needs_sim = file.needs_sim();
    if hardware && !needs_sim {
        out.passes_in.push("hardware");
    }
    if simulation {
        out.passes_in.push("simulation");
    }
    out.summary = match (hardware, simulation, needs_sim) {
        (true, true, false) => {
            "Passes on your board (`ter run --hw`) or in simulation (`ter run --sim`).".into()
        }
        (true, false, false) => "Passes on your board: `ter run --hw`.".into(),
        (_, true, true) if hardware => {
            "Passes only in simulation: `ter run --sim`. A run on a board can't complete it.".into()
        }
        (_, true, _) => "Passes in simulation: `ter run --sim`.".into(),
        (false, false, _) => "No ter run can pass it.".into(),
        (_, false, _) => "No ter run can pass it yet: some checks are graded only with a \
                          telemetry board, which ter does not support yet. Mark the lesson \
                          complete on the lesson page once the rest pass."
            .into(),
    };
    if sim_mode && !simulation {
        out.summary.push_str(&format!(
            " Its modes allow simulation, but it has no Wokwi circuit ({}), so `ter run --sim` can't run it.",
            ter_telemetry::wokwi::RUN_DIAGRAM
        ));
    }
    match (sim_mode, &on_sim86) {
        (true, Some(Ok(_))) if circuit => {
            out.summary
                .push_str(" sim86 runs it too: `ter run --sim sim86`.");
        }
        (true, Some(Ok(_))) => {
            out.summary
                .push_str(" The simulator is sim86 (`ter install sim86` sets it up).");
        }
        (true, Some(Err(why))) => {
            out.summary.push_str(&format!(
                " sim86 does not run it: its sim86 board cannot show these checks ({why})."
            ));
        }
        _ => {}
    }
    Ok(out)
}

/// Where one check is graded, in the exercise's modes, said plainly.
fn graded_where(board_grades: bool, watches_pin: bool, hardware: bool, simulation: bool) -> String {
    match (hardware, simulation) {
        (true, true) if board_grades => "graded on a board and in simulation".into(),
        (true, true) if watches_pin => {
            "graded only in simulation; a board shows it, but ter can't grade it there".into()
        }
        (true, true) => {
            "graded only in simulation: it runs under the setup's input, which only the simulator drives".into()
        }
        (true, false) if board_grades => "graded on a board".into(),
        (true, false) => "graded only with a telemetry board, which ter does not support yet".into(),
        (false, _) => "graded in simulation".into(),
    }
}

fn print_show(out: &ShowOut) {
    match &out.title {
        Some(title) => println!("{title} ({})", out.exercise),
        None => println!("{}", out.exercise),
    }
    if let Some(path) = &out.path {
        println!("  folder  {}", path.display());
    }
    if let Some(course) = &out.course {
        println!("  course  {course}");
    }
    println!("  target  {}", out.target);
    println!("  modes   {}", out.modes.join(", "));
    if out.sim_venues.contains(&"sim86") {
        println!("  sims    {}", out.sim_venues.join(", "));
    }
    if out.has_check {
        match &out.check_yaml {
            Some(path) => println!(
                "  checks  from {} (the copy fetched; the site's can be newer)",
                path.display()
            ),
            None => println!("  checks  from the site (not fetched)"),
        }
        let width = out.checks.iter().map(|c| c.id.len()).max().unwrap_or(0);
        let kind_width = out.checks.iter().map(|c| c.kind.len()).max().unwrap_or(0);
        for c in &out.checks {
            println!(
                "    {:<width$}  {:<kind_width$}  {}",
                c.id, c.kind, c.graded
            );
        }
    }
    println!("{}", out.summary);
}

fn here_for(command: &str) -> Result<PathBuf, CliError> {
    let cwd = std::env::current_dir()
        .map_err(|e| CliError::new("io_error", format!("No current directory: {e}")))?;
    project::enclosing_exercise(&cwd).ok_or_else(|| {
        CliError::new(
            "not_an_exercise",
            format!(
                "This is not an exercise folder. Run it in one, or name one: `ter ex {command} <id>`."
            ),
        )
    })
}
