async fn assert_metadata_queue_fifo_eviction_no_replay(inline: bool) {
    let feed = activity_comment_feed(inline);
    let mut responses = activity_comment_poll(inline, json!([
        activity_comment(0, 1, ACTIVITY_T0, ACTIVITY_T0)
    ]));
    let mut capped = activity_poll_responses(json!([]), json!([activity_pr(1, ACTIVITY_T0)]), json!([]));
    if inline { capped.pop(); } else { capped.truncate(2); }
    responses.extend(capped);
    for page in 0..MAX_COMMENT_PAGES {
        let comments: Vec<_> = (page * 100 + 1..=page * 100 + 100).map(|id| {
            let time = activity_second(id as u64);
            activity_comment(id as u64, 1, &time, &time)
        }).collect();
        let mut response = activity_response(feed, json!(comments));
        response.3 = Some(format!("{feed}?page={}", page + 2));
        responses.push(response);
    }
    if !inline { responses.push(activity_response("/repos/org/repo/pulls/comments", json!([]))); }
    responses.push(activity_response("/repos/org/repo/pulls/1/reviews", json!([])));
    let tail = json!([
        activity_comment(1000, 1, &activity_second(1000), &activity_second(1000)),
        activity_comment(1, 1, &activity_second(1), &activity_second(1003)),
        activity_comment(1001, 1, &activity_second(1004), &activity_second(1004)),
        activity_comment(1002, 99, &activity_second(1005), &activity_second(1005))
    ]);
    responses.extend(activity_comment_poll(inline, tail.clone()));
    let mut failure = activity_response("/repos/org/repo/pulls/99", json!({"message": "failed"}));
    failure.2 = 500;
    responses.push(failure);
    let mut retry = activity_comment_poll(inline, tail.clone());
    activity_lookup_before_feeds(&mut retry, 0, &[99]);
    responses.extend(retry);
    responses.extend(activity_comment_poll(inline, tail));
    let (base, _, server) = activity_server(responses).await;
    let mut harness = ActivityHarness::new(base);
    assert!(harness.poll().await.is_empty());
    let initial = harness.poll().await;
    assert_eq!(initial.len(), 1000);
    assert_eq!(initial.iter().filter(|event| event.payload["comment_id"] == 1).count(), 1);
    let second = harness.poll().await;
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].payload["comment_id"], 1001);
    let activity = &harness.repo_state().activity;
    let (position, lower, ids) = if inline {
        (&activity.review_comment_cursor, &activity.review_comment_lower, &activity.emitted_review_comment_ids)
    } else { (&activity.pr_comment_cursor, &activity.pr_comment_lower, &activity.emitted_pr_comment_ids) };
    assert_eq!(position.as_deref(), Some(activity_second(1005).as_str()));
    assert_eq!(lower.as_deref(), Some(activity_second(1005).as_str()));
    assert!(!ids.contains(&1), "the regression requires actual FIFO eviction");
    assert!(ids.contains(&1002), "queue owns the unresolved ID immediately");
    let third = harness.poll().await;
    assert_eq!(third.len(), 1);
    assert_eq!(third[0].payload["comment_id"], 1002);
    assert_eq!(third[0].payload["pr_author"], "pr-owner");
    assert_eq!(third[0].payload["title"], "A PR");
    assert!(harness.poll().await.is_empty());
    server.await.unwrap();
}

#[tokio::test]
async fn pr_activity_metadata_queue_conversation_fifo_eviction_no_replay() {
    assert_metadata_queue_fifo_eviction_no_replay(false).await;
}

#[tokio::test]
async fn pr_activity_metadata_queue_inline_fifo_eviction_no_replay() {
    assert_metadata_queue_fifo_eviction_no_replay(true).await;
}

// Keep L at T1 while 1000 new, known-metadata comments evict pending ID 1.
fn activity_pending_receipt_eviction(inline: bool, attempts_before_cap: usize) -> Vec<ActivityResponse> {
    let feed = activity_comment_feed(inline);
    let mut responses = activity_comment_poll(inline, json!([
        activity_comment(0, 1, ACTIVITY_T0, ACTIVITY_T0)
    ]));
    for attempt in 0..attempts_before_cap {
        let comments = if attempt == 0 {
            json!([activity_comment(1, 99, ACTIVITY_T1, ACTIVITY_T1)])
        } else { json!([]) };
        let mut batch = activity_comment_poll(inline, comments);
        if attempt == 0 { activity_lookup_after_feed(&mut batch, feed, 0, &[99]); }
        else { activity_lookup_before_feeds(&mut batch, 0, &[99]); }
        batch.iter_mut().find(|response| response.0 == "/repos/org/repo/pulls/99").unwrap().2 = 500;
        responses.extend(batch);
    }
    let mut capped = activity_poll_responses(json!([]), json!([activity_pr(1, ACTIVITY_T0)]), json!([]));
    activity_lookup_before_feeds(&mut capped, 0, &[99]);
    capped.iter_mut().find(|response| response.0 == "/repos/org/repo/pulls/99").unwrap().2 = 500;
    if inline { capped.pop(); } else { capped.truncate(3); }
    responses.extend(capped);
    for page in 0..MAX_COMMENT_PAGES {
        let comments: Vec<_> = (page * 100 + 2..=page * 100 + 101).map(|id| {
            let time = activity_second(id as u64 + 60);
            activity_comment(id as u64, 1, &time, &time)
        }).collect();
        let mut response = activity_response(feed, json!(comments));
        response.3 = Some(format!("{feed}?page={}", page + 2));
        responses.push(response);
    }
    if !inline { responses.push(activity_response("/repos/org/repo/pulls/comments", json!([]))); }
    responses.push(activity_response("/repos/org/repo/pulls/1/reviews", json!([])));
    responses
}

fn assert_pending_receipt_evicted(harness: &ActivityHarness, inline: bool) {
    let activity = &harness.repo_state().activity;
    let (position, lower, ids) = if inline {
        (&activity.review_comment_cursor, &activity.review_comment_lower, &activity.emitted_review_comment_ids)
    } else { (&activity.pr_comment_cursor, &activity.pr_comment_lower, &activity.emitted_pr_comment_ids) };
    assert_eq!(position.as_deref(), Some(activity_second(1061).as_str()));
    assert_eq!(lower.as_deref(), Some(ACTIVITY_T1), "the capped fetch must hold L");
    assert!(!ids.contains(&1), "pending ID 1 must actually leave the feed FIFO");
    assert_eq!(activity.pending_comments.len(), 1);
    assert_eq!(activity.pending_comments[0].id, 1);
    assert_eq!(activity.pending_comments[0].path.is_some(), inline);
}

async fn assert_pending_delivery_refreshes_evicted_receipt(inline: bool) {
    let mut responses = activity_pending_receipt_eviction(inline, 1);
    let mut recovery = activity_comment_poll(inline, json!([
        activity_comment(1, 99, ACTIVITY_T1, &activity_second(1062))
    ]));
    activity_lookup_before_feeds(&mut recovery, 0, &[99]);
    responses.extend(recovery);
    let (base, _, server) = activity_server(responses).await;
    let mut harness = ActivityHarness::new(base);
    assert!(harness.poll().await.is_empty());
    assert!(harness.poll().await.is_empty());
    let mut events = harness.poll().await;
    assert_eq!(events.len(), 1000);
    assert_pending_receipt_evicted(&harness, inline);
    events.extend(harness.poll().await);
    let delivered: Vec<_> = events.iter().filter(|event| event.payload["comment_id"] == 1).collect();
    assert_eq!(delivered.len(), 1, "pending delivery and its same-poll edit must emit once total");
    assert_eq!(delivered[0].payload["pr_author"], "pr-owner");
    assert_eq!(delivered[0].payload["title"], "A PR");
    assert!(harness.repo_state().activity.pending_comments.is_empty());
    server.await.unwrap();
}

#[tokio::test]
async fn pr_activity_metadata_queue_conversation_delivery_refreshes_evicted_receipt() {
    assert_pending_delivery_refreshes_evicted_receipt(false).await;
}

#[tokio::test]
async fn pr_activity_metadata_queue_inline_delivery_refreshes_evicted_receipt() {
    assert_pending_delivery_refreshes_evicted_receipt(true).await;
}

#[tokio::test]
async fn pr_activity_metadata_queue_give_up_and_overflow_refresh_evicted_receipt() {
    for inline in [false, true] {
        for give_up in [true, false] {
            let attempts = if give_up { MAX_METADATA_ATTEMPTS - 2 } else { 1 };
            let mut responses = activity_pending_receipt_eviction(inline, attempts);
            let mut comments: Vec<_> = if give_up { Vec::new() } else {
                (1002..1002 + MAX_PENDING_PR_COMMENTS as u64).map(|id| {
                    activity_comment(id, 99, &activity_second(1062), &activity_second(1062))
                }).collect()
            };
            comments.push(activity_comment(1, 99, ACTIVITY_T1, &activity_second(1063)));
            let mut delivery = activity_comment_poll(inline, json!(comments));
            activity_lookup_before_feeds(&mut delivery, 0, &[99]);
            delivery.iter_mut().find(|response| response.0 == "/repos/org/repo/pulls/99").unwrap().2 = 500;
            responses.extend(delivery);
            let mut recovery = activity_comment_poll(inline, json!([]));
            if !give_up { activity_lookup_before_feeds(&mut recovery, 0, &[99]); }
            responses.extend(recovery);
            let (base, _, server) = activity_server(responses).await;
            let mut harness = ActivityHarness::new(base);
            assert!(harness.poll().await.is_empty());
            for _ in 0..attempts { assert!(harness.poll().await.is_empty()); }
            let mut events = harness.poll().await;
            assert_eq!(events.len(), 1000);
            assert_pending_receipt_evicted(&harness, inline);
            let delivery = harness.poll().await;
            assert_eq!(delivery.len(), 1, "give_up={give_up}, inline={inline}");
            assert_eq!(delivery[0].payload["comment_id"], 1);
            assert_eq!(delivery[0].payload["pr_author"], "");
            assert_eq!(delivery[0].payload["title"], "");
            let activity = &harness.repo_state().activity;
            let ids = if inline { &activity.emitted_review_comment_ids } else { &activity.emitted_pr_comment_ids };
            assert!(ids.contains(&1), "every pending delivery must refresh its receipt");
            assert!(!activity.pending_comments.iter().any(|comment| comment.id == 1));
            events.extend(delivery);
            events.extend(harness.poll().await);
            assert_eq!(events.iter().filter(|event| event.payload["comment_id"] == 1).count(), 1);
            assert_eq!(events.len(), 1001 + if give_up { 0 } else { MAX_PENDING_PR_COMMENTS });
            assert!(harness.repo_state().activity.pending_comments.is_empty());
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn pr_activity_metadata_queue_saturation_is_fair_and_reports_deferral() {
    let mut responses = activity_poll_responses(
        json!([activity_comment(0, 1, ACTIVITY_T0, ACTIVITY_T0)]), json!([]),
        json!([activity_comment(0, 1, ACTIVITY_T0, ACTIVITY_T0)]),
    );
    for poll in 0..4u64 {
        let first = poll * 22 + 1;
        let time = activity_second(poll + 60);
        let conversations: Vec<_> = (first..first + 21).map(|id| activity_comment(id, id, &time, &time)).collect();
        let mut batch = activity_poll_responses(json!(conversations), json!([]),
            json!([activity_comment(first + 21, first + 21, &time, &time)]));
        let start = poll * 20 + 1;
        let before: Vec<_> = (start..first).collect();
        let after: Vec<_> = (first..=start + 19).collect();
        activity_lookup_before_feeds(&mut batch, 0, &before);
        activity_lookup_after_feed(&mut batch, "/repos/org/repo/pulls/comments", 0, &after);
        responses.extend(batch);
    }
    let mut drain = activity_poll_responses(json!([]), json!([]), json!([]));
    activity_lookup_before_feeds(&mut drain, 0, &(81..=88).collect::<Vec<_>>());
    responses.extend(drain);
    responses.extend(activity_poll_responses(json!([]), json!([]), json!([])));
    let (base, requests, server) = activity_server(responses).await;
    let mut harness = ActivityHarness::new(base);
    assert!(harness.poll().await.is_empty());
    let mut events = Vec::new();
    for poll in 0..5u64 {
        let batch = harness.poll().await;
        if poll == 0 { harness.assert_degraded("GitHub PR metadata lookup budget exhausted"); }
        events.extend(batch);
        if poll > 0 {
            assert!(events.iter().any(|event| event.canonical_kind() == "github.pr-review-commented"
                && event.payload["comment_id"] == poll * 22), "inline must emit within the next poll");
        }
        assert!(requests.lock().unwrap().iter().filter(|path| {
            path.starts_with("/repos/org/repo/pulls/") && !path.contains("comments")
        }).count() <= (poll as usize + 1) * MAX_PR_METADATA_LOOKUPS);
    }
    assert_eq!(events.len(), 88);
    assert_eq!(events.iter().map(|event| event.payload["comment_id"].as_u64().unwrap()).collect::<HashSet<_>>(), (1..=88).collect());
    assert!(events.iter().all(|event| event.payload["pr_author"] == "pr-owner"));
    assert!(harness.repo_state().activity.pending_comments.is_empty());
    assert!(harness.poll().await.is_empty());
    server.await.unwrap();
}

#[tokio::test]
async fn pr_activity_metadata_queue_persistent_failures_give_up_once() {
    for (inline, status) in [(false, 500), (true, 451), (false, 200)] {
        let mut responses = activity_comment_poll(inline, json!([
            activity_comment(0, 99, ACTIVITY_T0, ACTIVITY_T0)
        ]));
        for attempt in 0..MAX_METADATA_ATTEMPTS {
            let mut batch = activity_comment_poll(inline, json!([
                activity_comment(1, 99, ACTIVITY_T1, ACTIVITY_T1)
            ]));
            if attempt == 0 { activity_lookup_after_feed(&mut batch, activity_comment_feed(inline), 0, &[99]); }
            else { activity_lookup_before_feeds(&mut batch, 0, &[99]); }
            let failure = batch.iter_mut().find(|response| response.0 == "/repos/org/repo/pulls/99").unwrap();
            failure.2 = status;
            failure.1 = if status == 200 { let mut pr = activity_pr(99, ACTIVITY_T1); pr["user"]["login"] = json!(""); pr }
                else { json!({"message": "unavailable"}) };
            responses.extend(batch);
        }
        responses.extend(activity_comment_poll(inline, json!([activity_comment(1, 99, ACTIVITY_T1, ACTIVITY_T1)])));
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        for attempt in 1..=MAX_METADATA_ATTEMPTS {
            let events = harness.poll().await;
            if attempt < MAX_METADATA_ATTEMPTS {
                assert!(events.is_empty());
                assert_eq!(harness.repo_state().activity.pending_comments[0].attempts, attempt);
            } else {
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].payload["comment_id"], 1);
                assert_eq!(events[0].payload["title"], "");
                assert_eq!(events[0].payload["pr_author"], "");
                // Identical degradation is throttled across subsequent scenarios.
                if status == 500 {
                    harness.assert_degraded("GitHub PR metadata attempt cap reached");
                }
            }
        }
        assert!(harness.poll().await.is_empty());
        assert_eq!(requests.lock().unwrap().iter().filter(|path| path.as_str() == "/repos/org/repo/pulls/99").count(), MAX_METADATA_ATTEMPTS);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn pr_activity_metadata_queue_410_emits_empty_once() {
    for inline in [false, true] {
        let mut responses = activity_comment_poll(inline, json!([activity_comment(0, 99, ACTIVITY_T0, ACTIVITY_T0)]));
        for id in [1, 1, 2] {
            responses.extend(activity_comment_poll(inline, json!([activity_comment(id, 99, ACTIVITY_T1, ACTIVITY_T1)])));
        }
        activity_lookup_after_feed(&mut responses, activity_comment_feed(inline), 1, &[99]);
        let gone = responses.iter_mut().find(|response| response.0 == "/repos/org/repo/pulls/99").unwrap();
        gone.2 = 410;
        gone.1 = json!({"message": "Gone"});
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let first = harness.poll().await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].payload["title"], "");
        assert_eq!(first[0].payload["pr_author"], "");
        assert!(harness.poll().await.is_empty());
        assert_eq!(harness.poll().await.len(), 1);
        assert_eq!(requests.lock().unwrap().iter().filter(|path| path.as_str() == "/repos/org/repo/pulls/99").count(), 1);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn pr_activity_metadata_queue_overflow_emits_oldest_without_loss() {
    let mut responses = activity_comment_poll(false, json!([activity_comment(0, 99, ACTIVITY_T0, ACTIVITY_T0)]));
    let comments: Vec<_> = (1..=MAX_PENDING_PR_COMMENTS as u64 + 1)
        .map(|id| activity_comment(id, 99, ACTIVITY_T1, ACTIVITY_T1)).collect();
    responses.extend(activity_comment_poll(false, json!(comments)));
    activity_lookup_after_feed(&mut responses, activity_comment_feed(false), 1, &[99]);
    responses.last_mut().unwrap().2 = 500;
    let mut retry = activity_comment_poll(false, json!(comments));
    activity_lookup_before_feeds(&mut retry, 0, &[99]);
    responses.extend(retry);
    responses.extend(activity_comment_poll(false, json!(comments)));
    let (base, _, server) = activity_server(responses).await;
    let mut harness = ActivityHarness::new(base);
    assert!(harness.poll().await.is_empty());
    let mut events = harness.poll().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload["comment_id"], 1);
    assert_eq!(events[0].payload["pr_author"], "");
    assert_eq!(harness.repo_state().activity.pending_comments.len(), MAX_PENDING_PR_COMMENTS);
    harness.assert_degraded("GitHub PR metadata pending queue overflow");
    events.extend(harness.poll().await);
    assert_eq!(events.len(), MAX_PENDING_PR_COMMENTS + 1);
    assert_eq!(events.iter().map(|event| event.payload["comment_id"].as_u64().unwrap()).collect::<HashSet<_>>(), (1..=MAX_PENDING_PR_COMMENTS as u64 + 1).collect());
    assert!(harness.poll().await.is_empty());
    server.await.unwrap();
}

#[tokio::test]
async fn pr_activity_metadata_queue_survives_ci_persist_failure() {
    let mut responses = activity_comment_poll(false, json!([activity_comment(0, 99, ACTIVITY_T0, ACTIVITY_T0)]));
    responses.extend(activity_poll_responses(
        json!([activity_comment(1, 99, ACTIVITY_T1, ACTIVITY_T1)]), json!([]),
        json!([activity_comment(2, 99, ACTIVITY_T1, ACTIVITY_T1)]),
    ));
    activity_lookup_after_feed(&mut responses, activity_comment_feed(false), 1, &[99]);
    responses.last_mut().unwrap().2 = 500;
    let mut retry = activity_poll_responses(
        json!([activity_comment(1, 99, ACTIVITY_T1, ACTIVITY_T1)]), json!([]),
        json!([activity_comment(2, 99, ACTIVITY_T1, ACTIVITY_T1)]),
    );
    activity_lookup_before_feeds(&mut retry, 0, &[99]);
    responses.extend(retry.clone());
    retry.retain(|response| response.0 != "/repos/org/repo/pulls/99");
    responses.extend(retry);
    let (base, _, server) = activity_server(responses).await;
    let mut harness = ActivityHarness::new(base);
    assert!(harness.poll().await.is_empty());
    let directory = tempfile::tempdir().unwrap();
    harness.baseline_path = Some(directory.path().to_path_buf());
    assert!(harness.poll().await.is_empty());
    assert_eq!(harness.repo_state().activity.pending_comments.len(), 2);
    assert!(harness.repo_state().activity.emitted_pr_comment_ids.contains(&1));
    assert!(harness.repo_state().activity.emitted_review_comment_ids.contains(&2));
    assert!(telemetry::take_test_records().iter().any(|record| record["reason_code"] == "ci_baseline_persist_failed"));
    let events = harness.poll().await;
    assert_eq!(events.len(), 2);
    assert_eq!(events.iter().map(|event| event.payload["comment_id"].as_u64().unwrap()).collect::<HashSet<_>>(), [1, 2].into_iter().collect());
    assert!(events.iter().all(|event| event.payload["pr_author"] == "pr-owner"));
    assert!(harness.repo_state().activity.pending_comments.is_empty());
    assert!(harness.poll().await.is_empty());
    server.await.unwrap();
}
