//! `ter llm` and `ter hint --llm` against a local mock of the site and of
//! a provider, on the recorded runs in `tests/fixtures/llm`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const API: &str = "/api/method/ter_courses.api";
const USER: &str = "learner@example.com";
const EXERCISE: &str = "gpio-blinky--xiao-esp32c3-nostd";
/// Looks like a real key, so a masked echo of it would be caught too.
const KEY: &str = "sk-proj-TESTKEY0123456789abcdefghijklmnopqrstuv";
const ANSWER: &str = "Which wait sets how long the LED stays on?";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/llm")
}

struct Machine {
    config: TempDir,
    exercise: TempDir,
}

impl Machine {
    /// An exercise folder holding the recorded run `fixture`, posted on
    /// `site`.
    fn with_run(fixture: &str, site: &str) -> Self {
        let config = tempfile::tempdir().unwrap();
        let exercise = tempfile::tempdir().unwrap();
        let from = fixtures().join(fixture);
        let dir = exercise.path();
        for rel in ["Cargo.toml", "src/lib.rs", "src/bin/main.rs"] {
            std::fs::create_dir_all(dir.join(rel).parent().unwrap()).unwrap();
            std::fs::copy(from.join(rel), dir.join(rel)).unwrap();
        }
        let sha = ter_sdk::project::source_sha256(dir).unwrap();
        std::fs::write(
            dir.join("ter.toml"),
            format!(
                "exercise = {EXERCISE:?}\ncourse = \"esp-gpio\"\ntarget = \"xiao-esp32c3-nostd\"\n\
                 modes = [\"hardware\", \"simulation\"]\nmode = \"simulation\"\nfetched_sha256 = {sha:?}\n"
            ),
        )
        .unwrap();
        let mut last: Value =
            serde_json::from_str(&std::fs::read_to_string(from.join("last-run.json")).unwrap())
                .unwrap();
        last["site"] = site.into();
        std::fs::create_dir_all(dir.join(".ter")).unwrap();
        std::fs::write(dir.join(".ter/last-run.json"), last.to_string()).unwrap();
        let recording = dir.join(last["recording"].as_str().unwrap());
        std::fs::create_dir_all(&recording).unwrap();
        if from.join("check.toml").is_file() {
            std::fs::copy(from.join("check.toml"), recording.join("check.toml")).unwrap();
        }
        Self { config, exercise }
    }

    fn config(&self, text: &str) {
        std::fs::write(self.config.path().join("config.toml"), text).unwrap();
    }

    fn ter(&self, site: &str, args: &[&str]) -> Output {
        self.ter_with(site, args, None, &[])
    }

    fn ter_with(
        &self,
        site: &str,
        args: &[&str],
        stdin: Option<&str>,
        env: &[(&str, &str)],
    ) -> Output {
        use std::io::Write;
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ter"));
        cmd.args(args)
            .current_dir(self.exercise.path())
            .env("TER_KEYCHAIN", "off")
            .env("TER_SITE_URL", site)
            .env("TER_CONFIG_DIR", self.config.path())
            .env("TER_TOKEN", "good-token")
            .env_remove("TER_LLM_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        if let Some(text) = stdin {
            input.write_all(text.as_bytes()).unwrap();
        }
        drop(input);
        child.wait_with_output().unwrap()
    }

    /// Store `KEY` for `provider` and point it at `base_url`.
    fn setup(&self, site: &str, provider: &str, base_url: &str, extra: &[&str]) {
        let mut args = vec![
            "llm",
            "setup",
            "--provider",
            provider,
            "--base-url",
            base_url,
            "--key-stdin",
        ];
        args.extend_from_slice(extra);
        let out = self.ter_with(site, &args, Some(&format!("{KEY}\n")), &[]);
        assert!(out.status.success(), "{}", text(&out));
        assert_no_key(&out);
    }

    /// Everything ter wrote in the config and exercise folders, as text,
    /// except the key's own store.
    fn written(&self) -> String {
        let mut all = String::new();
        for root in [self.config.path(), self.exercise.path()] {
            walk(root, &mut |p| {
                if p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("llm-key-"))
                {
                    return;
                }
                all.push_str(&String::from_utf8_lossy(&std::fs::read(p).unwrap()));
            });
        }
        all
    }
}

fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, f);
        } else {
            f(&p);
        }
    }
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn assert_no_key(out: &Output) {
    let all = text(out);
    assert!(!all.contains(KEY), "the key was printed:\n{all}");
    assert!(
        !all.contains(&KEY[..12]),
        "part of the key was printed:\n{all}"
    );
    assert!(
        !all.contains(&KEY[KEY.len() - 8..]),
        "part of the key was printed:\n{all}"
    );
}

fn json_out(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}\n{}", text(out)))
}

fn ok(v: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "message": v }))
}

/// The site: ping, the exercise, and `hint_exchange` when `takes_exchanges`
/// (otherwise Frappe's answer for a function it lacks).
async fn site(takes_exchanges: bool) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API}.ping")))
        .and(header("authorization", "Bearer good-token"))
        .respond_with(ok(json!({
            "ok": true, "user": USER, "server_time": "2026-09-27T12:00:00",
            "min_supported_version": "0.1.0", "latest_version": "0.1.0",
            "download_url": null, "premium": true
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API}.exercise")))
        .respond_with(ok(json!({
            "exercise_id": EXERCISE, "title": "Heartbeat", "target": "xiao-esp32c3-nostd",
            "runner": "cargo", "kind": "code",
            "instructions_md": std::fs::read_to_string(fixtures().join("lesson.md")).unwrap(),
            "files": []
        })))
        .mount(&server)
        .await;
    let exchange = if takes_exchanges {
        ok(json!({"ok": true, "name": "hx0001"}))
    } else {
        let item = json!({"message": "Failed to get method for command ter_courses.api.hint_exchange with module 'ter_courses.api' has no attribute 'hint_exchange'"}).to_string();
        let list = serde_json::to_string(&vec![item]).unwrap();
        ResponseTemplate::new(417)
            .set_body_json(json!({"exc_type": "ValidationError", "_server_messages": list}))
    };
    Mock::given(method("POST"))
        .and(path(format!("{API}.hint_exchange")))
        .respond_with(exchange)
        .mount(&server)
        .await;
    server
}

/// A provider speaking the chat completions format, answering `answer`.
async fn chat_provider(answer: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(answer)
        .mount(&server)
        .await;
    server
}

fn chat_answer(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}}]
    }))
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap()
}

fn body(r: &Request) -> Value {
    serde_json::from_slice(&r.body).unwrap()
}

fn raw(r: &Request) -> String {
    let headers: String = r
        .headers
        .iter()
        .map(|(k, v)| format!("{k}: {}\n", v.to_str().unwrap_or("")))
        .collect();
    format!(
        "{} {}\n{headers}\n{}",
        r.method,
        r.url,
        String::from_utf8_lossy(&r.body)
    )
}

#[tokio::test]
async fn a_hint_from_the_learners_model_sends_the_run_and_never_the_key_elsewhere() {
    let site = site(false).await;
    let provider = chat_provider(chat_answer(ANSWER)).await;
    let m = Machine::with_run("check-failed", &site.uri());
    m.setup(
        &site.uri(),
        "openai",
        &format!("{}/v1", provider.uri()),
        &[],
    );

    let out = m.ter(&site.uri(), &["hint", "--llm"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_no_key(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Hint from openai (gpt-5-mini) for attempt 7:"),
        "{stdout}"
    );
    assert!(stdout.contains(ANSWER), "{stdout}");

    // The provider got the key, as a bearer header, and the evidence.
    let sent = requests(&provider).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        format!("Bearer {KEY}")
    );
    let b = body(&sent[0]);
    assert_eq!(b["model"], "gpt-5-mini");
    assert!(b.get("max_completion_tokens").is_some(), "{b}");
    let prompt = b["messages"][1]["content"].as_str().unwrap();
    assert!(
        prompt.contains("Two short flashes, then a pause"),
        "the lesson"
    );
    assert!(
        prompt.contains("First failing check: flash-width"),
        "the run"
    );
    assert!(
        prompt.contains("(Level::High, 250_u64)"),
        "the learner's code"
    );
    assert!(!prompt.contains(KEY));

    // The site was asked for the lesson, and never saw the key. The
    // learner was not asked to share (no terminal), so nothing was posted.
    let to_site = requests(&site).await;
    assert!(to_site.iter().all(|r| !raw(r).contains(&KEY[..12])));
    assert!(
        !to_site
            .iter()
            .any(|r| r.url.path().ends_with("hint_exchange"))
    );

    // The exchange is kept, without the key; so is everything else.
    let kept: Vec<_> = std::fs::read_dir(m.exercise.path().join(".ter/llm"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(kept, ["r7cf0000bb-1.json"]);
    let written = m.written();
    assert!(!written.contains(&KEY[..12]) && !written.contains(&KEY[KEY.len() - 8..]));
}

#[tokio::test]
async fn json_output_carries_the_hint_and_where_it_went() {
    let site = site(true).await;
    let provider = chat_provider(chat_answer(ANSWER)).await;
    let m = Machine::with_run("build-failed", &site.uri());
    m.setup(
        &site.uri(),
        "openai",
        &format!("{}/v1", provider.uri()),
        &["--share"],
    );

    let out = m.ter(&site.uri(), &["hint", "--llm", "--json"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_no_key(&out);
    let v = json_out(&out);
    assert_eq!(v["text"], ANSWER);
    assert_eq!(v["provider"], "openai");
    assert_eq!(v["attempt"], 4);
    assert_eq!(v["shared"], "posted");
    assert_eq!(v["exchange"], "hx0001");
    assert_eq!(v["saved"], ".ter/llm/r4bf0000aa-1.json");

    let prompt = body(&requests(&provider).await[0])["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(prompt.contains("The build failed; nothing ran."));
    assert!(prompt.contains("error[E0308]: mismatched types"));

    // Shared with consent: the exchange as sent, with no key in it.
    let posted: Vec<_> = requests(&site)
        .await
        .into_iter()
        .filter(|r| r.url.path().ends_with("hint_exchange"))
        .collect();
    assert_eq!(posted.len(), 1);
    assert!(!raw(&posted[0]).contains(&KEY[..12]));
    let p = &body(&posted[0])["payload"];
    assert_eq!(p["run"], "r4bf0000aa");
    assert_eq!(p["answer"], ANSWER);
    assert_eq!(p["prompt"], prompt.as_str());
    assert_eq!(p["provider"], "openai");
}

#[tokio::test]
async fn a_site_without_hint_exchanges_keeps_it_local_and_succeeds() {
    let site = site(false).await;
    let provider = chat_provider(chat_answer(ANSWER)).await;
    let m = Machine::with_run("build-failed", &site.uri());
    m.setup(
        &site.uri(),
        "openai",
        &format!("{}/v1", provider.uri()),
        &["--share"],
    );
    let out = m.ter(&site.uri(), &["hint", "--llm", "--json"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(json_out(&out)["shared"], "not_on_site");
    let out = m.ter(&site.uri(), &["hint", "--llm"]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("does not take shared hints yet"),
        "{}",
        text(&out)
    );
}

#[tokio::test]
async fn the_messages_format_sends_the_key_in_its_own_header() {
    let site = site(false).await;
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": ANSWER}], "stop_reason": "end_turn"
        })))
        .mount(&provider)
        .await;
    let m = Machine::with_run("check-failed", &site.uri());
    m.setup(
        &site.uri(),
        "anthropic",
        &format!("{}/v1", provider.uri()),
        &["--model", "test-model"],
    );

    let out = m.ter(&site.uri(), &["hint", "--llm", "--json"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(json_out(&out)["text"], ANSWER);
    let r = &requests(&provider).await[0];
    assert_eq!(r.headers.get("x-api-key").unwrap().to_str().unwrap(), KEY);
    assert!(r.headers.get("anthropic-version").is_some());
    assert!(r.headers.get("authorization").is_none());
    let b = body(r);
    assert_eq!(b["model"], "test-model");
    assert!(
        b["system"]
            .as_str()
            .unwrap()
            .contains("Give one hint, never the solution")
    );
}

#[tokio::test]
async fn a_rejected_key_is_named_and_the_providers_echo_of_it_is_cut() {
    let site = site(false).await;
    let echo = format!(
        "Incorrect API key provided: {}****{}. You can find your API key at https://platform.openai.com/account/api-keys.",
        &KEY[..10],
        &KEY[KEY.len() - 4..]
    );
    let full_echo = format!("Invalid key {KEY}");
    for (status, message, code) in [
        (401, echo.as_str(), "llm_key_invalid"),
        (401, full_echo.as_str(), "llm_key_invalid"),
        (429, "You exceeded your current quota", "llm_unavailable"),
        (404, "The model `gpt-5-mini` does not exist", "llm_error"),
    ] {
        let provider = chat_provider(
            ResponseTemplate::new(status)
                .set_body_json(json!({"error": {"message": message, "type": "x"}})),
        )
        .await;
        let m = Machine::with_run("check-failed", &site.uri());
        m.setup(
            &site.uri(),
            "openai",
            &format!("{}/v1", provider.uri()),
            &[],
        );
        let out = m.ter(&site.uri(), &["hint", "--llm", "--json"]);
        assert!(!out.status.success());
        assert_no_key(&out);
        let v = json_out(&out);
        assert_eq!(v["error"]["code"], code, "{v}");
        assert!(!m.exercise.path().join(".ter/llm").exists(), "nothing kept");
    }
}

#[tokio::test]
async fn an_unreachable_provider_is_unavailable() {
    let site = site(false).await;
    let m = Machine::with_run("check-failed", &site.uri());
    m.setup(&site.uri(), "openai", "http://127.0.0.1:9/v1", &[]);
    let out = m.ter(&site.uri(), &["hint", "--llm", "--json"]);
    assert_eq!(json_out(&out)["error"]["code"], "llm_unavailable");
}

#[tokio::test]
async fn nothing_set_up_says_how_and_asks_no_one() {
    let site = site(false).await;
    let m = Machine::with_run("check-failed", &site.uri());
    let out = m.ter(&site.uri(), &["hint", "--llm", "--json"]);
    let v = json_out(&out);
    assert_eq!(v["error"]["code"], "llm_not_configured");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ter llm setup")
    );
    assert!(requests(&site).await.is_empty(), "the site is not asked");

    // A provider but no key.
    m.config("llm_provider = \"openai\"\n");
    let v = json_out(&m.ter(&site.uri(), &["hint", "--llm", "--json"]));
    assert_eq!(v["error"]["code"], "llm_not_configured");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("No openai key")
    );

    // Setup with no key and no terminal says where to get one.
    let out = m.ter(
        &site.uri(),
        &["llm", "setup", "--provider", "gemini", "--json"],
    );
    let v = json_out(&out);
    assert_eq!(v["error"]["code"], "llm_not_configured");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("aistudio.google.com")
    );
}

#[tokio::test]
async fn the_key_can_come_from_the_environment_and_status_never_shows_it() {
    let site = site(false).await;
    let provider = chat_provider(chat_answer(ANSWER)).await;
    let m = Machine::with_run("check-failed", &site.uri());
    m.config(&format!(
        "llm_provider = \"openrouter\"\nllm_base_url = \"{}/v1\"\n",
        provider.uri()
    ));
    let env = [("TER_LLM_KEY", KEY)];
    let out = m.ter_with(&site.uri(), &["hint", "--llm", "--json"], None, &env);
    assert!(out.status.success(), "{}", text(&out));
    assert_no_key(&out);
    assert_eq!(json_out(&out)["model"], "openai/gpt-5-mini");

    let out = m.ter_with(&site.uri(), &["llm", "status", "--json"], None, &env);
    assert_no_key(&out);
    let v = json_out(&out);
    assert_eq!(v["key"], "TER_LLM_KEY");
    assert_eq!(v["provider"], "openrouter");
}

#[tokio::test]
async fn setup_status_and_forget_keep_the_key_private() {
    let site = site(false).await;
    let m = Machine::with_run("check-failed", &site.uri());
    m.setup(
        &site.uri(),
        "gemini",
        "https://example.com/v1",
        &["--no-share"],
    );
    let file = m.config.path().join("llm-key-gemini");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    let config = std::fs::read_to_string(m.config.path().join("config.toml")).unwrap();
    assert!(!config.contains(&KEY[..12]), "{config}");
    assert!(config.contains("llm_share = false"), "{config}");

    let out = m.ter(&site.uri(), &["llm", "status", "--json"]);
    assert_no_key(&out);
    let v = json_out(&out);
    assert_eq!(
        (
            v["provider"].as_str(),
            v["model"].as_str(),
            v["key"].as_str()
        ),
        (Some("gemini"), Some("gemini-flash-latest"), Some("file"))
    );
    assert_eq!(v["share"], false);

    let out = m.ter(&site.uri(), &["llm", "forget", "--json"]);
    assert_eq!(json_out(&out)["removed"], json!(["file"]));
    assert!(!file.exists());
    let v = json_out(&m.ter(&site.uri(), &["llm", "status", "--json"]));
    assert_eq!(v["key"], "none");
}

#[tokio::test]
async fn dry_run_shows_the_prompt_and_sends_nothing() {
    let site = site(true).await;
    let m = Machine::with_run("check-failed", &site.uri());
    let out = m.ter(&site.uri(), &["hint", "--llm", "--dry-run"]);
    assert!(out.status.success(), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Nothing is sent with --dry-run"));
    assert!(stdout.contains("First failing check: flash-width"));
    assert!(
        !requests(&site)
            .await
            .iter()
            .any(|r| r.url.path().ends_with("hint_exchange"))
    );
}

#[tokio::test]
async fn a_run_by_another_account_is_refused_before_the_provider() {
    let site = site(false).await;
    let provider = chat_provider(chat_answer(ANSWER)).await;
    let m = Machine::with_run("check-failed", "https://elsewhere.example.com");
    m.setup(
        &site.uri(),
        "openai",
        &format!("{}/v1", provider.uri()),
        &[],
    );
    let v = json_out(&m.ter(&site.uri(), &["hint", "--llm", "--json"]));
    assert_eq!(v["error"]["code"], "no_run");
    assert!(requests(&provider).await.is_empty());
}
