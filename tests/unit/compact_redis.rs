use super::*;
use crate::core::protocol::Protocol;
use crate::server::state::compact_test_state;
use redis::AsyncCommands;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

struct RedisServer(Child, PathBuf);

impl Drop for RedisServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_file(&self.1);
    }
}

async fn start_redis() -> (RedisServer, redis::aio::ConnectionManager) {
    let socket = std::env::temp_dir().join(format!("r2a-cache-{}.sock", crate::util::new_uuid()));
    let server = RedisServer(
        Command::new("redis-server")
            .args([
                "--port",
                "0",
                "--save",
                "",
                "--appendonly",
                "no",
                "--unixsocketperm",
                "700",
                "--unixsocket",
            ])
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        socket.clone(),
    );
    let client = redis::Client::open(format!("redis+unix://{}?db=1", socket.display())).unwrap();
    let connection = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.get_connection_manager().await {
                Ok(connection) => break connection,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .unwrap();
    (server, connection)
}

fn state(connection: &redis::aio::ConnectionManager) -> SharedState {
    let mut state = compact_test_state();
    Arc::get_mut(&mut state).unwrap().redis = Some(connection.clone());
    state
}

fn history() -> CanonicalRequest {
    let mut request = CanonicalRequest::new(Protocol::Responses, "test-model");
    request.messages = vec![
        Message::text(Role::User, "old task"),
        Message::text(Role::Assistant, "x".repeat(90_000)),
        Message::text(Role::User, "CURRENT_TASK"),
    ];
    request
}

fn summary(text: &str) -> String {
    format!("{SUMMARY_PREFIX}\n{text}")
}

#[tokio::test]
#[ignore = "requires redis-server on PATH; starts a private Unix-socket instance"]
async fn redis_caches_share_prefixes_expire_and_degrade() {
    let (server, mut connection) = start_redis().await;
    let writer = state(&connection);
    let reader = state(&connection);
    let original = history();
    let compacted = maybe_compact(&writer, original.clone(), |_| {
        Box::pin(async { Ok("shared summary".into()) })
    })
    .await
    .unwrap();
    let usage_key = UsageKey::new(&original);
    writer.record_usage(usage_key, 25_000).await;
    assert!(reader.summary_cache.entries.lock().await.is_empty());
    assert_eq!(reader.estimate_input_tokens(&original).await, Some(25_000));
    assert!(matches!(
        plan(&reader, &original).await,
        Compaction::Cached(_)
    ));
    let reused = maybe_compact(&reader, original.clone(), |_| {
        panic!("must reuse Redis summary")
    })
    .await
    .unwrap();
    assert_eq!(reused.messages.last().unwrap().text, "CURRENT_TASK");
    assert_eq!(
        reused.messages[reused.messages.len() - 2].text,
        compacted.messages[compacted.messages.len() - 2].text
    );

    let hash = prompt_prefix_hashes(&original)[2];
    let summary_key = redis_cache_key(&writer, "summary", &hash);
    let usage_redis_key = redis_cache_key(&writer, "usage", &usage_key.prefix_hash);
    for key in [&summary_key, &usage_redis_key] {
        let ttl: i64 = connection.ttl(key).await.unwrap();
        assert!((1790..=1800).contains(&ttl), "{key}: {ttl}");
    }

    // A new process state has no L1 entries but can reuse the same Redis keys.
    drop(writer);
    let restarted = state(&connection);
    assert_eq!(
        restarted.cached_summary(&original).await,
        Some((2, summary("shared summary")))
    );
    assert_eq!(
        restarted.estimate_input_tokens(&original).await,
        Some(25_000)
    );
    let mut isolated = state(&connection);
    Arc::get_mut(&mut isolated).unwrap().redis_key_prefix = "other-gateway".into();
    assert_eq!(isolated.cached_summary(&original).await, None);
    assert_eq!(isolated.estimate_input_tokens(&original).await, None);
    let mut changed = original.clone();
    changed.system = Some("other instructions".into());
    assert_eq!(restarted.cached_summary(&changed).await, None);
    assert_eq!(restarted.estimate_input_tokens(&changed).await, None);

    // Local short hits must not hide longer Redis hits, even across branches.
    let mut seed = original.clone();
    seed.messages.truncate(1);
    reader.usage_cache.record(UsageKey::new(&seed), 10).await;
    reader
        .summary_cache
        .record(
            &original.model,
            prompt_prefix_hashes(&seed)[1],
            1,
            summary("short"),
        )
        .await;
    let mut extended = original.clone();
    extended.messages.extend([
        Message::text(Role::Assistant, "next"),
        Message::text(Role::User, "next task"),
    ]);
    let restarted = state(&connection);
    restarted
        .record_summary(&extended, summary("longest"))
        .await;
    restarted
        .record_usage(UsageKey::new(&extended), 30_000)
        .await;
    let mut branch = extended.clone();
    branch.messages[3].text = "other branch".into();
    restarted.record_summary(&branch, summary("branch")).await;
    restarted.record_usage(UsageKey::new(&branch), 40_000).await;
    assert_eq!(
        reader.cached_summary(&extended).await,
        Some((4, summary("longest")))
    );
    assert_eq!(reader.estimate_input_tokens(&extended).await, Some(30_000));
    assert_eq!(
        reader.cached_summary(&branch).await,
        Some((4, summary("branch")))
    );
    assert_eq!(reader.estimate_input_tokens(&branch).await, Some(40_000));
    extended
        .messages
        .push(Message::text(Role::Assistant, "abcd"));
    assert_eq!(reader.estimate_input_tokens(&extended).await, Some(30_001));
    // A partial hit must pass only the unsummarized suffix to the next summary.
    branch.messages[3].text = "uncached branch".into();
    match plan(&reader, &branch).await {
        Compaction::Summarize(messages) => {
            assert_eq!(messages[0].text, summary("shared summary"));
            assert_eq!(messages[1].text, "CURRENT_TASK");
            assert_eq!(messages[2].text, "uncached branch");
        }
        _ => panic!("expected incremental compaction"),
    }

    // Corrupt remote entries are misses, including binary values and wrong types.
    let fresh = state(&connection);
    for invalid in [b"0".as_slice(), b"-1", b"4294967296", b"garbage", &[0xff]] {
        let _: () = connection
            .set_ex(&usage_redis_key, invalid, 1800)
            .await
            .unwrap();
        assert_eq!(fresh.estimate_input_tokens(&original).await, None);
    }
    let _: usize = connection.del(&usage_redis_key).await.unwrap();
    let _: usize = connection
        .lpush(&usage_redis_key, "wrong type")
        .await
        .unwrap();
    assert_eq!(fresh.estimate_input_tokens(&original).await, None);
    for invalid in [String::new(), "garbage".into(), summary("  ")] {
        let _: () = connection
            .set_ex(&summary_key, invalid, 1800)
            .await
            .unwrap();
        assert_eq!(fresh.cached_summary(&original).await, None);
    }
    let _: () = connection
        .set_ex(&summary_key, vec![0xff_u8], 1800)
        .await
        .unwrap();
    assert_eq!(fresh.cached_summary(&original).await, None);
    let _: usize = connection.del(&summary_key).await.unwrap();
    let _: usize = connection.lpush(&summary_key, "wrong type").await.unwrap();
    assert_eq!(fresh.cached_summary(&original).await, None);

    // Invalid records must never reach Redis.
    let mut empty = original.clone();
    empty.messages.clear();
    fresh.record_usage(UsageKey::new(&empty), 10).await;
    fresh.record_summary(&empty, summary("empty history")).await;
    fresh.record_usage(usage_key, 0).await;
    let invalid_key = redis_cache_key(&fresh, "usage", &UsageKey::new(&empty).prefix_hash);
    assert!(!connection.exists::<_, bool>(invalid_key).await.unwrap());
    let mut failed = original.clone();
    failed.system = Some("failed compaction".into());
    assert!(
        maybe_compact(&fresh, failed.clone(), |_| Box::pin(async {
            Ok("  ".into())
        }))
        .await
        .is_err()
    );
    assert_eq!(state(&connection).cached_summary(&failed).await, None);

    // Reads neither renew Redis TTL nor populate L1 with a fresh 30-minute TTL.
    fresh.record_summary(&original, summary("expiring")).await;
    fresh.record_usage(usage_key, 25_000).await;
    for key in [&summary_key, &usage_redis_key] {
        let _: bool = connection.pexpire(key, 180).await.unwrap();
    }
    let expiring_reader = state(&connection);
    assert!(expiring_reader.cached_summary(&original).await.is_some());
    assert_eq!(
        expiring_reader.estimate_input_tokens(&original).await,
        Some(25_000)
    );
    for key in [&summary_key, &usage_redis_key] {
        let ttl: i64 = connection.pttl(key).await.unwrap();
        assert!((1..=180).contains(&ttl));
    }
    tokio::time::sleep(Duration::from_millis(220)).await;
    assert_eq!(expiring_reader.cached_summary(&original).await, None);
    assert_eq!(expiring_reader.estimate_input_tokens(&original).await, None);

    // A stalled Redis command cannot hold a local cache lock or the response.
    let _: () = redis::cmd("CLIENT")
        .arg("PAUSE")
        .arg(1000)
        .arg("ALL")
        .query_async(&mut connection)
        .await
        .unwrap();
    let started = Instant::now();
    let pending = tokio::spawn({
        let fresh = fresh.clone();
        let original = original.clone();
        async move {
            fresh.record_usage(usage_key, 123).await;
            fresh.record_summary(&original, summary("fallback")).await;
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        tokio::time::timeout(
            Duration::from_millis(50),
            fresh.usage_cache.estimate_tokens(&original)
        )
        .await
        .unwrap(),
        Some(123)
    );
    pending.await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(700));
    let started = Instant::now();
    assert_eq!(fresh.estimate_input_tokens(&original).await, Some(123));
    assert_eq!(
        fresh.cached_summary(&original).await,
        Some((2, summary("fallback")))
    );
    assert_eq!(expiring_reader.estimate_input_tokens(&original).await, None);
    assert_eq!(expiring_reader.cached_summary(&original).await, None);
    assert!(started.elapsed() < Duration::from_millis(700));

    // Connection loss is also a cache miss; inference/compaction still proceeds.
    drop(server);
    assert_eq!(fresh.estimate_input_tokens(&original).await, Some(123));
    let rewritten = maybe_compact(&expiring_reader, original, |_| {
        Box::pin(async { Ok("offline summary".into()) })
    })
    .await
    .unwrap();
    assert_eq!(rewritten.messages.last().unwrap().text, "CURRENT_TASK");
    assert_eq!(
        rewritten.messages[rewritten.messages.len() - 2].text,
        summary("offline summary")
    );
}
