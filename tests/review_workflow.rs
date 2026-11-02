use rmcp::handler::server::wrapper::Parameters;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use ulearn_mcp::{
    client::UlearnClient,
    server::{
        CommentArgs, ReplyArgs, ReviewTarget, ScoreArgs, SubmissionArgs, UlearnServer, code_hash,
    },
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, method, path, query_param},
};

const CODE: &str = "@R0\nD=M\n";

fn target() -> ReviewTarget {
    ReviewTarget {
        course_id: "nand2tetris".into(),
        slide_id: "slide-a".into(),
        user_id: "student-a".into(),
        submission_id: 11,
        expected_code_sha256: code_hash(CODE),
        expected_percent: None,
    }
}
fn attempt() -> Value {
    json!({"id":11,"code":CODE,"manualChecking":{"percent":null,"reviews":[{"id":101,"comment":"Existing review"}]}})
}
fn server(mock: &MockServer) -> UlearnServer {
    UlearnServer::new(UlearnClient::new(&mock.uri(), Some("synthetic-test-token".into())).unwrap())
}
fn args() -> SubmissionArgs {
    SubmissionArgs {
        course_id: "nand2tetris".into(),
        slide_id: "slide-a".into(),
        user_id: "student-a".into(),
        submission_id: Some(11),
    }
}
fn comment(t: ReviewTarget) -> CommentArgs {
    CommentArgs {
        target: t,
        text: "Explain this instruction".into(),
        start_line: 0,
        start_position: 0,
        finish_line: 0,
        finish_position: 3,
    }
}
async fn attempts(mock: &MockServer, response: Value, times: u64) {
    Mock::given(method("GET"))
        .and(path("/submissions"))
        .and(query_param("courseId", "nand2tetris"))
        .and(query_param("slideId", "slide-a"))
        .and(query_param("userId", "student-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .expect(times)
        .mount(mock)
        .await;
}
async fn assert_no_posts(mock: &MockServer) {
    assert!(
        mock.received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.method == "GET")
    );
}

#[tokio::test]
async fn context_uses_requested_attempt_and_matching_topic() {
    let mock = MockServer::start().await;
    attempts(
        &mock,
        json!({"submissions":[{"id":12,"code":"newer attempt"},attempt()]}),
        1,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/slides/nand2tetris/slide-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"title":"Mult","blocks":[{"type":"markdown","content":"Multiply R0 and R1"}]}),
        ))
        .expect(1)
        .mount(&mock)
        .await;
    Mock::given(method("GET")).and(path("/courses/nand2tetris"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"units":[{"id":"unrelated","slides":[{"id":"slide-b"}]},{"id":"unit-a","slides":[{"id":"slide-a"}]}]})))
        .expect(1).mount(&mock).await;
    let result = server(&mock)
        .get_review_context(Parameters(args()))
        .await
        .unwrap()
        .0;
    assert_eq!(result["submission"]["id"], 11);
    assert_eq!(result["submission"]["code"], CODE);
    assert_eq!(result["submission"]["code_sha256"], code_hash(CODE));
    assert_eq!(result["unit"]["id"], "unit-a");
    assert_eq!(result["slide"]["title"], "Mult");
    assert_no_posts(&mock).await;
}

#[tokio::test]
async fn missing_requested_attempt_is_not_replaced_with_latest() {
    let mock = MockServer::start().await;
    attempts(
        &mock,
        json!({"submissions":[{"id":12,"code":"newer attempt"}]}),
        1,
    )
    .await;
    let result = server(&mock).get_review_context(Parameters(args())).await;
    assert!(result.err().unwrap().contains("Exact submission not found"));
    assert_eq!(mock.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn source_mismatch_stops_before_post() {
    let mock = MockServer::start().await;
    attempts(&mock, json!({"submissions":[attempt()]}), 1).await;
    let mut t = target();
    t.expected_code_sha256 = code_hash("different code");
    let result = server(&mock)
        .add_review_comment(Parameters(comment(t)))
        .await;
    assert!(result.err().unwrap().contains("Source changed"));
    assert_no_posts(&mock).await;
}

#[tokio::test]
async fn stale_grade_stops_before_post() {
    let mock = MockServer::start().await;
    let mut s = attempt();
    s["manualChecking"]["percent"] = json!(75);
    attempts(&mock, json!({"submissions":[s]}), 1).await;
    let result = server(&mock)
        .set_review_score(Parameters(ScoreArgs {
            target: target(),
            percent: 100,
        }))
        .await;
    assert!(result.err().unwrap().contains("Grade changed"));
    assert_no_posts(&mock).await;
}

#[tokio::test]
async fn invalid_score_does_not_make_network_requests() {
    let mock = MockServer::start().await;
    for n in [-1, 101] {
        assert!(
            server(&mock)
                .set_review_score(Parameters(ScoreArgs {
                    target: target(),
                    percent: n
                }))
                .await
                .err()
                .unwrap()
                .contains("0..100")
        );
    }
    assert!(mock.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn reply_cannot_target_another_submissions_review() {
    let mock = MockServer::start().await;
    attempts(&mock, json!({"submissions":[attempt()]}), 1).await;
    let result = server(&mock)
        .reply_to_review(Parameters(ReplyArgs {
            target: target(),
            review_id: 999,
            text: "Reply".into(),
        }))
        .await;
    assert!(result.err().unwrap().contains("does not belong"));
    assert_no_posts(&mock).await;
}

#[tokio::test]
async fn comment_posts_verified_range_and_exact_submission() {
    let mock = MockServer::start().await;
    attempts(&mock, json!({"submissions":[attempt()]}), 2).await;
    Mock::given(method("POST")).and(path("/reviews")).and(query_param("submissionId","11"))
        .and(body_json(json!({"text":"Explain this instruction","startLine":0,"startPosition":0,"finishLine":0,"finishPosition":3})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":102})))
        .expect(1).mount(&mock).await;
    let result = server(&mock)
        .add_review_comment(Parameters(comment(target())))
        .await
        .unwrap()
        .0;
    assert_eq!(result["write_accepted"], true);
    assert_eq!(result["receipt"]["id"], 102);
    assert_eq!(result["submission"]["id"], 11);
}

#[tokio::test]
async fn zero_percent_is_sent_and_verified_without_reopening_review() {
    let mock = MockServer::start().await;
    let counter = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/submissions"))
        .respond_with(move |_: &wiremock::Request| {
            let mut s = attempt();
            if counter.fetch_add(1, Ordering::SeqCst) > 0 {
                s["manualChecking"]["percent"] = json!(0);
            }
            ResponseTemplate::new(200).set_body_json(json!({"submissions":[s]}))
        })
        .expect(2)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/submissions/11/score"))
        .and(query_param("percent", "0"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&mock)
        .await;
    let result = server(&mock)
        .set_review_score(Parameters(ScoreArgs {
            target: target(),
            percent: 0,
        }))
        .await
        .unwrap()
        .0;
    assert_eq!(result["write_accepted"], true);
    assert_eq!(result["score_verified"], true);
    let requests = mock.received_requests().await.unwrap();
    assert_eq!(requests.iter().filter(|r| r.method == "POST").count(), 1);
    assert!(
        requests
            .iter()
            .all(|r| !r.url.path().contains("manual-checking"))
    );
}

#[tokio::test]
async fn failed_readback_preserves_accepted_write_and_does_not_retry() {
    let mock = MockServer::start().await;
    let counter = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/submissions"))
        .respond_with(move |_: &wiremock::Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(200).set_body_json(json!({"submissions":[attempt()]}))
            } else {
                ResponseTemplate::new(503)
            }
        })
        .expect(2)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/submissions/11/score"))
        .and(query_param("percent", "100"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock)
        .await;
    let result = server(&mock)
        .set_review_score(Parameters(ScoreArgs {
            target: target(),
            percent: 100,
        }))
        .await
        .unwrap()
        .0;
    assert_eq!(result["write_accepted"], true);
    assert!(result["readback_error"].as_str().unwrap().contains("503"));
    assert!(
        result["instruction"]
            .as_str()
            .unwrap()
            .contains("Do not retry")
    );
}

#[test]
fn expected_grade_is_required_but_null_is_valid_in_json_and_schema() {
    let mut value = json!({"slide_id":"slide-a","user_id":"student-a","submission_id":11,"expected_code_sha256":code_hash(CODE),"expected_percent":null});
    let pending: ReviewTarget = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(pending.expected_percent, None);
    value["expected_percent"] = json!(0);
    assert_eq!(
        serde_json::from_value::<ReviewTarget>(value.clone())
            .unwrap()
            .expected_percent,
        Some(0)
    );
    value.as_object_mut().unwrap().remove("expected_percent");
    assert!(serde_json::from_value::<ReviewTarget>(value).is_err());
    let schema = serde_json::to_value(rmcp::schemars::schema_for!(ReviewTarget)).unwrap();
    assert!(
        schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "expected_percent")
    );
    let field = &schema["properties"]["expected_percent"];
    assert!(
        field["type"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "null")
    );
}

#[tokio::test]
async fn malformed_grade_readback_does_not_turn_a_saved_score_into_failed_write() {
    let mock = MockServer::start().await;
    let counter = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/submissions"))
        .respond_with(move |_: &wiremock::Request| {
            let mut s = attempt();
            if counter.fetch_add(1, Ordering::SeqCst) > 0 {
                s["manualChecking"]["percent"] = json!("unrecognized-new-format");
            }
            ResponseTemplate::new(200).set_body_json(json!({"submissions":[s]}))
        })
        .expect(2)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/submissions/11/score"))
        .and(query_param("percent", "100"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock)
        .await;
    let result = server(&mock)
        .set_review_score(Parameters(ScoreArgs {
            target: target(),
            percent: 100,
        }))
        .await
        .unwrap()
        .0;
    assert_eq!(result["write_accepted"], true);
    assert_eq!(result["score_verified"], false);
    assert!(result["verification_error"].is_string());
}
