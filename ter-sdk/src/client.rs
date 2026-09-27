use reqwest::{Method, RequestBuilder};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::OnceCell;

use crate::bench::{Connected, Done, Heartbeat, Mine, Registered, Shared, Unshared};
use crate::exercise::{Exercise, ExerciseRef};
use crate::pairing::{PairStart, PollStatus};
use crate::run::{HintAnswer, HintExchange, HintFile, RunAnswer, RunRecord};
use crate::{Error, envelope::parse_response, version::check_supported};

/// The site's answer to `ping`: who the token belongs to and which CLI
/// versions it accepts.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Ping {
    pub ok: bool,
    pub user: String,
    pub server_time: String,
    pub min_supported_version: String,
    pub latest_version: String,
    #[serde(default)]
    pub download_url: Option<String>,
    /// Whether the account holds the TER Premium role. `None` from a site
    /// that does not report it.
    #[serde(default)]
    pub premium: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Enrollments {
    pub courses: Vec<Course>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Course {
    pub course_id: String,
    pub title: String,
    #[serde(default)]
    pub lessons: Vec<Lesson>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Lesson {
    pub lesson_id: String,
    pub title: String,
    #[serde(default)]
    pub completed: bool,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub exercise: Option<ExerciseRef>,
    #[serde(default)]
    pub exercises: Vec<ExerciseRef>,
}

impl Lesson {
    /// The lesson's exercises, from `exercises` or, from a site that sends
    /// only the one, `exercise`.
    pub fn all_exercises(&self) -> impl Iterator<Item = &ExerciseRef> {
        let single = self.exercise.iter().filter(|_| self.exercises.is_empty());
        self.exercises.iter().chain(single)
    }
}

impl Enrollments {
    /// The course an exercise belongs to, if it is in an enrolled course.
    pub fn course_of(&self, exercise_id: &str) -> Option<&Course> {
        self.courses.iter().find(|c| {
            c.lessons
                .iter()
                .flat_map(Lesson::all_exercises)
                .any(|e| e.exercise_id == exercise_id)
        })
    }
}

/// A client for the TER API on one site, as one CLI version, with one
/// device token (or none).
pub struct Client {
    http: reqwest::Client,
    site_url: String,
    token: Option<String>,
    cli_version: String,
    ping: OnceCell<Ping>,
}

impl Client {
    pub fn new(site_url: &str, token: Option<String>, cli_version: &str) -> Result<Self, Error> {
        reqwest::Url::parse(site_url)
            .map_err(|e| Error::Network(format!("site URL {site_url:?} is not valid: {e}")))?;
        let http = reqwest::Client::builder()
            .user_agent(format!("ter-cli/{cli_version}"))
            .build()
            .map_err(|e| Error::Network(e.to_string()))?;
        Ok(Self {
            http,
            site_url: site_url.trim_end_matches('/').to_string(),
            token,
            cli_version: cli_version.to_string(),
            ping: OnceCell::new(),
        })
    }

    pub fn site_url(&self) -> &str {
        &self.site_url
    }

    pub fn cli_version(&self) -> &str {
        &self.cli_version
    }

    /// `ping`, once per client.
    pub async fn ping(&self) -> Result<&Ping, Error> {
        self.ping.get_or_try_init(|| self.get("ping", &[])).await
    }

    /// The `ping` answer if this client has already fetched it; never
    /// sends a request.
    pub fn cached_ping(&self) -> Option<&Ping> {
        self.ping.get()
    }

    /// Fail with `outdated` when this CLI is below the site's minimum.
    pub async fn ensure_supported(&self) -> Result<(), Error> {
        let ping = self.ping().await?;
        check_supported(&self.cli_version, &ping.min_supported_version)
    }

    pub async fn enrollments(&self) -> Result<Enrollments, Error> {
        self.get("enrollments", &[]).await
    }

    /// One exercise with its files: `not_found`, `not_enrolled` or `locked`
    /// when this account may not have it.
    pub async fn exercise(&self, exercise_id: &str) -> Result<Exercise, Error> {
        self.get("exercise", &[("exercise_id", exercise_id)]).await
    }

    /// Post one run record. Refused before sending when this CLI is below
    /// the site's minimum.
    pub async fn post_run(&self, record: &RunRecord) -> Result<RunAnswer, Error> {
        self.post("run", &serde_json::json!({ "payload": record }))
            .await
    }

    /// The next hint for a run this account posted.
    pub async fn hint(&self, run: &str, files: &[HintFile]) -> Result<HintAnswer, Error> {
        self.post("hint", &serde_json::json!({ "run": run, "files": files }))
            .await
    }

    /// Share one exchange with a hint model (the learner's own provider)
    /// against a run, with the learner's consent. `not_on_site` until the
    /// site takes them.
    pub async fn hint_exchange(&self, exchange: &HintExchange) -> Result<Value, Error> {
        self.post("hint_exchange", &serde_json::json!({ "payload": exchange }))
            .await
    }

    /// Announce a bench this machine serves, or update `bench` (a board
    /// swap, a new URL). The site matches an unnamed bench by its label.
    pub async fn bench_register(
        &self,
        board: &str,
        label: Option<&str>,
        url: Option<&str>,
        bench: Option<&str>,
    ) -> Result<Registered, Error> {
        self.post(
            "bench_register",
            &serde_json::json!({"board": board, "label": label, "url": url, "bench": bench}),
        )
        .await
    }

    /// Tell the site the bench is alive: `available`, or `busy` with a run.
    pub async fn bench_heartbeat(&self, bench: &str, busy: bool) -> Result<Heartbeat, Error> {
        let status = if busy { "busy" } else { "available" };
        self.post(
            "bench_heartbeat",
            &serde_json::json!({"bench": bench, "status": status}),
        )
        .await
    }

    /// This account's own benches: whether each is sharing, under which
    /// code, and who is connected. The site's Devices page reads the same.
    pub async fn my_benches(&self) -> Result<Vec<Mine>, Error> {
        #[derive(Deserialize)]
        struct Devices {
            #[serde(default)]
            mine: Vec<Mine>,
        }
        let d: Devices = self.get("devices", &[]).await?;
        Ok(d.mine)
    }

    /// Turn sharing on for an own bench; the same code while it stays on.
    pub async fn bench_share(&self, bench: &str) -> Result<Shared, Error> {
        self.post("bench_share", &serde_json::json!({ "bench": bench }))
            .await
    }

    /// Sharing off: the code stops working and everyone connected is
    /// dropped.
    pub async fn bench_unshare(&self, bench: &str) -> Result<Unshared, Error> {
        self.post("bench_unshare", &serde_json::json!({ "bench": bench }))
            .await
    }

    /// Connect to the bench sharing under `code`.
    pub async fn bench_connect(&self, code: &str) -> Result<Connected, Error> {
        self.post("bench_connect", &serde_json::json!({ "code": code }))
            .await
    }

    /// Connect again to a bench already in this account's list.
    pub async fn bench_reconnect(&self, bench: &str) -> Result<Connected, Error> {
        self.post("bench_reconnect", &serde_json::json!({ "bench": bench }))
            .await
    }

    /// End this account's session on a bench; it stays in the list.
    pub async fn bench_disconnect(&self, bench: &str) -> Result<Done, Error> {
        self.post("bench_disconnect", &serde_json::json!({ "bench": bench }))
            .await
    }

    /// Take a bench off this account's list.
    pub async fn bench_forget(&self, bench: &str) -> Result<Done, Error> {
        self.post("bench_forget", &serde_json::json!({ "bench": bench }))
            .await
    }

    /// Delete an own bench and every share on it.
    pub async fn bench_remove(&self, bench: &str) -> Result<Done, Error> {
        self.post("bench_remove", &serde_json::json!({ "bench": bench }))
            .await
    }

    /// Ask for a pairing code. Needs no token.
    pub async fn pair_start(&self) -> Result<PairStart, Error> {
        let req = self
            .http
            .post(self.endpoint("pair_start"))
            .json(&serde_json::json!({}));
        self.send("pair_start", req).await
    }

    /// Whether `code` has been approved yet. Needs no token.
    pub async fn pair_poll(&self, code: &str) -> Result<PollStatus, Error> {
        let req = self
            .http
            .get(self.endpoint("pair_poll"))
            .query(&[("code", code)]);
        self.send("pair_poll", req).await
    }

    /// The page where the learner approves `code`, with the code filled in.
    pub fn pairing_url(&self, code: &str) -> String {
        let mut url = reqwest::Url::parse(&format!("{}/cli", self.site_url))
            .expect("the site URL was parsed when the client was built");
        url.query_pairs_mut().append_pair("code", code);
        url.into()
    }

    /// A client for the same site and CLI version with another token.
    pub fn with_token(&self, token: String) -> Self {
        Self {
            http: self.http.clone(),
            site_url: self.site_url.clone(),
            token: Some(token),
            cli_version: self.cli_version.clone(),
            ping: OnceCell::new(),
        }
    }

    /// GET a token endpoint.
    pub async fn get<T: DeserializeOwned>(
        &self,
        function: &str,
        query: &[(&str, &str)],
    ) -> Result<T, Error> {
        let req = self.authed(Method::GET, function)?.query(query);
        self.send(function, req).await
    }

    /// POST to a token endpoint that records something on the site. Refused
    /// before anything is sent when this CLI is below the site's minimum.
    pub async fn post<T: DeserializeOwned>(
        &self,
        function: &str,
        body: &Value,
    ) -> Result<T, Error> {
        let req = self.authed(Method::POST, function)?.json(body);
        self.ensure_supported().await?;
        self.send(function, req).await
    }

    fn endpoint(&self, function: &str) -> String {
        format!("{}/api/method/ter_courses.api.{function}", self.site_url)
    }

    fn authed(&self, method: Method, function: &str) -> Result<RequestBuilder, Error> {
        let token = self.token.as_deref().ok_or(Error::NoToken)?;
        Ok(self
            .http
            .request(method, self.endpoint(function))
            .bearer_auth(token))
    }

    async fn send<T: DeserializeOwned>(
        &self,
        function: &str,
        req: RequestBuilder,
    ) -> Result<T, Error> {
        let resp = req.send().await.map_err(network)?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await.map_err(network)?;
        let value = parse_response(status, &body)?;
        serde_json::from_value(value).map_err(|e| Error::UnexpectedAnswer {
            function: function.to_string(),
            detail: e.to_string(),
        })
    }
}

fn network(e: reqwest::Error) -> Error {
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        msg = format!("{msg}: {s}");
        source = s.source();
    }
    Error::Network(msg)
}
