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
