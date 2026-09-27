use serde_json::Value;

use crate::Error;

/// Turn a site response into its value or an [`Error`].
///
/// Frappe wraps a successful answer as `{"message": <value>}`. The TER API
/// reports failures as `{"error": {"code", "message"}}`, which Frappe in
/// turn wraps in `message`; both forms are accepted. Anything else that is
/// not a success (a body with `exc` or `exc_type`, an HTML error page) is a
/// server error, except Frappe's own 401 `AuthenticationError`, which
/// means the same to the learner as `token_invalid`. That is how the site
/// reports a token it cannot parse, and also a revoked or expired one, with
/// the text (`Token revoked.`) in `_server_messages`.
pub fn parse_response(http_status: u16, body: &[u8]) -> Result<Value, Error> {
    let server = |exc_type: Option<String>| Error::Server {
        http_status,
        exc_type,
    };

    let Ok(json) = serde_json::from_slice::<Value>(body) else {
        return Err(match http_status {
            401 => token_rejected(None),
            _ => server(None),
        });
    };

    let envelope = json
        .get("message")
        .and_then(|m| m.get("error"))
        .or_else(|| json.get("error"));
    if let Some(err) = envelope
        && let Some(code) = err.get("code").and_then(Value::as_str)
    {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(code)
            .to_string();
        return Err(Error::Site {
            code: code.to_string(),
            message,
            http_status,
        });
    }

    let exc_type = json
        .get("exc_type")
        .and_then(Value::as_str)
        .map(str::to_string);
    let success = (200..300).contains(&http_status);
    if !success || exc_type.is_some() || json.get("exc").is_some() {
        if http_status == 401 {
            return Err(token_rejected(server_message(&json)));
        }
        return Err(server(exc_type));
    }

    match json {
        Value::Object(mut map) => Ok(map.remove("message").unwrap_or(Value::Null)),
        _ => Err(server(None)),
    }
}

fn token_rejected(message: Option<String>) -> Error {
    Error::Site {
        code: "token_invalid".into(),
        message: message.unwrap_or_else(|| "The site did not accept this device token.".into()),
        http_status: 401,
    }
}

/// The first message Frappe attached to an exception. `_server_messages` is
/// a JSON string holding a list of JSON strings, each an object with a
/// `message`.
fn server_message(json: &Value) -> Option<String> {
    let list: Vec<String> = serde_json::from_str(json.get("_server_messages")?.as_str()?).ok()?;
    list.iter().find_map(|item| {
        let item: Value = serde_json::from_str(item).ok()?;
        let text = item.get("message")?.as_str()?.trim();
        (!text.is_empty()).then(|| text.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn site_error(status: u16, body: Value) -> (String, String, u16) {
        match parse_response(status, body.to_string().as_bytes()) {
            Err(Error::Site {
                code,
                message,
                http_status,
            }) => (code, message, http_status),
            other => panic!("expected a site error, got {other:?}"),
        }
    }

    const CODES: &[(&str, u16)] = &[
        ("token_invalid", 401),
        ("not_enrolled", 403),
        ("locked", 403),
        ("mode_not_allowed", 403),
        ("not_found", 404),
        ("rate_limited", 429),
        ("server_error", 500),
    ];

    #[test]
    fn every_site_code_wrapped_in_message() {
        for &(code, status) in CODES {
            let body = json!({"message": {"error": {"code": code, "message": format!("text for {code}")}}});
            let (c, m, s) = site_error(status, body);
            assert_eq!(c, code);
            assert_eq!(m, format!("text for {code}"));
            assert_eq!(s, status);
        }
    }

    #[test]
    fn every_site_code_at_top_level() {
        for &(code, status) in CODES {
            let body = json!({"error": {"code": code, "message": "m"}});
            let (c, _, s) = site_error(status, body);
            assert_eq!((c.as_str(), s), (code, status));
        }
    }

    #[test]
    fn site_error_code_is_the_exit_code() {
        let body = json!({"message": {"error": {"code": "locked", "message": "Finish the previous exercise in this course first."}}});
        let err = parse_response(403, body.to_string().as_bytes()).unwrap_err();
        assert_eq!(err.code(), "locked");
        assert_eq!(
            err.to_string(),
            "Finish the previous exercise in this course first."
        );
    }

    #[test]
    fn unknown_site_code_passes_through() {
        let body = json!({"message": {"error": {"code": "something_new", "message": "New."}}});
        assert_eq!(site_error(500, body).0, "something_new");
    }

    #[test]
    fn frappe_500_with_exc_is_a_server_error() {
        let body = json!({"exc_type": "TypeError", "exc": "[\"Traceback ...\"]"});
        let err = parse_response(500, body.to_string().as_bytes()).unwrap_err();
        assert_eq!(
            err,
            Error::Server {
                http_status: 500,
                exc_type: Some("TypeError".into())
            }
        );
        assert_eq!(err.code(), "server_error");
        assert!(err.to_string().contains("TypeError"));
    }

    #[test]
    fn exc_on_http_200_is_still_a_server_error() {
        let body = json!({"exc": "boom"});
        assert_eq!(
            parse_response(200, body.to_string().as_bytes())
                .unwrap_err()
                .code(),
            "server_error"
        );
    }

    #[test]
    fn frappe_authentication_error_is_token_invalid() {
        let body = json!({"exc_type": "AuthenticationError"});
        let (code, _, status) = site_error(401, body);
        assert_eq!((code.as_str(), status), ("token_invalid", 401));
    }

    /// As the live site sent it for a device revoked on the Devices page.
    fn frappe_auth_error(message: &str) -> Value {
        let inner = json!({
            "message": message,
            "as_table": false,
            "title": "Message",
            "indicator": "red",
            "raise_exception": 1,
            "__frappe_exc_id": "a41eb984"
        });
        let list = serde_json::to_string(&vec![inner.to_string()]).unwrap();
        json!({"exc_type": "AuthenticationError", "_server_messages": list})
    }

    #[test]
    fn revoked_and_expired_tokens_keep_the_site_text() {
        for message in ["Token revoked.", "Token expired."] {
            let (code, m, status) = site_error(401, frappe_auth_error(message));
            assert_eq!(
                (code.as_str(), m.as_str(), status),
                ("token_invalid", message, 401)
            );
        }
    }

    #[test]
    fn unreadable_server_messages_fall_back_to_the_generic_text() {
        for body in [
            json!({"exc_type": "AuthenticationError", "_server_messages": "not json"}),
            json!({"exc_type": "AuthenticationError", "_server_messages": "[\"{}\"]"}),
            frappe_auth_error("  "),
        ] {
            let (code, m, _) = site_error(401, body);
            assert_eq!(code, "token_invalid");
            assert_eq!(m, "The site did not accept this device token.");
        }
    }

    #[test]
    fn html_error_page_is_a_server_error() {
        let err = parse_response(502, b"<html>Bad Gateway</html>").unwrap_err();
        assert_eq!(
            err,
            Error::Server {
                http_status: 502,
                exc_type: None
            }
        );
    }

    #[test]
    fn success_unwraps_message() {
        let body = json!({"message": {"ok": true, "user": "a@b.c"}});
        let v = parse_response(200, body.to_string().as_bytes()).unwrap();
        assert_eq!(v, json!({"ok": true, "user": "a@b.c"}));
    }
}
