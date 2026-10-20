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
