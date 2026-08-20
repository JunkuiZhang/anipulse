use std::sync::Arc;

use chrono::Utc;
use tokio::time::Duration;
use tracing::{error, info, warn};

use crate::{
    config::AppConfig, detector::Detector, error::Result, notification::NotificationDispatcher,
    repository::Repository, schedule::ScheduleSynchronizer,
};

pub async fn run(
    repository: Repository,
    detector: Detector,
    dispatcher: NotificationDispatcher,
    config: Arc<AppConfig>,
) -> Result<()> {
    let schedule = ScheduleSynchronizer::new(repository.clone(), config.schedule.clone())?;
    info!(tick_secs = config.scheduler.tick_secs, "scheduler started");
    loop {
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
