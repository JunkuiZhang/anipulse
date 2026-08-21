use std::sync::Arc;

use chrono::Utc;
use tokio::time::Duration;
use tracing::{error, info, warn};

use crate::{
    application::ApplicationService,
    config::AppConfig,
    detector::Detector,
    error::{AppError, Result},
    notification::NotificationDispatcher,
    repository::{ManagementJob, Repository},
    schedule::ScheduleSynchronizer,
};

pub async fn run(
    repository: Repository,
    detector: Detector,
    dispatcher: NotificationDispatcher,
    config: Arc<AppConfig>,
) -> Result<()> {
    let schedule = ScheduleSynchronizer::new(repository.clone(), config.schedule.clone())?;
    let application = ApplicationService::new(repository.clone(), config.clone());
    info!(tick_secs = config.scheduler.tick_secs, "scheduler started");
    loop {
        repository.update_scheduler_heartbeat().await?;
        let recovered = repository
            .recover_stale_management_jobs(Utc::now() - chrono::Duration::minutes(10))
            .await?;
        if recovered > 0 {
            warn!(recovered, "requeued stale management jobs");
        }
        for job in repository
            .claim_management_jobs(config.scheduler.management_job_batch_size)
            .await?
        {
            if let Err(error) = execute_management_job(
                &job,
                &repository,
                &application,
                &detector,
                &dispatcher,
                &schedule,
            )
            .await
            {
                repository
                    .fail_management_job(job.id, &error.to_string())
                    .await?;
                warn!(job_id = job.id, kind = %job.kind, %error, "management job failed");
            } else {
                repository.complete_management_job(job.id).await?;
                info!(job_id = job.id, kind = %job.kind, "management job completed");
            }
        }
        if let Err(error) = schedule.sync_due().await {
            warn!(%error, "automatic schedule synchronization failed");
        }
        let due = repository
            .due_anime_ids(config.scheduler.due_batch_size)
            .await?;
        for anime_id in due {
            if let Err(error) = detector.check_anime(anime_id).await {
                warn!(anime_id, %error, "episode check failed");
                if let Ok(episode) = repository.active_episode(anime_id).await {
                    let retry_at = Utc::now() + chrono::Duration::minutes(15);
                    if let Err(reschedule_error) =
                        repository.reschedule_episode(episode.id, retry_at).await
                    {
                        error!(anime_id, %reschedule_error, "failed to reschedule episode");
                    }
                }
            }
        }
        dispatcher.dispatch_pending().await?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    warn!(%error, "failed to listen for shutdown signal");
                }
                info!("shutdown signal received");
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_secs(config.scheduler.tick_secs)) => {}
        }
    }
}

async fn execute_management_job(
    job: &ManagementJob,
    repository: &Repository,
    application: &ApplicationService,
    detector: &Detector,
    dispatcher: &NotificationDispatcher,
    schedule: &ScheduleSynchronizer,
) -> Result<()> {
    match job.kind.as_str() {
        "check_anime" => {
            let anime_id = target_id(job)?;
            let anime = repository.get_anime(anime_id).await?;
            if !anime.anime.enabled {
                return Err(AppError::InvalidInput(format!(
                    "anime {anime_id} is disabled"
                )));
            }
            detector.check_anime(anime_id).await
        }
        "sync_schedule" => schedule.sync_now(target_id(job)?).await,
        "notification_test" => dispatcher.test().await,
        "accept_bilibili_url" => {
            let anime_id = target_id(job)?;
            let payload: serde_json::Value = serde_json::from_str(&job.payload_json)
                .map_err(|_| AppError::InvalidInput("job payload is invalid".into()))?;
            let url = payload
                .get("url")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| AppError::InvalidInput("job payload has no URL".into()))?;
            let episode_id = payload
                .get("episode_id")
                .and_then(serde_json::Value::as_i64)
                .ok_or_else(|| AppError::InvalidInput("job payload has no episode ID".into()))?;
            application
                .import_bilibili_url_for_episode(anime_id, episode_id, url)
                .await?;
            Ok(())
        }
        "resolve_anime_draft" => {
            let draft_id = job
                .target_id
                .as_deref()
                .ok_or_else(|| AppError::InvalidInput("draft job has no target ID".into()))?;
            if let Err(error) = application.resolve_anime_draft(draft_id).await {
                repository
                    .mark_anime_draft_failed(draft_id, &error.to_string())
                    .await?;
                return Err(error);
            }
            Ok(())
        }
        kind => Err(AppError::InvalidInput(format!(
            "unsupported management job kind: {kind}"
        ))),
    }
}

fn target_id(job: &ManagementJob) -> Result<i64> {
    job.target_id
        .as_deref()
        .ok_or_else(|| AppError::InvalidInput("management job has no target ID".into()))?
        .parse()
        .map_err(|_| AppError::InvalidInput("management job target ID is invalid".into()))
}
