use serde_json::json;
use std::fs;
use ulearn_mcp::web_client::UlearnWebClient;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string, header, method, path, query_param},
};

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
    let client = UlearnWebClient::new(
        &mock.uri(),
        ".AspNetCore.Identity.Application=synthetic-secret".into(),
    )
    .unwrap();
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
    let client = UlearnWebClient::new(&mock.uri(), "session=synthetic".into()).unwrap();
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
    let client = UlearnWebClient::new(&mock.uri(), "session=private-cookie".into()).unwrap();
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
        let client = UlearnWebClient::new(&mock.uri(), "session=private-cookie".into()).unwrap();
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
    let client = UlearnWebClient::new(&mock.uri(), "session=synthetic".into()).unwrap();
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

#[test]
fn rejects_unsafe_origins_and_header_injection() {
    for url in [
        "http://example.com",
        "https://user:password@example.com",
        "https://example.com/path",
        "https://example.com?x=y",
        "https://example.com#fragment",
    ] {
        assert!(UlearnWebClient::new(url, "session=synthetic".into()).is_err());
    }
    for cookie in [
        "",
        "Cookie: session=secret",
        "session=secret\r\nInjected: x",
        "{\"session\":\"secret\"}",
        "session=secret; HttpOnly",
    ] {
        assert!(UlearnWebClient::new("https://ulearn.me", cookie.into()).is_err());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn private_cookie_file_is_reloaded_and_symlinks_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ulearn.cookie");
    fs::write(&file, "session=old").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let mock = MockServer::start().await;
    let client = UlearnWebClient::from_cookie_file(&mock.uri(), file.clone()).unwrap();
    fs::write(&file, "session=new").unwrap();
    score_request()
        .and(header("cookie", "session=new"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status":"ok","score":3})))
        .expect(1)
        .mount(&mock)
        .await;
    client
        .set_additional_score("course/a & b", "unit-a", "student-a", "activity", Some(3))
        .await
        .unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        client
            .set_additional_score("course/a & b", "unit-a", "student-a", "activity", Some(3))
            .await
            .is_err()
    );
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let link = dir.path().join("link.cookie");
    symlink(&file, &link).unwrap();
    assert!(UlearnWebClient::from_cookie_file(&mock.uri(), link).is_err());
    assert_eq!(mock.received_requests().await.unwrap().len(), 1);
}
