use serde::Serialize;

use crate::output::{CliError, print_json};
use crate::session::Session;

#[derive(Serialize)]
struct Whoami<'a> {
    user: &'a str,
    premium: Option<bool>,
    site: &'a str,
    token_source: &'a str,
    courses: Vec<CourseRef<'a>>,
    cli_version: &'a str,
    min_supported_version: &'a str,
    latest_version: &'a str,
    supported: bool,
}

#[derive(Serialize)]
struct CourseRef<'a> {
    course_id: &'a str,
    title: &'a str,
}

pub async fn run(session: &Session, json: bool) -> Result<(), CliError> {
    let client = &session.client;
    let ping = client.ping().await?;
    let enrollments = client.enrollments().await?;
    let supported = client.ensure_supported().await;

    if json {
        print_json(&Whoami {
            user: &ping.user,
            premium: ping.premium,
            site: client.site_url(),
            token_source: session.token_source,
            courses: enrollments
                .courses
                .iter()
                .map(|c| CourseRef {
                    course_id: &c.course_id,
                    title: &c.title,
                })
                .collect(),
            cli_version: client.cli_version(),
            min_supported_version: &ping.min_supported_version,
            latest_version: &ping.latest_version,
            supported: supported.is_ok(),
        });
        return Ok(());
    }

    println!("user     {}", ping.user);
    let premium = match ping.premium {
        Some(true) => "yes",
        Some(false) => "no (exercises need TER Premium)",
        None => "unknown",
    };
    println!("premium  {premium}");
    println!("site     {}", client.site_url());
    println!("token    {}", session.token_source);
    if enrollments.courses.is_empty() {
        println!("courses  none");
    }
    for (i, course) in enrollments.courses.iter().enumerate() {
        let label = if i == 0 { "courses" } else { "" };
        println!("{label:<8} {} ({})", course.title, course.course_id);
    }
    println!(
        "ter      {} (site accepts {} and later)",
        client.cli_version(),
        ping.min_supported_version
    );
    if let Err(e) = supported {
        eprintln!("warning: {e}");
    }
    Ok(())
}
