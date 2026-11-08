use rmcp::handler::server::wrapper::Parameters;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use ulearn_mcp::{
    client::UlearnClient,
    server::{ModuleScoreArgs, ModuleScoresArgs, UlearnServer},
    web_client::UlearnWebClient,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, body_string, method, path, query_param},
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
fn read_args() -> ModuleScoresArgs {
    ModuleScoresArgs {
        course_id: "course-a".into(),
        user_id: "student-a".into(),
        unit_id: Some("unit-a".into()),
        group_id: None,
    }
}
fn write_args() -> ModuleScoreArgs {
    ModuleScoreArgs {
        course_id: "course-a".into(),
        unit_id: "unit-a".into(),
        user_id: "student-a".into(),
        scoring_group_id: "activity".into(),
        score: Some(3),
        expected_score: Some(1),
        group_id: None,
    }
}
fn server(api: &MockServer, web: &MockServer) -> UlearnServer {
    UlearnServer::new(UlearnClient::new(&api.uri(), Some("synthetic-token".into())).unwrap())
        .with_web_client(
            UlearnWebClient::new(&web.uri(), "session=synthetic-cookie".into()).unwrap(),
        )
}
async fn course_mock(api: &MockServer, data: Value, times: u64) {
    Mock::given(method("GET"))
        .and(path("/courses/course-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(data))
        .expect(times)
        .mount(api)
        .await;
}
async fn progress_mock(api: &MockServer, data: Value, times: u64) {
    Mock::given(method("POST"))
        .and(path("/user-progress/course-a"))
        .and(body_json(json!({"userIds":["student-a"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(data))
        .expect(times)
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
async fn score_mutation(web: &MockServer, score: Option<i32>) {
    let body = format!("score={}", score.map(|v| v.to_string()).unwrap_or_default());
    Mock::given(method("POST"))
        .and(path("/Admin/SetAdditionalScore"))
        .and(query_param("courseId", "course-a"))
        .and(query_param("unitId", "unit-a"))
        .and(query_param("userId", "student-a"))
        .and(query_param("scoringGroupId", "activity"))
        .and(body_string(body))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"status":"ok","score":score.map(|v|json!(v)).unwrap_or(json!(""))}),
        ))
        .expect(1)
        .mount(web)
        .await;
}

#[tokio::test]
async fn module_read_uses_exact_user_and_keeps_scoring_categories_separate() {
    let api = MockServer::start().await;
    let web = MockServer::start().await;
    course_mock(&api, course(), 1).await;
    progress_mock(&api, progress(None), 1).await;
    let result = server(&api, &web)
        .get_module_scores(Parameters(read_args()))
        .await
        .unwrap()
        .0;
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
    assert_eq!(category(&result, "selfcheck")["score"], 1);
    assert!(result.get("total_score").is_none());
    assert!(web.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn all_modules_and_group_visibility_are_supported() {
    let api = MockServer::start().await;
    let web = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/courses/course-a"))
        .and(query_param("groupId", "42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(course()))
        .expect(1)
        .mount(&api)
        .await;
    progress_mock(&api, progress(Some(0)), 1).await;
    let mut args = read_args();
    args.unit_id = None;
    args.group_id = Some(42);
    let result = server(&api, &web)
        .get_module_scores(Parameters(args))
        .await
        .unwrap()
        .0;
    assert_eq!(result["units"].as_array().unwrap().len(), 2);
    assert_eq!(category(&result, "activity")["additional_score"], 0);
}

#[tokio::test]
async fn unavailable_student_is_not_reported_as_zero() {
    let api = MockServer::start().await;
    let web = MockServer::start().await;
    course_mock(&api, course(), 1).await;
    progress_mock(&api, json!({"userProgress":{}}), 1).await;
    let error = server(&api, &web)
        .get_module_scores(Parameters(read_args()))
        .await
        .err()
        .unwrap();
    assert!(error.contains("unavailable"));
}

#[tokio::test]
async fn stale_score_stops_before_any_website_mutation() {
    let api = MockServer::start().await;
    let web = MockServer::start().await;
    course_mock(&api, course(), 1).await;
    progress_mock(&api, progress(Some(2)), 1).await;
    let error = server(&api, &web)
        .set_module_score(Parameters(write_args()))
        .await
        .err()
        .unwrap();
    assert!(error.contains("Module score changed"));
    assert!(web.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn out_of_range_or_disabled_category_stops_before_mutation() {
    for disabled in [false, true] {
        let api = MockServer::start().await;
        let web = MockServer::start().await;
        let mut data = course();
        if disabled {
            data["units"][0]["additionalScores"][0]["canInstructorSetAdditionalScore"] =
                json!(false);
        }
        course_mock(&api, data, 1).await;
        progress_mock(&api, progress(Some(1)), 1).await;
        let mut args = write_args();
        if !disabled {
            args.score = Some(5);
        }
        assert!(
            server(&api, &web)
                .set_module_score(Parameters(args))
                .await
                .is_err()
        );
        assert!(web.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn score_and_clear_are_written_once_then_verified() {
    for score in [Some(0), Some(3), None] {
        let api = MockServer::start().await;
        let web = MockServer::start().await;
        course_mock(&api, course(), 2).await;
        let counter = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/user-progress/course-a"))
            .and(body_json(json!({"userIds":["student-a"]})))
            .respond_with(move |_: &wiremock::Request| {
                ResponseTemplate::new(200).set_body_json(progress(
                    if counter.fetch_add(1, Ordering::SeqCst) == 0 {
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
        let mut args = write_args();
        args.score = score;
        let result = server(&api, &web)
            .set_module_score(Parameters(args))
            .await
            .unwrap()
            .0;
        assert_eq!(result["write_accepted"], true);
        assert_eq!(result["score_verified"], true);
        assert_eq!(web.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn failed_readback_does_not_hide_accepted_write_or_retry_it() {
    let api = MockServer::start().await;
    let web = MockServer::start().await;
    course_mock(&api, course(), 2).await;
    let counter = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/user-progress/course-a"))
        .respond_with(move |_: &wiremock::Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(200).set_body_json(progress(Some(1)))
            } else {
                ResponseTemplate::new(503)
            }
        })
        .expect(2)
        .mount(&api)
        .await;
    score_mutation(&web, Some(3)).await;
    let result = server(&api, &web)
        .set_module_score(Parameters(write_args()))
        .await
        .unwrap()
        .0;
    assert_eq!(result["write_accepted"], true);
    assert_eq!(result["score_verified"], false);
    assert!(result["readback_error"].is_string());
    assert_eq!(web.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn missing_or_malformed_preconditions_do_not_allow_writes() {
    let args = json!({"course_id":"course-a","unit_id":"unit-a","user_id":"student-a","scoring_group_id":"activity","score":1});
    assert!(serde_json::from_value::<ModuleScoreArgs>(args).is_err());
    let api = MockServer::start().await;
    let web = MockServer::start().await;
    course_mock(&api, course(), 1).await;
    let mut data = progress(Some(1));
    data["userProgress"]["student-a"]["additionalScores"]["unit-a"]["activity"] = json!("1");
    progress_mock(&api, data, 1).await;
    assert!(
        server(&api, &web)
            .set_module_score(Parameters(write_args()))
            .await
            .is_err()
    );
    assert!(web.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn unchanged_score_is_a_verified_no_op() {
    let api = MockServer::start().await;
    let web = MockServer::start().await;
    course_mock(&api, course(), 1).await;
    progress_mock(&api, progress(Some(1)), 1).await;
    let mut args = write_args();
    args.score = Some(1);
    let result = server(&api, &web)
        .set_module_score(Parameters(args))
        .await
        .unwrap()
        .0;
    assert_eq!(result["changed"], false);
    assert_eq!(result["score_verified"], true);
    assert!(web.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn nullable_unscored_course_fields_are_readable() {
    let api = MockServer::start().await;
    let web = MockServer::start().await;
    let data = json!({"units":[{"id":"unit-a","title":"Introduction","additionalScores":null,
        "slides":[{"id":"lesson-a","title":"Welcome","maxScore":0,"scoringGroup":null}]}]});
    course_mock(&api, data, 1).await;
    progress_mock(
        &api,
        json!({"userProgress":{"student-a":{"visitedSlides":null,"additionalScores":null}}}),
        1,
    )
    .await;
    let result = server(&api, &web)
        .get_module_scores(Parameters(read_args()))
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
async fn malformed_or_missing_scoring_categories_cannot_reach_mutation() {
    for case in 0..4 {
        let api = MockServer::start().await;
        let web = MockServer::start().await;
        let mut data = course();
        match case {
            0 => data["units"][0]["slides"][0]["scoringGroup"] = Value::Null,
            1 => data["units"][0]["slides"][0]["scoringGroup"] = json!(true),
            2 => data["units"][0]["additionalScores"] = json!({}),
            _ => data["units"][0]["additionalScores"] = Value::Null,
        }
        course_mock(&api, data, 1).await;
        progress_mock(&api, progress(Some(1)), 1).await;
        assert!(
            server(&api, &web)
                .set_module_score(Parameters(write_args()))
                .await
                .is_err()
        );
        assert!(web.received_requests().await.unwrap().is_empty());
    }
}
