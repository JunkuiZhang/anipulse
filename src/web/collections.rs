use super::*;
use crate::domain::{CollectionSelection, VideoCollection};
use std::collections::HashMap;

#[derive(Deserialize)]
pub(super) struct StartForm {
    csrf_token: String,
    url: String,
}

pub(super) async fn start(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(anime_id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<StartForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    parse_bilibili_bvid(&form.url)?;
    let id = uuid::Uuid::new_v4().to_string();
    state
        .repository
        .create_collection_draft(&id, anime_id, identity.admin_id, &form.url)
        .await?;
    let payload = serde_json::json!({"purpose":"collection_preview","draft_id":id,"url":form.url})
        .to_string();
    state
        .application
        .enqueue_job(
            ManagementJobKind::AcceptBilibiliUrl,
            Some("anime"),
            Some(&anime_id.to_string()),
            &payload,
            Some(identity.admin_id),
            Some(&format!("collection_preview:{id}")),
        )
        .await?;
    Ok(Redirect::to(&format!("/collections/{id}")).into_response())
}

struct PartView {
    page: i64,
    title: String,
    duration: String,
    suggested: i64,
    url: String,
}
#[derive(Template)]
#[template(path = "collection_import.html")]
struct PreviewTemplate {
    username: String,
    csrf_token: String,
    id: String,
    anime_id: i64,
    title: String,
    nonce: String,
    queued: bool,
    ready: bool,
    completed: bool,
    error: String,
    parts: Vec<PartView>,
    source_url: String,
    uploader: String,
    followers: String,
    published_at: String,
}

pub(super) fn suggested_episode(title: &str, total: Option<i64>) -> i64 {
    let normalized = normalize_part(title);
    let regex=regex::Regex::new(r"(?i)^(?:第[0-9一二三四五六七八九十]+季\s*)?(?:(?:episode|ep)\s*|第)?([0-9]{1,3})(?:[集话話])?$").unwrap();
    regex
        .captures(&normalized)
        .and_then(|c| c[1].parse::<i64>().ok())
        .filter(|n| *n > 0 && total.is_none_or(|total| *n <= total))
        .unwrap_or(0)
}
fn normalize_part(title: &str) -> String {
    title.trim().to_string()
}

pub(super) async fn preview(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<String>,
) -> WebResponse {
    let draft = state.repository.collection_draft(&id).await?;
    if draft.admin_id != identity.admin_id {
        return Err(WebError::not_found("未找到合集预览"));
    }
    let anime = state.repository.get_anime(draft.anime_id).await?.anime;
    let metadata = draft
        .metadata_json
        .as_deref()
        .and_then(|v| serde_json::from_str::<VideoCollection>(v).ok());
    let parts = metadata
        .as_ref()
        .map(|m| {
            m.parts
                .iter()
                .map(|p| PartView {
                    page: p.page,
                    title: p.title.clone(),
                    duration: format_duration(p.duration_sec),
                    suggested: suggested_episode(&p.title, anime.total_episodes),
                    url: canonical_bilibili_part_url(&m.video.bvid, Some(p.page))
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    let nonce = state
        .auth
        .issue_action_nonce(&identity, "collection.import", Some(&id))
        .await?;
    render(PreviewTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        id,
        anime_id: draft.anime_id,
        source_url: metadata
            .as_ref()
            .and_then(|m| canonical_bilibili_part_url(&m.video.bvid, None))
            .unwrap_or_default(),
        uploader: metadata
            .as_ref()
            .map(|m| m.video.uploader_name.clone())
            .unwrap_or_default(),
        followers: metadata
            .as_ref()
            .and_then(|m| m.video.uploader_follower_count)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "未知，请核实来源".into()),
        published_at: metadata
            .as_ref()
            .map(|m| format_time(Some(m.video.published_at), state.display_timezone))
            .unwrap_or_default(),
        title: metadata.map(|m| m.video.title).unwrap_or(anime.title),
        nonce,
        queued: matches!(draft.state.as_str(), "queued" | "importing"),
        ready: draft.state == "ready",
        completed: draft.state == "completed",
        error: draft.error.unwrap_or_default(),
        parts,
    })
}

pub(super) async fn confirm(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> WebResponse {
    validate_write(
        &state,
        &identity,
        &headers,
        form.get("csrf_token").map(String::as_str).unwrap_or(""),
    )?;
    let draft = state.repository.collection_draft(&id).await?;
    if draft.admin_id != identity.admin_id || draft.state != "ready" {
        return Err(WebError::forbidden(
            "预览不属于当前账号、尚未就绪或已经使用",
        ));
    }
    let metadata: VideoCollection =
        serde_json::from_str(draft.metadata_json.as_deref().unwrap_or(""))
            .map_err(|_| WebError::internal())?;
    let mut seen = std::collections::HashSet::new();
    let mut selections = Vec::new();
    for part in metadata.parts {
        let number = form
            .get(&format!("episode_{}", part.page))
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        if number == 0 {
            continue;
        }
        if !(1..=10000).contains(&number) || !seen.insert(number) {
            return Err(WebError::forbidden("集数重复或超出范围"));
        }
        selections.push(CollectionSelection {
            episode_no: number,
            cid: part.cid,
        });
    }
    if selections.is_empty() {
        return Err(WebError::forbidden("请至少选择一集；填 0 表示跳过该 P"));
    }
    if !state
        .auth
        .consume_action_nonce(
            &identity,
            form.get("nonce").map(String::as_str).unwrap_or(""),
            "collection.import",
            Some(&id),
        )
        .await?
    {
        return Err(WebError::forbidden("确认已过期或使用，请刷新预览"));
    }
    let payload=serde_json::json!({"purpose":"collection_import","draft_id":id,"url":draft.url,"selections":selections}).to_string();
    let job = state
        .application
        .enqueue_job(
            ManagementJobKind::AcceptBilibiliUrl,
            Some("anime"),
            Some(&draft.anime_id.to_string()),
            &payload,
            Some(identity.admin_id),
            Some(&format!("collection_import:{id}")),
        )
        .await?;
    audit_success(
        &state,
        &identity,
        "collection.import.enqueue",
        "management_job",
        job,
        serde_json::json!({"draft_id":id}),
    )
    .await?;
    Ok(Redirect::to("/jobs").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_suggestions_require_a_clear_episode_and_skip_non_episode_parts() {
        for (title, expected) in [
            ("第二季1", 1),
            ("EP10", 10),
            ("第3话", 3),
            ("01", 1),
            ("已完结", 0),
            ("点点关注不迷路", 0),
            ("全10集", 0),
            ("11", 0),
        ] {
            assert_eq!(suggested_episode(title, Some(10)), expected, "{title}");
        }
    }

    #[test]
    fn preview_escapes_metadata_and_exposes_per_part_links() {
        let html = PreviewTemplate {
            username: "admin".into(),
            csrf_token: "csrf".into(),
            id: "draft".into(),
            anime_id: 18,
            title: "<script>bad</script>".into(),
            nonce: "once".into(),
            queued: false,
            ready: true,
            completed: false,
            error: String::new(),
            source_url: "https://www.bilibili.com/video/BV1nwnc6WEc9".into(),
            uploader: "test".into(),
            followers: "1000".into(),
            published_at: "2026-10-20 15:00:00".into(),
            parts: vec![PartView {
                page: 3,
                title: "<b>EP3</b>".into(),
                duration: "25:00".into(),
                suggested: 3,
                url: "https://www.bilibili.com/video/BV1nwnc6WEc9?p=3".into(),
            }],
        }
        .render()
        .unwrap();
        assert!(!html.contains("<script>bad"));
        assert!(html.contains("?p=3"));
        assert!(html.contains("name=\"nonce\" value=\"once\""));
        assert!(html.contains("name=\"episode_3\""));
    }
}
