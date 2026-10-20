use super::*;
use crate::domain::*;
use chrono::{Duration, Utc};

async fn fixture() -> (Repository, i64, VideoCollection) {
    let repo = Repository::connect(":memory:").await.unwrap();
    let id = repo
        .add_anime(NewAnime {
            title: "边缘行者2".into(),
            aliases: vec![],
            next_episode: 1,
            expected_at: Some(Utc::now()),
            expected_weekday: None,
            expected_time: None,
            timezone: "Asia/Shanghai".into(),
            duration_min_sec: 1200,
            duration_max_sec: 2400,
            auto_schedule: Some(AutoScheduleMetadata {
                bangumi_subject_id: 513878,
                anilist_media_id: None,
                anime_schedule_route: Some("cyberpunk-edgerunners-ii".into()),
                total_episodes: Some(10),
                broadcast_pattern: "R/2026-10-20T07:00:00Z/P7D".into(),
                schedule_source: "anime_schedule".into(),
                schedule_confidence: "calibrated".into(),
                schedule_warning: None,
                next_sync_at: Utc::now(),
                episode_mapping: None,
            }),
        })
        .await
        .unwrap();
    let now = Utc::now();
    repo.apply_release_batches(
        id,
        &[ReleaseBatch {
            first_episode: 1,
            last_episode: 10,
            expected_at: Some(now - Duration::hours(1)),
            aired: true,
            source: "anime_schedule".into(),
        }],
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO web_admin(username,password_hash,created_at,password_changed_at) VALUES('owner','test-only',?,?)",
    )
    .bind(now)
    .bind(now)
    .execute(&repo.pool)
    .await
    .unwrap();
    let collection = VideoCollection {
        video: VideoCandidate {
            bvid: "BV1nwnc6WEc9".into(),
            title: "边缘行者2 全10集".into(),
            description: None,
            uploader_mid: 100,
            uploader_name: "test".into(),
            duration_sec: 20000,
            published_at: now,
            url: "https://www.bilibili.com/video/BV1nwnc6WEc9".into(),
            tags: vec![],
            page_count: Some(12),
            view_count: Some(10000),
            reply_count: Some(100),
            uploader_follower_count: Some(1000),
            discovered_at: now,
            enriched: true,
        },
        parts: (1..=12)
            .map(|page| VideoPart {
                page,
                cid: 100 + page,
                title: if page <= 10 {
                    format!("第二季{page}")
                } else {
                    "广告".into()
                },
                duration_sec: 1500,
            })
            .collect(),
    };
    (repo, id, collection)
}
async fn draft(repo: &Repository, id: i64, collection: &VideoCollection, key: &str) {
    repo.create_collection_draft(key, id, 1, &collection.video.url)
        .await
        .unwrap();
    repo.ready_collection_draft(key, collection).await.unwrap();
}
fn selections() -> Vec<CollectionSelection> {
    (1..=10)
        .map(|episode_no| CollectionSelection {
            episode_no,
            cid: 100 + episode_no,
        })
        .collect()
}

async fn scoped_fixture(start: i64) -> (Repository, i64) {
    let repo = Repository::connect(":memory:").await.unwrap();
    let id = repo
        .add_anime(NewAnime {
            title: "普通周更".into(),
            aliases: vec![],
            next_episode: start,
            expected_at: Some(Utc::now()),
            expected_weekday: None,
            expected_time: None,
            timezone: "Asia/Shanghai".into(),
            duration_min_sec: 1200,
            duration_max_sec: 2400,
            auto_schedule: Some(AutoScheduleMetadata {
                bangumi_subject_id: 542007,
                anilist_media_id: None,
                anime_schedule_route: None,
                total_episodes: Some(13),
                broadcast_pattern: "R/2026-10-03T13:00:00Z/P7D".into(),
                schedule_source: "anime_schedule".into(),
                schedule_confidence: "calibrated".into(),
                schedule_warning: None,
                next_sync_at: Utc::now(),
                episode_mapping: None,
            }),
        })
        .await
        .unwrap();
    (repo, id)
}

fn plan(first: i64, last: i64) -> ReleaseBatch {
    ReleaseBatch {
        first_episode: first,
        last_episode: last,
        expected_at: Some(Utc::now() - Duration::days(1)),
        aired: true,
        source: "anime_schedule".into(),
    }
}

#[tokio::test]
async fn weekly_schedule_facts_never_backfill_history_or_create_future_targets() {
    let (repo, id) = scoped_fixture(3).await;
    let plans = (1..=7).map(|n| plan(n, n)).collect::<Vec<_>>();
    for _ in 0..3 {
        repo.apply_release_batches(id, &plans).await.unwrap();
    }
    let numbers: Vec<i64> =
        sqlx::query_scalar("SELECT episode_no FROM episode WHERE anime_id=? ORDER BY episode_no")
            .bind(id)
            .fetch_all(&repo.pool)
            .await
            .unwrap();
    assert_eq!(numbers, vec![3]);
    assert_eq!(
        repo.get_anime(id)
            .await
            .unwrap()
            .anime
            .tracking_start_episode,
        3
    );
    assert!(!repo.has_release_batch(id).await.unwrap());
    assert_eq!(repo.detection_targets(id).await.unwrap().len(), 1);
    assert_eq!(
        repo.upcoming_releases(Utc::now() + Duration::days(7))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(repo.pending_notifications().await.unwrap().is_empty());
}

#[tokio::test]
async fn weekly_confirmation_creates_only_next_episode_using_its_own_verified_clock() {
    let (repo, id) = scoped_fixture(3).await;
    let expected = Utc::now() + Duration::days(4);
    let mut fourth = plan(4, 4);
    fourth.expected_at = Some(expected);
    fourth.aired = false;
    repo.apply_release_batches(id, &[plan(1, 1), plan(3, 3), fourth, plan(5, 5)])
        .await
        .unwrap();
    let episode = repo.active_episode(id).await.unwrap();
    sqlx::query("UPDATE episode SET state='confirmed',confirmed_at=? WHERE id=?")
        .bind(Utc::now())
        .bind(episode.id)
        .execute(&repo.pool)
        .await
        .unwrap();
    let notification=sqlx::query("INSERT INTO notification(episode_id,channel,status,created_at) VALUES(?,'default','pending',?)").bind(episode.id).bind(Utc::now()).execute(&repo.pool).await.unwrap().last_insert_rowid();
    repo.mark_notification_sent(notification).await.unwrap();
    let next = repo.active_episode(id).await.unwrap();
    assert_eq!(next.episode_no, 4);
    assert_eq!(next.expected_at, Some(expected));
    let numbers: Vec<i64> =
        sqlx::query_scalar("SELECT episode_no FROM episode WHERE anime_id=? ORDER BY episode_no")
            .bind(id)
            .fetch_all(&repo.pool)
            .await
            .unwrap();
    assert_eq!(numbers, vec![3, 4]);
}

#[tokio::test]
async fn premiere_double_release_before_tracking_start_is_only_metadata() {
    let (repo, id) = scoped_fixture(3).await;
    repo.apply_release_batches(id, &[plan(1, 2), plan(3, 3), plan(4, 4)])
        .await
        .unwrap();
    assert_eq!(
        repo.detection_targets(id)
            .await
            .unwrap()
            .iter()
            .map(|e| e.episode_no)
            .collect::<Vec<_>>(),
        vec![3]
    );
    assert!(repo.release_progress().await.unwrap().is_empty());
    assert!(!repo.has_release_batch(id).await.unwrap());
    assert_eq!(repo.active_episode(id).await.unwrap().episode_no, 3);
}

#[tokio::test]
async fn partial_release_has_its_own_denominator_and_disappears_once_watched() {
    let (repo, id) = scoped_fixture(1).await;
    repo.apply_release_batches(id, &[plan(1, 2), plan(3, 3), plan(4, 4)])
        .await
        .unwrap();
    let progress = repo.release_progress().await.unwrap().remove(0);
    assert_eq!((progress.total, progress.official), (2, 2));
    assert!(!progress.full_season);
    assert!(progress.label().contains("局部连播"));
    assert!(!progress.label().contains("全 13"));
    sqlx::query("UPDATE episode SET state='notified',watched_at=? WHERE anime_id=?")
        .bind(Utc::now())
        .bind(id)
        .execute(&repo.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO episode(anime_id,episode_no,state,next_check_at) VALUES(?,3,'waiting',?)",
    )
    .bind(id)
    .bind(Utc::now())
    .execute(&repo.pool)
    .await
    .unwrap();
    assert!(repo.release_progress().await.unwrap().is_empty());
    assert!(!repo.has_release_batch(id).await.unwrap());
    assert_eq!(repo.detection_targets(id).await.unwrap()[0].episode_no, 3);
}

#[tokio::test]
async fn full_drop_respects_tracking_start_without_losing_official_airing_count() {
    let (repo, id) = scoped_fixture(3).await;
    repo.apply_release_batches(id, &[plan(1, 13)])
        .await
        .unwrap();
    let targets = repo.detection_targets(id).await.unwrap();
    assert_eq!(targets.len(), 11);
    assert!(
        targets
            .iter()
            .all(|e| e.episode_no >= 3 && e.watched_at.is_none())
    );
    let progress = repo.release_progress().await.unwrap().remove(0);
    assert!(progress.full_season);
    assert_eq!(
        (progress.total, progress.official, progress.watched),
        (13, 13, 0)
    );
}

#[tokio::test]
async fn old_ghost_episode_cannot_rewind_tracking_or_offer_manual_confirmation() {
    let (repo, id) = scoped_fixture(3).await;
    let ghost=sqlx::query("INSERT INTO episode(anime_id,episode_no,state,next_check_at) VALUES(?,1,'needs_manual_review',?)").bind(id).bind(Utc::now()).execute(&repo.pool).await.unwrap().last_insert_rowid();
    repo.apply_release_batches(id, &[plan(1, 2), plan(3, 3)])
        .await
        .unwrap();
    assert_eq!(repo.active_episode(id).await.unwrap().episode_no, 3);
    assert!(!repo.independently_monitorable(id, ghost).await.unwrap());
    assert!(
        repo.upcoming_releases(Utc::now() + Duration::days(7))
            .await
            .unwrap()
            .iter()
            .all(|e| e.episode_no == 3)
    );
    let mut video = fixture().await.2.video;
    video.title = "普通周更 EP1".into();
    video.page_count = Some(1);
    video.duration_sec = 1500;
    let evaluation = crate::detector::evaluator::evaluate(
        &repo.get_anime(id).await.unwrap(),
        &repo.episode(ghost).await.unwrap(),
        &video,
        &UploaderTrust::default(),
        &crate::config::ConfirmationConfig::default(),
        &[],
    );
    repo.upsert_candidate(ghost, &video, &evaluation, CandidateState::Pending)
        .await
        .unwrap();
    repo.enqueue_review_notification(ghost, "default", "ghost")
        .await
        .unwrap();
    assert!(
        repo.list_candidates(Some("pending"))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        repo.pending_review_notifications("https://anime.example.com")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        repo.confirm_candidate(ghost, &video.bvid, "manual", "default", true)
            .await
            .is_err()
    );
    assert!(
        repo.reject_candidate_for_episode(ghost, &video.bvid, true)
            .await
            .is_err()
    );
    assert!(repo.reject_all_candidates(ghost, true).await.is_err());
    assert!(
        repo.reject_all_candidates_checked(ghost, "ghost")
            .await
            .is_err()
    );
    assert_eq!(
        repo.uploader_trust(id, video.uploader_mid)
            .await
            .unwrap()
            .rejected_count,
        0
    );
}

#[tokio::test]
async fn learning_the_final_episode_never_creates_unknown_old_history() {
    let (repo, id) = scoped_fixture(13).await;
    sqlx::query("UPDATE episode SET state='confirmed' WHERE anime_id=?")
        .bind(id)
        .execute(&repo.pool)
        .await
        .unwrap();
    let mut tx = repo.pool.begin().await.unwrap();
    assert!(
        !super::sqlite::transition_released_complete_if_final_released(&mut tx, id, Utc::now())
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM episode WHERE anime_id=?")
        .bind(id)
        .fetch_one(&repo.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(
        repo.get_anime(id).await.unwrap().anime.lifecycle,
        "tracking"
    );
}

#[tokio::test]
async fn migration_recovers_first_created_episode_not_a_newly_backfilled_lower_number() {
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
    migrations.migrations.to_mut().retain(|m| m.version < 20);
    migrations.run(&pool).await.unwrap();
    sqlx::query("INSERT INTO anime(title,duration_min_sec,duration_max_sec,created_at,updated_at) VALUES('legacy',1200,2400,?,?)").bind(Utc::now()).bind(Utc::now()).execute(&pool).await.unwrap();
    for number in [3, 4, 1, 2, 5] {
        sqlx::query(
            "INSERT INTO episode(anime_id,episode_no,state,next_check_at) VALUES(1,?,'waiting',?)",
        )
        .bind(number)
        .bind(Utc::now())
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::raw_sql(include_str!("../../migrations/0020_tracking_scope.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let start: i64 = sqlx::query_scalar("SELECT tracking_start_episode FROM anime WHERE id=1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(start, 3);
    let scope: Vec<i64> =
        sqlx::query_scalar("SELECT episode_no FROM monitorable_episode ORDER BY episode_no")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(scope, vec![3]);
    let changed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM episode WHERE state!='waiting' OR watched_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(changed, 0);
}

#[tokio::test]
async fn explicit_batch_materializes_all_episodes_without_resources_or_watch_progress() {
    let (repo, id, _) = fixture().await;
    let targets = repo.detection_targets(id).await.unwrap();
    assert_eq!(targets.len(), 10);
    assert_eq!(repo.due_anime_ids(20).await.unwrap(), vec![id]);
    assert!(
        targets.iter().all(
            |e| !matches!(e.state.as_str(), "confirmed" | "notified") && e.watched_at.is_none()
        )
    );
    assert!(
        repo.independently_monitorable(id, targets[9].id)
            .await
            .unwrap()
    );
    let p = repo.release_progress().await.unwrap().remove(0);
    assert_eq!((p.total, p.official, p.found, p.watched), (10, 10, 0, 0));
    repo.apply_release_batches(id, &[]).await.unwrap();
    assert_eq!(repo.detection_targets(id).await.unwrap().len(), 10);
}

#[tokio::test]
async fn narrow_delay_survives_a_repeated_broad_batch_sync() {
    let (repo, id, _) = fixture().await;
    repo.apply_release_batches(
        id,
        &[ReleaseBatch {
            first_episode: 4,
            last_episode: 4,
            expected_at: None,
            aired: false,
            source: "anime_schedule".into(),
        }],
    )
    .await
    .unwrap();
    repo.apply_release_batches(
        id,
        &[ReleaseBatch {
            first_episode: 1,
            last_episode: 10,
            expected_at: Some(Utc::now()),
            aired: true,
            source: "anime_schedule".into(),
        }],
    )
    .await
    .unwrap();
    let targets = repo.detection_targets(id).await.unwrap();
    assert_eq!(
        targets
            .iter()
            .find(|e| e.episode_no == 4)
            .unwrap()
            .expected_at,
        None
    );
    assert_eq!(targets.len(), 10);
    assert_eq!(repo.release_progress().await.unwrap()[0].official, 9);
}

#[tokio::test]
async fn collection_import_is_atomic_idempotent_and_keeps_only_selected_parts() {
    let (repo, id, collection) = fixture().await;
    draft(&repo, id, &collection, "draft").await;
    let mut invalid = selections();
    invalid[9].cid = 9999;
    assert!(
        repo.import_collection("draft", &collection, &invalid, "default")
            .await
            .is_err()
    );
    assert!(repo.list_episode_videos(id).await.unwrap().is_empty());
    for _ in 0..2 {
        repo.import_collection("draft", &collection, &selections(), "default")
            .await
            .unwrap();
    }
    let videos = repo.list_episode_videos(id).await.unwrap();
    assert_eq!(videos.len(), 10);
    assert!(videos.iter().all(|v| v.page_no.is_some_and(|p| p <= 10)));
    let p = repo.release_progress().await.unwrap().remove(0);
    assert_eq!((p.found, p.watched), (10, 0));
    let notifications = repo.pending_notifications().await.unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].url.as_deref().unwrap().contains("?p=1"));
    assert_eq!(notifications[0].episode_label(), "EP1～EP10（10集）");
    repo.mark_notification_sent(notifications[0].id)
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM episode WHERE anime_id=?")
        .bind(id)
        .fetch_one(&repo.pool)
        .await
        .unwrap();
    assert_eq!(count, 10);
}

#[tokio::test]
async fn reordered_pages_follow_cid_instead_of_old_page_number() {
    let (repo, id, mut collection) = fixture().await;
    draft(&repo, id, &collection, "reordered").await;
    for part in &mut collection.parts {
        part.page = 13 - part.page;
    }
    repo.import_collection("reordered", &collection, &selections(), "default")
        .await
        .unwrap();
    let videos = repo.list_episode_videos(id).await.unwrap();
    assert_eq!(
        videos.iter().find(|v| v.episode_no == 3).unwrap().page_no,
        Some(10)
    );
}

#[tokio::test]
async fn watching_finale_with_missing_episodes_cannot_stop_tracking_or_archive() {
    let (repo, id, collection) = fixture().await;
    draft(&repo, id, &collection, "last-only").await;
    repo.import_collection(
        "last-only",
        &collection,
        &[CollectionSelection {
            episode_no: 10,
            cid: 110,
        }],
        "default",
    )
    .await
    .unwrap();
    let notification = repo.pending_notifications().await.unwrap().remove(0);
    repo.mark_notification_sent(notification.id).await.unwrap();
    assert_eq!(
        repo.get_anime(id).await.unwrap().anime.lifecycle,
        "tracking"
    );
    let watched = repo
        .mark_episode_watched(notification.episode_id)
        .await
        .unwrap();
    assert!(watched.auto_archive.is_none());
    assert_eq!(repo.detection_targets(id).await.unwrap().len(), 9);
}

#[tokio::test]
async fn complete_batch_only_archives_after_every_episode_is_watched() {
    let (repo, id, collection) = fixture().await;
    draft(&repo, id, &collection, "complete").await;
    repo.import_collection("complete", &collection, &selections(), "default")
        .await
        .unwrap();
    let videos = repo.list_episode_videos(id).await.unwrap();
    for video in videos.iter().filter(|v| v.episode_no != 1) {
        assert!(
            repo.mark_episode_watched(video.episode_id)
                .await
                .unwrap()
                .auto_archive
                .is_none()
        );
    }
    assert_ne!(
        repo.get_anime(id).await.unwrap().anime.lifecycle,
        "archived"
    );
    let first = videos.iter().find(|v| v.episode_no == 1).unwrap();
    assert!(
        repo.mark_episode_watched(first.episode_id)
            .await
            .unwrap()
            .auto_archive
            .is_some()
    );
}

#[tokio::test]
async fn repeated_schedule_sync_preserves_resources_preferences_and_watch_timestamps() {
    let (repo, id, collection) = fixture().await;
    draft(&repo, id, &collection, "preserve").await;
    repo.import_collection("preserve", &collection, &selections(), "default")
        .await
        .unwrap();
    let video = repo.list_episode_videos(id).await.unwrap().remove(0);
    repo.set_episode_video_preferred(id, video.episode_id, Some(video.id))
        .await
        .unwrap();
    repo.mark_episode_watched(video.episode_id).await.unwrap();
    let before = repo.episode(video.episode_id).await.unwrap();
    repo.apply_release_batches(
        id,
        &[ReleaseBatch {
            first_episode: 1,
            last_episode: 10,
            expected_at: Some(Utc::now() + Duration::days(2)),
            aired: false,
            source: "anime_schedule".into(),
        }],
    )
    .await
    .unwrap();
    let after = repo.episode(video.episode_id).await.unwrap();
    assert_eq!(before.watched_at, after.watched_at);
    assert_eq!(before.expected_at, after.expected_at);
    assert!(
        repo.list_episode_videos(id)
            .await
            .unwrap()
            .iter()
            .find(|v| v.id == video.id)
            .unwrap()
            .is_preferred
    );
}
