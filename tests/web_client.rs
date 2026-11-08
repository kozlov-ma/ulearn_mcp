use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::json;
use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use ulearn_mcp::{client::UlearnClient, session::UlearnSession, web_client::UlearnWebClient};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string, header, method, path, query_param},
};

fn jwt() -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 86400;
    format!(
        "e30.{}.signature",
        URL_SAFE_NO_PAD.encode(json!({"exp":exp}).to_string())
    )
}

async fn auth_mocks(api: &MockServer, cookie: &str) {
    let token = jwt();
    Mock::given(method("POST"))
        .and(path("/account/token"))
        .and(header("cookie", cookie))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token":token})))
        .expect(1)
        .mount(api)
        .await;
    Mock::given(method("GET"))
        .and(path("/account"))
        .and(header("authorization", format!("Bearer {token}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"isAuthenticated":true,"user":{"id":"teacher"}})),
        )
        .expect(1)
        .mount(api)
        .await;
}

async fn authenticated_client(web: &MockServer, cookie: &str) -> UlearnWebClient {
    let api = MockServer::start().await;
    auth_mocks(&api, cookie).await;
    let session = UlearnSession::from_cookie(&api.uri(), &web.uri(), cookie.into()).unwrap();
    let client = UlearnClient::from_session(session)
        .unwrap()
        .pin_session()
        .await
        .unwrap();
    UlearnWebClient::from_session(client.session_snapshot().unwrap()).unwrap()
}

fn save_cookie(path: &Path, cookie: &str) {
    fs::write(path, cookie).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn score_request() -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path("/Admin/SetAdditionalScore"))
        .and(query_param("courseId", "course/a & b"))
        .and(query_param("unitId", "unit-a"))
        .and(query_param("userId", "student-a"))
        .and(query_param("scoringGroupId", "activity"))
}

#[tokio::test]
async fn exact_form_request_uses_cookie_and_returns_only_receipt() {
    let mock = MockServer::start().await;
    score_request()
        .and(header(
            "cookie",
            ".AspNetCore.Identity.Application=synthetic-secret",
        ))
        .and(header("content-type", "application/x-www-form-urlencoded"))
        .and(body_string("score=3"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"status":"ok","score":3,"private":"do-not-return"})),
        )
        .expect(1)
        .mount(&mock)
        .await;
    let client =
        authenticated_client(&mock, ".AspNetCore.Identity.Application=synthetic-secret").await;
    let result = client
        .set_additional_score("course/a & b", "unit-a", "student-a", "activity", Some(3))
        .await
        .unwrap();
    assert_eq!(result, json!({"status":"ok","score":3}));
    assert!(
        !mock.received_requests().await.unwrap()[0]
            .headers
            .contains_key("authorization")
    );
}

#[tokio::test]
async fn clearing_and_zero_are_different_form_values() {
    let mock = MockServer::start().await;
    score_request()
        .and(body_string("score="))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status":"ok","score":""})))
        .expect(1)
        .mount(&mock)
        .await;
    score_request()
        .and(body_string("score=0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status":"ok","score":0})))
        .expect(1)
        .mount(&mock)
        .await;
    let client = authenticated_client(&mock, "session=synthetic").await;
    client
        .set_additional_score("course/a & b", "unit-a", "student-a", "activity", None)
        .await
        .unwrap();
    client
        .set_additional_score("course/a & b", "unit-a", "student-a", "activity", Some(0))
        .await
        .unwrap();
}

#[tokio::test]
async fn redirects_are_not_followed_or_retried() {
    let mock = MockServer::start().await;
    let destination = MockServer::start().await;
    score_request()
        .respond_with(ResponseTemplate::new(307).insert_header("location", destination.uri()))
        .expect(1)
        .mount(&mock)
        .await;
    let client = authenticated_client(&mock, "session=private-cookie").await;
    let error = client
        .set_additional_score("course/a & b", "unit-a", "student-a", "activity", Some(3))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Redirect was not followed"));
    assert!(!error.contains("private-cookie"));
    assert!(destination.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn failed_or_malformed_responses_are_redacted_and_never_replayed() {
    for response in [
        ResponseTemplate::new(500).set_body_string("private-cookie private-student-data"),
        ResponseTemplate::new(200).set_body_string("private-cookie private-student-data"),
        ResponseTemplate::new(200)
            .set_body_json(json!({"status":"error","error":"private-cookie private-student-data"})),
        ResponseTemplate::new(200).set_body_json(json!({"status":"ok","score":99})),
    ] {
        let mock = MockServer::start().await;
        score_request()
            .respond_with(response)
            .expect(1)
            .mount(&mock)
            .await;
        let client = authenticated_client(&mock, "session=private-cookie").await;
        let error = client
            .set_additional_score("course/a & b", "unit-a", "student-a", "activity", Some(3))
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("private-cookie"));
        assert!(!error.contains("private-student-data"));
        assert_eq!(mock.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn invalid_targets_and_negative_scores_do_not_send_requests() {
    let mock = MockServer::start().await;
    let client = authenticated_client(&mock, "session=synthetic").await;
    assert!(
        client
            .set_additional_score("", "unit-a", "student-a", "activity", Some(3))
            .await
            .is_err()
    );
    assert!(
        client
            .set_additional_score("course", "unit-a", "student-a", "activity", Some(-1))
            .await
            .is_err()
    );
    assert!(mock.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn replacing_session_after_pinning_cannot_switch_mutation_account() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.cookie");
    save_cookie(&file, "session=original");
    let api = MockServer::start().await;
    let web = MockServer::start().await;
    auth_mocks(&api, "session=original").await;
    let session = UlearnSession::from_file(&api.uri(), &web.uri(), file.clone()).unwrap();
    let pinned = UlearnClient::from_session(session)
        .unwrap()
        .pin_session()
        .await
        .unwrap();
    let client = UlearnWebClient::from_session(pinned.session_snapshot().unwrap()).unwrap();
    save_cookie(&file, "session=replacement");
    let error = client
        .set_additional_score("course", "unit-a", "student-a", "activity", Some(3))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.to_lowercase().contains("session"));
    assert!(!error.contains("original"));
    assert!(!error.contains("replacement"));
    assert!(web.received_requests().await.unwrap().is_empty());
}
