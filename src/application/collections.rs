use super::*;
use crate::domain::{CollectionSelection, VideoCollection};

impl ApplicationService {
    async fn validate_collection_uploader(
        &self,
        anime_id: i64,
        collection: &VideoCollection,
    ) -> Result<()> {
        let trust = self
            .repository
            .uploader_trust(anime_id, collection.video.uploader_mid)
            .await?;
        if collection.video.uploader_mid <= 0 || trust.manually_blocked {
            return Err(AppError::InvalidInput(
                "合集 UP 信息不可用或已被屏蔽".into(),
            ));
        }
        let minimum = self.config.confirmation.minimum_uploader_followers;
        if minimum > 0
            && collection
                .video
                .uploader_follower_count
                .is_some_and(|count| count < minimum)
        {
            return Err(AppError::InvalidInput(format!(
                "合集 UP 粉丝不足 {minimum}，不能导入"
            )));
        }
        let title = normalize_title(&collection.video.title);
        if ["泄露", "泄漏", "偷跑", "枪版", "盗录"]
            .iter()
            .any(|word| title.contains(word))
        {
            return Err(AppError::InvalidInput(
                "合集标题包含泄露或盗录标记，请核实来源".into(),
            ));
        }
        for keyword in self.repository.list_blocked_keywords().await? {
            if title.contains(&keyword.normalized_keyword) {
                return Err(AppError::InvalidInput("合集命中了已屏蔽关键词".into()));
            }
        }
        Ok(())
    }

    pub async fn resolve_collection_preview(&self, id: &str) -> Result<()> {
        let draft = self.repository.collection_draft(id).await?;
        let bvid = parse_bilibili_bvid(&draft.url)?;
        let provider =
            BilibiliProvider::new(self.config.bilibili.clone(), self.repository.clone())?;
        let metadata = provider.collection(&bvid).await?;
        self.validate_collection_uploader(draft.anime_id, &metadata)
            .await?;
        self.repository.ready_collection_draft(id, &metadata).await
    }

    pub async fn import_collection_preview(
        &self,
        id: &str,
        selections: &[CollectionSelection],
    ) -> Result<()> {
        let draft = self.repository.collection_draft(id).await?;
        if draft.state == "completed" {
            return Ok(());
        }
        let preview: VideoCollection = serde_json::from_str(
            draft
                .metadata_json
                .as_deref()
                .ok_or_else(|| AppError::InvalidInput("缺少合集预览".into()))?,
        )
        .map_err(|_| AppError::InvalidInput("合集预览数据损坏".into()))?;
        let bvid = parse_bilibili_bvid(&draft.url)?;
        let provider =
            BilibiliProvider::new(self.config.bilibili.clone(), self.repository.clone())?;
        let fresh = provider.collection(&bvid).await?;
        self.validate_collection_uploader(draft.anime_id, &fresh)
            .await?;
        if preview.video.bvid != fresh.video.bvid
            || preview.video.uploader_mid != fresh.video.uploader_mid
        {
            return Err(AppError::InvalidInput(
                "合集来源已经变化，请重新预览".into(),
            ));
        }
        let anime = self.repository.get_anime(draft.anime_id).await?.anime;
        for selection in selections {
            let old = preview.parts.iter().find(|p| p.cid == selection.cid);
            let new = fresh.parts.iter().find(|p| p.cid == selection.cid);
            if old.is_none()
                || new.is_none_or(|p| {
                    p.duration_sec < anime.duration_min_sec * 60 / 100
                        || p.duration_sec > anime.duration_max_sec.saturating_mul(2)
                })
            {
                return Err(AppError::InvalidInput(
                    "分 P 已删除或时长异常，请重新预览并核实内容".into(),
                ));
            }
        }
        self.repository
            .import_collection(id, &fresh, selections, &self.config.notification.channel)
            .await
    }
}
