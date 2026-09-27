//! `ter ex`: list, fetch, clean and remove exercises.

use std::path::{Path, PathBuf};

use serde::Serialize;
use ter_sdk::exercise::default_modes;
use ter_sdk::project::{self, Fetched, Freed, Layout, Scaffold};
use ter_sdk::{Exercise, dev};

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

fn human_bytes(bytes: u64) -> String {
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
