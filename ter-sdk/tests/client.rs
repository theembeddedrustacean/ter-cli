//! The client against a local mock of the site.

use serde_json::{Value, json};
use ter_sdk::{Client, Error};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API: &str = "/api/method/ter_courses.api";

fn ping_body(min: &str) -> Value {
    json!({"message": {
        "ok": true,
        "user": "learner@example.com",
        "server_time": "2026-09-27T12:00:00",
        "min_supported_version": min,
        "latest_version": "0.2.0",
        "download_url": null
    }})
}

async fn site_with_min(min: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API}.ping")))
        .and(header("authorization", "Bearer good-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ping_body(min)))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn below_minimum_refuses_to_post_and_names_self_update() {
    let server = site_with_min("0.2.0").await;
    Mock::given(method("POST"))
        .and(path(format!("{API}.run")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"message": {}})))
        .expect(0)
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), Some("good-token".into()), "0.1.0").unwrap();
    let err = client
        .post::<Value>("run", &json!({"exercise_id": "x"}))
        .await
        .unwrap_err();

    assert_eq!(err.code(), "outdated");
    assert!(err.to_string().contains("ter self-update"), "{err}");
    // `expect(0)` on the run mock is verified when the server drops.
}

#[tokio::test]
async fn at_minimum_posts() {
    let server = site_with_min("0.1.0").await;
    Mock::given(method("POST"))
        .and(path(format!("{API}.run")))
        .and(header("user-agent", "ter-cli/0.1.0"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"message": {"name": "RUN-1"}})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), Some("good-token".into()), "0.1.0").unwrap();
    let v: Value = client
        .post("run", &json!({"exercise_id": "x"}))
        .await
        .unwrap();
    assert_eq!(v["name"], "RUN-1");
}

#[tokio::test]
async fn ping_is_fetched_once_per_client() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API}.ping")))
        .respond_with(ResponseTemplate::new(200).set_body_json(ping_body("0.1.0")))
        .expect(1)
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), Some("t".into()), "0.1.0").unwrap();
    client.ensure_supported().await.unwrap();
    assert_eq!(client.ping().await.unwrap().user, "learner@example.com");
}

#[tokio::test]
async fn bad_token_is_token_invalid() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API}.ping")))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"message": {"error": {
                "code": "token_invalid",
                "message": "No valid device token on this request."
            }}})),
        )
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), Some("bad".into()), "0.1.0").unwrap();
    let err = client.ping().await.unwrap_err();
    assert_eq!(err.code(), "token_invalid");
    assert_eq!(err.to_string(), "No valid device token on this request.");
}

#[tokio::test]
async fn no_token_fails_before_any_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), None, "0.1.0").unwrap();
    assert_eq!(client.ping().await.unwrap_err(), Error::NoToken);
}

#[tokio::test]
async fn unreachable_site_is_a_network_error() {
    // Port 9 (discard) on localhost is not listening.
    let client = Client::new("http://127.0.0.1:9", Some("t".into()), "0.1.0").unwrap();
    assert_eq!(client.ping().await.unwrap_err().code(), "network_error");
}

#[tokio::test]
async fn wrong_shape_names_the_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API}.enrollments")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"message": {"courses": "nope"}})),
        )
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), Some("t".into()), "0.1.0").unwrap();
    let err = client.enrollments().await.unwrap_err();
    assert_eq!(err.code(), "server_error");
    assert!(err.to_string().contains("`enrollments`"), "{err}");
}

#[tokio::test]
async fn lesson_exercise_may_be_an_object() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API}.enrollments")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"message": {"courses": [{
                "course_id": "c", "title": "C", "lessons": [
                    {"lesson_id": "l1", "title": "L1", "completed": false, "locked": false,
                     "exercise": null, "exercises": []},
                    {"lesson_id": "l2", "title": "L2", "completed": true, "locked": false,
                     "exercise": {"exercise_id": "gpio-blinky", "runner": "cargo"},
                     "exercises": [{"exercise_id": "gpio-blinky"}]}
                ]
            }]}})),
        )
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), Some("t".into()), "0.1.0").unwrap();
    let e = client.enrollments().await.unwrap();
    assert_eq!(e.courses[0].lessons.len(), 2);
}
