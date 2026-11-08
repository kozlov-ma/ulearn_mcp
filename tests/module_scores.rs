use rmcp::handler::server::wrapper::Parameters;
use serde_json::{Value, json};
use ulearn_mcp::{
    client::UlearnClient,
    server::{ModuleScoresArgs, UlearnServer},
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, method, path, query_param},
};

fn course() -> Value {
    json!({"scoring":{"groups":[{"id":"activity","name":"Activity","weight":1},{"id":"exercise","name":"Exercises","weight":2},{"id":"selfcheck","name":"Self-check","weight":1}]},"units":[
        {"id":"unit-a","title":"Logic","slides":[
            {"id":"task-a","title":"AND","scoringGroup":"exercise","maxScore":10},
            {"id":"task-b","title":"OR","scoringGroup":"exercise","maxScore":15},
            {"id":"quiz-a","title":"Quiz","scoringGroup":"selfcheck","maxScore":1}],
        "additionalScores":[{"id":"activity","maxAdditionalScore":4,"canInstructorSetAdditionalScore":true},{"id":"exercise","maxAdditionalScore":5,"canInstructorSetAdditionalScore":true}]},
        {"id":"unit-b","title":"Memory","slides":[],"additionalScores":[{"id":"activity","maxAdditionalScore":2,"canInstructorSetAdditionalScore":true}]}
    ]})
}
fn progress(score: Option<i32>) -> Value {
    let mut result = json!({"userProgress":{"student-a":{"visitedSlides":{"task-a":{"score":7,"visited":true},"quiz-a":{"score":1,"visited":true}},"additionalScores":{"unit-a":{"exercise":2},"unit-b":{"activity":1}}},"student-b":{"visitedSlides":{},"additionalScores":{"unit-a":{"activity":4}}}}});
    if let Some(score) = score {
        result["userProgress"]["student-a"]["additionalScores"]["unit-a"]["activity"] =
            json!(score);
    }
    result
}
fn args() -> ModuleScoresArgs {
    ModuleScoresArgs {
        course_id: "course-a".into(),
        user_id: "student-a".into(),
        unit_id: Some("unit-a".into()),
        group_id: None,
    }
}
fn server(api: &MockServer) -> UlearnServer {
    UlearnServer::new(UlearnClient::new(&api.uri(), Some("synthetic-token".into())).unwrap())
}
async fn course_mock(api: &MockServer, data: Value) {
    Mock::given(method("GET"))
        .and(path("/courses/course-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(data))
        .expect(1)
        .mount(api)
        .await;
}
async fn progress_mock(api: &MockServer, data: Value) {
    Mock::given(method("POST"))
        .and(path("/user-progress/course-a"))
        .and(body_json(json!({"userIds":["student-a"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(data))
        .expect(1)
        .mount(api)
        .await;
}
fn category<'a>(result: &'a Value, id: &str) -> &'a Value {
    result["units"][0]["scoring_groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == id)
        .unwrap()
}

#[tokio::test]
async fn module_read_uses_exact_user_and_keeps_scoring_categories_separate() {
    let api = MockServer::start().await;
    course_mock(&api, course()).await;
    progress_mock(&api, progress(None)).await;
    let result = server(&api)
        .get_module_scores(Parameters(args()))
        .await
        .unwrap()
        .0;
    assert_eq!(result["user_id"], "student-a");
    assert_eq!(result["units"].as_array().unwrap().len(), 1);
    assert_eq!(result["units"][0]["id"], "unit-a");
    assert_eq!(
        category(&result, "activity")["additional_score"],
        Value::Null
    );
    assert_eq!(category(&result, "activity")["max_score"], 4);
    assert_eq!(category(&result, "exercise")["slide_score"], 7);
    assert_eq!(category(&result, "exercise")["score"], 9);
    assert_eq!(category(&result, "exercise")["max_score"], 30);
    assert_eq!(category(&result, "exercise")["metadata"]["weight"], 2);
    assert_eq!(category(&result, "selfcheck")["score"], 1);
    assert!(result.get("total_score").is_none());
    assert!(result["units"][0]["slides"][1]["progress"].is_null());
    assert_eq!(api.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn all_modules_and_group_visibility_are_supported() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/courses/course-a"))
        .and(query_param("groupId", "42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(course()))
        .expect(1)
        .mount(&api)
        .await;
    progress_mock(&api, progress(Some(0))).await;
    let mut request = args();
    request.unit_id = None;
    request.group_id = Some(42);
    let result = server(&api)
        .get_module_scores(Parameters(request))
        .await
        .unwrap()
        .0;
    assert_eq!(result["group_id"], 42);
    assert_eq!(result["units"].as_array().unwrap().len(), 2);
    assert_eq!(category(&result, "activity")["additional_score"], 0);
    assert_eq!(result["units"][1]["additional_scores"]["activity"], 1);
}

#[tokio::test]
async fn unavailable_or_incomplete_student_progress_is_not_reported_as_zero() {
    for data in [
        json!({"userProgress":{}}),
        json!({"userProgress":{"student-b":{}}}),
        json!({"userProgress":{"student-a":{}}}),
        json!({"userProgress":{"student-a":{"visitedSlides":{}}}}),
    ] {
        let api = MockServer::start().await;
        course_mock(&api, course()).await;
        progress_mock(&api, data).await;
        assert!(
            server(&api)
                .get_module_scores(Parameters(args()))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn exact_module_must_be_present_and_unambiguous() {
    for duplicate in [false, true] {
        let api = MockServer::start().await;
        let mut data = course();
        if duplicate {
            data["units"][1]["id"] = json!("unit-a");
        } else {
            data["units"][0]["id"] = json!("other-unit");
        }
        course_mock(&api, data).await;
        progress_mock(&api, progress(Some(1))).await;
        let error = server(&api)
            .get_module_scores(Parameters(args()))
            .await
            .err()
            .unwrap();
        assert!(error.contains("Exact module not found or ambiguous"));
    }
}

#[tokio::test]
async fn nullable_unscored_course_fields_are_readable() {
    let api = MockServer::start().await;
    let data = json!({"units":[{"id":"unit-a","title":"Introduction","additionalScores":null,
        "slides":[{"id":"lesson-a","title":"Welcome","maxScore":0,"scoringGroup":null}]}]});
    course_mock(&api, data).await;
    progress_mock(
        &api,
        json!({"userProgress":{"student-a":{"visitedSlides":null,"additionalScores":null}}}),
    )
    .await;
    let result = server(&api)
        .get_module_scores(Parameters(args()))
        .await
        .unwrap()
        .0;
    assert!(
        result["units"][0]["scoring_groups"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(result["units"][0]["slides"][0]["max_score"], 0);
    assert_eq!(result["units"][0]["slides"][0]["scoring_group_id"], "");
}

#[tokio::test]
async fn malformed_scoring_fields_are_rejected() {
    for case in 0..7 {
        let api = MockServer::start().await;
        let mut data = course();
        let mut scores = progress(Some(1));
        match case {
            0 => data["units"][0]["slides"][0]["scoringGroup"] = Value::Null,
            1 => data["units"][0]["slides"][0]["scoringGroup"] = json!(true),
            2 => data["units"][0]["additionalScores"] = json!({}),
            3 => {
                data["units"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("additionalScores");
            }
            4 => {
                scores["userProgress"]["student-a"]["additionalScores"]["unit-a"]["activity"] =
                    json!("1")
            }
            5 => {
                scores["userProgress"]["student-a"]["visitedSlides"]["task-a"]["score"] = json!("7")
            }
            _ => scores["userProgress"]["student-a"]["additionalScores"] = json!([]),
        }
        course_mock(&api, data).await;
        progress_mock(&api, scores).await;
        assert!(
            server(&api)
                .get_module_scores(Parameters(args()))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn empty_identifiers_are_rejected_without_requests() {
    let api = MockServer::start().await;
    for case in 0..3 {
        let mut request = args();
        match case {
            0 => request.course_id = " ".into(),
            1 => request.user_id = "".into(),
            _ => request.unit_id = Some(" ".into()),
        }
        assert!(
            server(&api)
                .get_module_scores(Parameters(request))
                .await
                .is_err()
        );
    }
    assert!(api.received_requests().await.unwrap().is_empty());
}

fn session_jwt() -> String {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 86400;
    format!(
        "e30.{}.signature",
        URL_SAFE_NO_PAD.encode(json!({"exp":exp}).to_string())
    )
}

async fn session_auth(api: &MockServer) {
    use wiremock::matchers::header;
    let token = session_jwt();
    Mock::given(method("POST"))
        .and(path("/account/token"))
        .and(header("cookie", "session=teacher"))
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

async fn session_server(api: &MockServer, web: &MockServer) -> UlearnServer {
    session_auth(api).await;
    let session = ulearn_mcp::session::UlearnSession::from_cookie(
        &api.uri(),
        &web.uri(),
        "session=teacher".into(),
    )
    .unwrap();
    UlearnServer::new(UlearnClient::from_session(session).unwrap())
}

fn write_args() -> ulearn_mcp::server::ModuleScoreArgs {
    ulearn_mcp::server::ModuleScoreArgs {
        course_id: "course-a".into(),
        unit_id: "unit-a".into(),
        user_id: "student-a".into(),
        scoring_group_id: "activity".into(),
        score: Some(3),
        expected_score: Some(1),
        group_id: None,
    }
}

async fn repeated_course(api: &MockServer, times: u64) {
    Mock::given(method("GET"))
        .and(path("/courses/course-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(course()))
        .expect(times)
        .mount(api)
        .await;
}

async fn score_mutation(web: &MockServer, score: Option<i32>) {
    use wiremock::matchers::{body_string, header};
    Mock::given(method("POST"))
        .and(path("/Admin/SetAdditionalScore"))
        .and(header("cookie", "session=teacher"))
        .and(query_param("courseId", "course-a"))
        .and(query_param("unitId", "unit-a"))
        .and(query_param("userId", "student-a"))
        .and(query_param("scoringGroupId", "activity"))
        .and(body_string(format!(
            "score={}",
            score.map(|n| n.to_string()).unwrap_or_default()
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"status":"ok","score":score.map(|n|json!(n)).unwrap_or(json!(""))}),
        ))
        .expect(1)
        .mount(web)
        .await;
}

#[tokio::test]
async fn token_mode_cannot_edit_module_scores_or_silently_load_another_cookie() {
    let api = MockServer::start().await;
    let error = server(&api)
        .set_module_score(Parameters(write_args()))
        .await
        .err()
        .unwrap();
    assert!(error.contains("require session authentication"));
    assert!(api.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn scores_and_clear_use_one_session_for_api_preflight_write_and_readback() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    for score in [Some(0), Some(3), None] {
        let api = MockServer::start().await;
        let web = MockServer::start().await;
        let server = session_server(&api, &web).await;
        repeated_course(&api, 2).await;
        let count = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/user-progress/course-a"))
            .and(body_json(json!({"userIds":["student-a"]})))
            .respond_with(move |_: &wiremock::Request| {
                ResponseTemplate::new(200).set_body_json(progress(
                    if count.fetch_add(1, Ordering::SeqCst) == 0 {
                        Some(1)
                    } else {
                        score
                    },
                ))
            })
            .expect(2)
            .mount(&api)
            .await;
        score_mutation(&web, score).await;
        let mut request = write_args();
        request.score = score;
        let result = server
            .set_module_score(Parameters(request))
            .await
            .unwrap()
            .0;
        assert_eq!(result["write_accepted"], true);
        assert_eq!(result["score_verified"], true);
        let api_requests = api.received_requests().await.unwrap();
        let account = api_requests
            .iter()
            .find(|r| r.url.path() == "/account")
            .unwrap();
        for request in api_requests
            .iter()
            .filter(|r| r.url.path() != "/account/token")
        {
            assert!(!request.headers.contains_key("cookie"));
            assert_eq!(
                request.headers.get("authorization"),
                account.headers.get("authorization")
            );
        }
        let web_requests = web.received_requests().await.unwrap();
        assert_eq!(web_requests.len(), 1);
        assert!(!web_requests[0].headers.contains_key("authorization"));
    }
}

#[tokio::test]
async fn stale_out_of_range_disabled_and_malformed_scores_stop_before_mutation() {
    for case in 0..4 {
        let api = MockServer::start().await;
        let web = MockServer::start().await;
        let server = session_server(&api, &web).await;
        let mut metadata = course();
        let mut data = progress(Some(1));
        let mut request = write_args();
        match case {
            0 => request.expected_score = Some(2),
            1 => request.score = Some(5),
            2 => {
                metadata["units"][0]["additionalScores"][0]["canInstructorSetAdditionalScore"] =
                    json!(false)
            }
            _ => {
                data["userProgress"]["student-a"]["additionalScores"]["unit-a"]["activity"] =
                    json!("1")
            }
        }
        course_mock(&api, metadata).await;
        progress_mock(&api, data).await;
        assert!(server.set_module_score(Parameters(request)).await.is_err());
        assert!(web.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn unchanged_module_score_is_a_verified_no_op() {
    let api = MockServer::start().await;
    let web = MockServer::start().await;
    let server = session_server(&api, &web).await;
    course_mock(&api, course()).await;
    progress_mock(&api, progress(Some(1))).await;
    let mut request = write_args();
    request.score = Some(1);
    let result = server
        .set_module_score(Parameters(request))
        .await
        .unwrap()
        .0;
    assert_eq!(result["changed"], false);
    assert_eq!(result["score_verified"], true);
    assert!(web.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn failed_readback_preserves_accepted_write_and_never_retries_mutation() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let api = MockServer::start().await;
    let web = MockServer::start().await;
    let server = session_server(&api, &web).await;
    repeated_course(&api, 2).await;
    let count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/user-progress/course-a"))
        .respond_with(move |_: &wiremock::Request| {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(200).set_body_json(progress(Some(1)))
            } else {
                ResponseTemplate::new(503)
            }
        })
        .expect(2)
        .mount(&api)
        .await;
    score_mutation(&web, Some(3)).await;
    let result = server
        .set_module_score(Parameters(write_args()))
        .await
        .unwrap()
        .0;
    assert_eq!(result["write_accepted"], true);
    assert_eq!(result["score_verified"], false);
    assert!(result["readback_error"].is_string());
    assert_eq!(web.received_requests().await.unwrap().len(), 1);
}

fn save_session_file(path: &std::path::Path, content: &str) {
    std::fs::write(path, content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[tokio::test]
async fn session_replacement_during_module_preflight_prevents_website_write() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.cookie");
    save_session_file(&file, "session=teacher");
    let api = MockServer::start().await;
    let web = MockServer::start().await;
    session_auth(&api).await;
    let session =
        ulearn_mcp::session::UlearnSession::from_file(&api.uri(), &web.uri(), file.clone())
            .unwrap();
    let server = UlearnServer::new(UlearnClient::from_session(session).unwrap());
    course_mock(&api, course()).await;
    Mock::given(method("POST"))
        .and(path("/user-progress/course-a"))
        .respond_with(move |_: &wiremock::Request| {
            save_session_file(&file, "session=someone-else");
            ResponseTemplate::new(200).set_body_json(progress(Some(1)))
        })
        .expect(1)
        .mount(&api)
        .await;
    assert!(
        server
            .set_module_score(Parameters(write_args()))
            .await
            .is_err()
    );
    assert!(web.received_requests().await.unwrap().is_empty());
    assert_eq!(api.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn review_mutation_also_pins_session_across_its_preflight() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("session.cookie");
    save_session_file(&file, "session=teacher");
    let api = MockServer::start().await;
    let web = MockServer::start().await;
    session_auth(&api).await;
    let session =
        ulearn_mcp::session::UlearnSession::from_file(&api.uri(), &web.uri(), file.clone())
            .unwrap();
    let server = UlearnServer::new(UlearnClient::from_session(session).unwrap());
    Mock::given(method("GET"))
        .and(path("/submissions"))
        .respond_with(move |_: &wiremock::Request| {
            save_session_file(&file, "session=someone-else");
            ResponseTemplate::new(200).set_body_json(
                json!({"submissions":[{"id":1,"code":"code","manualChecking":{"percent":null}}]}),
            )
        })
        .expect(1)
        .mount(&api)
        .await;
    let target = ulearn_mcp::server::ReviewTarget {
        course_id: "course-a".into(),
        slide_id: "task-a".into(),
        user_id: "student-a".into(),
        submission_id: 1,
        expected_code_sha256: ulearn_mcp::server::code_hash("code"),
        expected_percent: None,
    };
    let result = server
        .set_review_score(Parameters(ulearn_mcp::server::ScoreArgs {
            target,
            percent: 75,
        }))
        .await;
    assert!(result.is_err());
    assert_eq!(api.received_requests().await.unwrap().len(), 3);
}

#[test]
fn module_grade_precondition_is_required_and_clear_is_explicit() {
    let incomplete = json!({"course_id":"course-a","unit_id":"unit-a","user_id":"student-a","scoring_group_id":"activity","score":null});
    assert!(serde_json::from_value::<ulearn_mcp::server::ModuleScoreArgs>(incomplete).is_err());
    let mut complete = json!({"course_id":"course-a","unit_id":"unit-a","user_id":"student-a","scoring_group_id":"activity","score":null,"expected_score":0});
    assert!(
        serde_json::from_value::<ulearn_mcp::server::ModuleScoreArgs>(complete.clone())
            .unwrap()
            .score
            .is_none()
    );
    complete.as_object_mut().unwrap().remove("score");
    assert!(serde_json::from_value::<ulearn_mcp::server::ModuleScoreArgs>(complete).is_err());
}
