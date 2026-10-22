use super::{ProviderPermit, ProviderScope, Repository};
use chrono::Utc;
use std::time::Duration;

async fn permit(repository: &Repository, scope: ProviderScope, budget: u32) -> ProviderPermit {
    repository
        .reserve_scoped_provider_request(scope, budget, Duration::ZERO, Duration::from_secs(30))
        .await
        .unwrap()
}

#[tokio::test]
async fn scopes_do_not_clear_or_extend_each_others_cooldown() {
    let repository = Repository::connect(":memory:").await.unwrap();
    let until = repository
        .record_scoped_provider_failure(ProviderScope::Detail, 21_600, 86_400)
        .await
        .unwrap();
    assert!(matches!(
        permit(&repository, ProviderScope::Detail, 500).await,
        ProviderPermit::Paused {
            scope: ProviderScope::Detail,
            ..
        }
    ));
    assert!(matches!(
        permit(&repository, ProviderScope::Search, 500).await,
        ProviderPermit::Ready
    ));
    repository
        .clear_scoped_provider_failure(ProviderScope::Search)
        .await
        .unwrap();
    repository
        .clear_scoped_provider_failure(ProviderScope::Detail)
        .await
        .unwrap();
    assert_eq!(
        repository
            .record_scoped_provider_failure(ProviderScope::Detail, 21_600, 86_400)
            .await
            .unwrap(),
        until
    );
    assert_eq!(
        repository.provider_backoffs().await.unwrap()[0].backoff_until,
        until
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT consecutive_failures FROM provider_state WHERE provider = 'bilibili:detail'"
        )
        .fetch_one(&repository.pool)
        .await
        .unwrap(),
        1
    );

    sqlx::query("UPDATE provider_state SET backoff_until = ? WHERE provider = 'bilibili:detail'")
        .bind(Utc::now() - chrono::Duration::seconds(1))
        .execute(&repository.pool)
        .await
        .unwrap();
    let next = repository
        .record_scoped_provider_failure(ProviderScope::Detail, 21_600, 86_400)
        .await
        .unwrap();
    assert!((next - Utc::now()).num_seconds() >= 43_198);
    sqlx::query("UPDATE provider_state SET backoff_until = ? WHERE provider = 'bilibili:detail'")
        .bind(Utc::now() - chrono::Duration::seconds(1))
        .execute(&repository.pool)
        .await
        .unwrap();
    repository
        .clear_scoped_provider_failure(ProviderScope::Detail)
        .await
        .unwrap();
    assert!(repository.provider_backoffs().await.unwrap().is_empty());
}

#[tokio::test]
async fn budget_is_global_but_paused_requests_are_not_charged() {
    let repository = Repository::connect(":memory:").await.unwrap();
    repository
        .record_scoped_provider_failure(ProviderScope::Detail, 60, 60)
        .await
        .unwrap();
    assert!(matches!(
        permit(&repository, ProviderScope::Detail, 1).await,
        ProviderPermit::Paused { .. }
    ));
    assert!(matches!(
        permit(&repository, ProviderScope::Search, 1).await,
        ProviderPermit::Ready
    ));
    assert!(matches!(
        permit(&repository, ProviderScope::Followers, 1).await,
        ProviderPermit::BudgetExhausted
    ));
    repository
        .record_scoped_provider_failure(ProviderScope::Global, 60, 60)
        .await
        .unwrap();
    assert!(matches!(
        permit(&repository, ProviderScope::Search, 500).await,
        ProviderPermit::Paused {
            scope: ProviderScope::Global,
            ..
        }
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT daily_count FROM provider_state WHERE provider = 'bilibili'"
        )
        .fetch_one(&repository.pool)
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn pacing_is_shared_across_independent_connections_and_instances() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("provider.db");
    let first = Repository::connect(path.to_str().unwrap()).await.unwrap();
    let second = Repository::connect(path.to_str().unwrap()).await.unwrap();
    let mut calls = tokio::task::JoinSet::new();
    for index in 0..16 {
        let repository = if index % 2 == 0 {
            first.clone()
        } else {
            second.clone()
        };
        calls.spawn(async move {
            repository
                .reserve_scoped_provider_request(
                    ProviderScope::Search,
                    500,
                    Duration::from_secs(30),
                    Duration::from_secs(30),
                )
                .await
                .unwrap()
        });
    }
    let mut ready = 0;
    while let Some(call) = calls.join_next().await {
        match call.unwrap() {
            ProviderPermit::Ready => ready += 1,
            ProviderPermit::Wait(wait) => assert!(wait > Duration::from_secs(20)),
            other => panic!("unexpected permit: {other:?}"),
        }
    }
    assert_eq!(ready, 1);
}

#[tokio::test]
async fn only_one_post_cooldown_probe_is_admitted() {
    let repository = Repository::connect(":memory:").await.unwrap();
    repository
        .record_scoped_provider_failure(ProviderScope::Detail, 60, 60)
        .await
        .unwrap();
    sqlx::query("UPDATE provider_state SET backoff_until = ? WHERE provider = 'bilibili:detail'")
        .bind(Utc::now() - chrono::Duration::seconds(1))
        .execute(&repository.pool)
        .await
        .unwrap();
    assert!(matches!(
        permit(&repository, ProviderScope::Detail, 500).await,
        ProviderPermit::Ready
    ));
    assert!(matches!(
        permit(&repository, ProviderScope::Detail, 500).await,
        ProviderPermit::Wait(_)
    ));
    repository
        .clear_scoped_provider_failure(ProviderScope::Detail)
        .await
        .unwrap();
    assert!(matches!(
        permit(&repository, ProviderScope::Detail, 500).await,
        ProviderPermit::Ready
    ));
}

#[tokio::test]
async fn cache_persists_and_expires_without_extending_on_read() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cache.db");
    let first = Repository::connect(path.to_str().unwrap()).await.unwrap();
    first
        .cache_provider_value("video:1", &serde_json::json!({"duration": 1500}), 1800)
        .await
        .unwrap();
    let second = Repository::connect(path.to_str().unwrap()).await.unwrap();
    assert_eq!(
        second
            .cached_provider_value("video:1")
            .await
            .unwrap()
            .unwrap()["duration"],
        1500
    );
    first
        .cache_provider_value("expired", &serde_json::json!(1000), -1)
        .await
        .unwrap();
    assert!(
        second
            .cached_provider_value("expired")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        second
            .cached_provider_value("missing")
            .await
            .unwrap()
            .is_none()
    );
    let until = first
        .record_scoped_provider_failure(ProviderScope::Detail, 3600, 3600)
        .await
        .unwrap();
    assert_eq!(
        second.provider_backoffs().await.unwrap()[0].backoff_until,
        until
    );
}

#[tokio::test]
async fn migration_keeps_legacy_cooldown_budget_and_watch_history_unchanged() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let mut migrations = sqlx::migrate::Migrator::new(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"),
    )
    .await
    .unwrap();
    migrations
        .migrations
        .to_mut()
        .retain(|migration| migration.version < 21);
    migrations.run(&pool).await.unwrap();
    let until = Utc::now() + chrono::Duration::hours(6);
    let now = Utc::now();
    sqlx::query("UPDATE provider_state SET backoff_until = ?, consecutive_failures = 1, daily_date = ?, daily_count = 32, updated_at = ? WHERE provider = 'bilibili'")
        .bind(until).bind(now.date_naive()).bind(now).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO anime(title,duration_min_sec,duration_max_sec,created_at,updated_at) VALUES('legacy',1200,2400,?,?)")
        .bind(now).bind(now).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO episode(anime_id,episode_no,state,next_check_at,watched_at) VALUES(1,1,'notified',?,?)")
        .bind(now).bind(now).execute(&pool).await.unwrap();
    sqlx::migrate!().run(&pool).await.unwrap();
    let (after, failures, count): (chrono::DateTime<Utc>, i64, i64) = sqlx::query_as("SELECT backoff_until, consecutive_failures, daily_count FROM provider_state WHERE provider = 'bilibili'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!((after, failures, count), (until, 1, 32));
    let (state, watched): (String, chrono::DateTime<Utc>) =
        sqlx::query_as("SELECT state, watched_at FROM episode WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "notified");
    assert_eq!(watched, now);
}
