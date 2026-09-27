//! `ter status`: where the learner is, from the site's enrolments and the
//! last run posted from each exercise folder; `--disk`, what the courses
//! cost on disk.

use std::path::{Path, PathBuf};

use serde::Serialize;
use ter_sdk::project::{
    COURSE_EMBUILD_DIR, COURSE_TARGET_DIR, Fetched, Layout, RUNS_DIR, TerToml, disk_usage,
};
use ter_sdk::run::LastRun;

use crate::config::Config;
use crate::exercises::human_bytes;
use crate::output::{CliError, print_json};
use crate::session::Session;

#[derive(Serialize)]
struct Row {
    exercise_id: String,
    course_id: String,
    /// The lesson the exercise belongs to; `None` for one fetched from a
    /// course this account is not enrolled in.
    lesson_title: Option<String>,
    /// `done`, `open`, `locked`, or `unlisted`.
    state: &'static str,
    path: Option<PathBuf>,
    edited: Option<bool>,
    last_run: Option<LastRunOut>,
}

#[derive(Serialize)]
struct LastRunOut {
    run: String,
    attempt: u32,
    posted_at: String,
    mode: String,
    build_status: String,
    check_status: String,
    concept_deltas: Vec<ter_sdk::run::ConceptDelta>,
    /// Hints taken on this run; the next run reports it.
    hints_used: u32,
    recording: PathBuf,
}

impl LastRunOut {
    fn from(last: LastRun) -> Self {
        Self {
            run: last.answer.name,
            attempt: last.answer.attempt,
            posted_at: last.posted_at,
            mode: last.record.mode,
            build_status: last.record.build_status,
            check_status: last.record.check_status,
            concept_deltas: last.answer.concept_deltas,
            hints_used: last.hints.used,
            recording: last.recording,
        }
    }

    fn summary(&self) -> String {
        let outcome = if self.build_status == "failed" {
            "build failed".to_string()
        } else {
            match self.check_status.as_str() {
                "passed" => "passed".into(),
                "failed" => "check failed".into(),
                _ => "built, not checked".into(),
            }
        };
        format!(
            "attempt {}, {} ({}), {}",
            self.attempt, outcome, self.mode, self.posted_at
        )
    }
}

pub async fn status(json: bool) -> Result<(), CliError> {
    let layout = Layout::new(Config::load()?.courses_root);
    let session = Session::open()?;
    let client = &session.client;
    let enrollments = client
        .enrollments()
        .await
        .map_err(|e| session.forget_dead_token(e.into()))?;
    let user = client.ping().await.map_err(CliError::from)?.user.clone();
    let site = client.site_url().to_string();
    let mut fetched = layout.exercises().unwrap_or_default();
    // In an exercise folder, wherever it was fetched to, that exercise.
    let here = match std::env::current_dir()
        .ok()
        .and_then(|d| ter_sdk::project::enclosing_exercise(&d))
    {
        Some(dir) => {
            let ter = TerToml::load(&dir)?;
            fetched.retain(|f| f.ter.exercise != ter.exercise);
            let id = ter.exercise.clone();
            fetched.push(Fetched { dir, ter });
            Some(id)
        }
        None => None,
    };

    let last_run = |dir: &Path| {
        LastRun::load(dir)
            .ok()
            .flatten()
            .filter(|l| l.user == user && l.site == site)
            .map(LastRunOut::from)
    };
    let local = |f: Fetched| (f.is_edited().ok(), last_run(&f.dir), f.dir);

    let mut rows = Vec::new();
    for course in &enrollments.courses {
        for lesson in &course.lessons {
            for ex in lesson.all_exercises() {
                let mine = fetched
                    .iter()
                    .position(|f| f.ter.exercise == ex.exercise_id)
                    .map(|i| local(fetched.remove(i)));
                let (edited, last, path) = match mine {
                    Some((e, l, p)) => (e, l, Some(p)),
                    None => (None, None, None),
                };
                rows.push(Row {
                    exercise_id: ex.exercise_id.clone(),
                    course_id: course.course_id.clone(),
                    lesson_title: Some(lesson.title.clone()),
                    state: if lesson.completed {
                        "done"
                    } else if lesson.locked {
                        "locked"
                    } else {
                        "open"
                    },
                    path,
                    edited,
                    last_run: last,
                });
            }
        }
    }
    // Fetched, but not from an enrolled course: a staff fetch, or a course
    // the account has left.
    for f in fetched {
        let course_id = f.ter.course.clone();
        let exercise_id = f.ter.exercise.clone();
        let (edited, last, path) = local(f);
        rows.push(Row {
            exercise_id,
            course_id,
            lesson_title: None,
            state: "unlisted",
            path: Some(path),
            edited,
            last_run: last,
        });
    }

    if let Some(here) = here {
        let row = rows
            .into_iter()
            .find(|r| r.exercise_id == here)
            .expect("the exercise here has a row");
        if json {
            print_json(&row);
        } else {
            print_one(&row);
        }
        return Ok(());
    }
    if json {
        print_json(&serde_json::json!({ "user": user, "exercises": rows }));
        return Ok(());
    }
    println!("{user}");
    let mut course = "";
    for row in &rows {
        if row.course_id != course {
            course = &row.course_id;
            println!();
            println!("{course}");
        }
        let detail = match (&row.last_run, &row.path) {
            (Some(last), _) => last.summary(),
            (None, Some(_)) => "fetched, no run yet".into(),
            (None, None) => String::new(),
        };
        println!("  {:<8}  {:<44}  {detail}", row.state, row.exercise_id);
    }
    Ok(())
}

fn print_one(row: &Row) {
    println!(
        "{} ({})",
        row.lesson_title.as_deref().unwrap_or(&row.exercise_id),
        row.exercise_id
    );
    println!("  course    {} ({})", row.course_id, row.state);
    if let Some(path) = &row.path {
        println!("  folder    {}", path.display());
    }
    if let Some(edited) = row.edited {
        println!("  edited    {}", if edited { "yes" } else { "no" });
    }
    let Some(last) = &row.last_run else {
        println!("  last run  none from this folder; `ter run` posts one");
        return;
    };
    println!("  last run  {}", last.summary());
    println!("  run log   {}", last.recording.display());
    println!("  hints     {} taken on this run", last.hints_used);
    for (i, d) in last.concept_deltas.iter().enumerate() {
        println!(
            "  {:<8}  {:<24} {:.2} -> {:.2}",
            if i == 0 { "concepts" } else { "" },
            d.label.as_deref().unwrap_or(&d.concept),
            d.before,
            d.after
        );
    }
}

#[derive(Serialize)]
struct CourseDisk {
    course: String,
    path: PathBuf,
    total_bytes: u64,
    /// The course's shared build output and ESP-IDF tools.
    shared_cache_bytes: u64,
    exercises: Vec<ExerciseDisk>,
}

#[derive(Serialize)]
struct ExerciseDisk {
    exercise: String,
    total_bytes: u64,
    /// Its own `target/` and run recordings, which `ter ex clean` drops.
    build_bytes: u64,
}

/// Local only: needs no token.
pub fn disk(json: bool) -> Result<(), CliError> {
    let layout = Layout::new(Config::load()?.courses_root);
    let mut courses = Vec::new();
    for dir in layout.course_dirs()? {
        let exercises = layout
            .exercises()?
            .into_iter()
            .filter(|f| f.dir.parent() == Some(dir.as_path()))
            .map(|f| ExerciseDisk {
                exercise: f.ter.exercise,
                total_bytes: disk_usage(&f.dir),
                build_bytes: disk_usage(&f.dir.join("target")) + disk_usage(&f.dir.join(RUNS_DIR)),
            })
            .collect();
        courses.push(CourseDisk {
            course: dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            total_bytes: disk_usage(&dir),
            shared_cache_bytes: disk_usage(&dir.join(COURSE_TARGET_DIR))
                + disk_usage(&dir.join(COURSE_EMBUILD_DIR)),
            path: dir,
            exercises,
        });
    }
    let total: u64 = courses.iter().map(|c| c.total_bytes).sum();
    if json {
        print_json(&serde_json::json!({
            "root": layout.root, "total_bytes": total, "courses": courses,
        }));
        return Ok(());
    }
    if courses.is_empty() {
        println!("Nothing fetched under {}.", layout.root.display());
        return Ok(());
    }
    for c in &courses {
        println!(
            "{:<44}  {:>9}  (shared build cache {})",
            c.course,
            human_bytes(c.total_bytes),
            human_bytes(c.shared_cache_bytes)
        );
        for e in &c.exercises {
            println!(
                "  {:<42}  {:>9}  (build output {})",
                e.exercise,
                human_bytes(e.total_bytes),
                human_bytes(e.build_bytes)
            );
        }
    }
    println!();
    println!(
        "{} under {}. `ter ex clean --course <course>` drops a course's build output; the source stays.",
        human_bytes(total),
        layout.root.display()
    );
    Ok(())
}
