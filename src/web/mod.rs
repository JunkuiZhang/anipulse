use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

mod cover_cache;

use askama::Template;
use axum::{
    Extension, Form, Router,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::{DateTime, Datelike, Utc, Weekday};
use chrono_tz::Tz;
use serde::Deserialize;
use tower_http::{
    catch_panic::CatchPanicLayer,
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

use crate::{
    application::{
        AnimeDraftRequest, AnimeDraftResolution, ApplicationService, ManagementJobKind,
        parse_bilibili_bvid,
    },
    auth::{AuthService, LoginOutcome, SessionIdentity},
    config::AppConfig,
    domain::{EpisodeNumberMapping, Evaluation},
    error::{AppError, Result},
    repository::{
        AuditEventRow, CandidateListRow, EpisodeVideoRow, ManagementJobListRow, Repository,
    },
};
use cover_cache::{CoverAsset, CoverCache};

const SESSION_COOKIE: &str = "__Host-anipulse_session";
const APP_CSS: &str = include_str!("../../static/app.css");
const APP_FAVICON: &str = include_str!("../../static/favicon.svg");

#[derive(Clone)]
struct WebState {
    repository: Repository,
    application: ApplicationService,
    auth: AuthService,
    config: Arc<AppConfig>,
    public_origin: String,
    display_timezone: Tz,
    cover_cache: CoverCache,
}

pub async fn serve(repository: Repository, config: Arc<AppConfig>) -> Result<()> {
    repository.cleanup_web_ephemera().await?;
    let cover_cache = CoverCache::new(&config)?;
    cover_cache.initialize(&repository).await?;
    let auth = AuthService::from_env(repository.clone(), config.web.clone()).await?;
    let public_origin = origin_of(&config.web.public_url)?;
    let display_timezone = config
        .web
        .timezone
        .parse::<Tz>()
        .map_err(|_| AppError::Config("web.timezone is invalid".into()))?;
    let state = WebState {
        application: ApplicationService::new(repository.clone(), config.clone()),
        repository: repository.clone(),
        auth,
        config: config.clone(),
        public_origin,
        display_timezone,
        cover_cache: cover_cache.clone(),
    };
    spawn_web_cleanup(repository.clone(), cover_cache);
    let app = build_router(state, &config);

    let address = config
        .web
        .bind
        .parse::<SocketAddr>()
        .map_err(|_| AppError::Config("web.bind is invalid".into()))?;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| {
            AppError::Config(format!("cannot bind web server to {address}: {error}"))
        })?;
    tracing::info!(%address, public_url = %config.web.public_url, "web server started");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .map_err(|error| AppError::Config(format!("web server stopped: {error}")))
}

fn spawn_web_cleanup(repository: Repository, cover_cache: CoverCache) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3_600));
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = repository.cleanup_web_ephemera().await {
                tracing::warn!(%error, "web state cleanup failed");
            }
            if let Err(error) = cover_cache.prune(&repository).await {
                tracing::warn!(%error, "cover cache cleanup failed");
            }
        }
    });
}

fn build_router(state: WebState, config: &AppConfig) -> Router {
    let protected = Router::new()
        .route("/", get(dashboard))
        .route("/anime", get(anime_list).post(anime_create))
        .route("/anime/new", get(anime_new))
        .route("/anime/resolve", post(anime_resolve))
        .route("/anime/drafts/{id}", get(anime_draft))
        .route("/anime/{id}", get(anime_detail))
        .route("/anime/{id}/title", post(anime_title))
        .route("/anime/{id}/enable", post(anime_enable))
        .route("/anime/{id}/disable", post(anime_disable))
        .route("/anime/{id}/check", post(anime_check))
        .route("/anime/{id}/sync", post(anime_sync))
        .route("/anime/{id}/episode-mapping", post(anime_episode_mapping))
        .route(
            "/anime/{id}/released-complete",
            post(anime_released_complete),
        )
        .route("/anime/{id}/resume", post(anime_resume_tracking))
        .route("/anime/{id}/archive-intent", post(anime_archive_intent))
        .route("/anime/{id}/archive", post(anime_archive))
        .route("/anime/{id}/history/videos", post(history_video_add))
        .route(
            "/anime/{id}/history/videos/{video_id}/update",
            post(history_video_update),
        )
        .route(
            "/anime/{id}/history/episodes/{episode_id}/videos/{video_id}/prefer",
            post(history_video_prefer),
        )
        .route(
            "/anime/{id}/history/episodes/{episode_id}/preferred/clear",
            post(history_video_preferred_clear),
        )
        .route("/anime/{id}/delete-intent", post(anime_delete_intent))
        .route("/anime/{id}/delete", post(anime_delete))
        .route("/covers/{subject_id}", get(cover_image))
        .route("/candidates", get(candidate_list))
        .route("/rules", get(rule_list))
        .route("/rules/keywords", post(keyword_create))
        .route("/rules/keywords/{id}", post(keyword_update))
        .route("/rules/keywords/{id}/delete", post(keyword_delete))
        .route(
            "/rules/uploaders/global",
            post(global_trusted_uploader_create),
        )
        .route(
            "/rules/uploaders/global/{mid}",
            post(global_trusted_uploader_update),
        )
        .route(
            "/rules/uploaders/global/{mid}/delete",
            post(global_trusted_uploader_delete),
        )
        .route("/rules/uploaders", post(trusted_uploader_create))
        .route(
            "/rules/uploaders/{anime_id}/{mid}",
            post(trusted_uploader_update),
        )
        .route(
            "/rules/uploaders/{anime_id}/{mid}/delete",
            post(trusted_uploader_delete),
        )
        .route(
            "/rules/uploaders/{anime_id}/{mid}/promote",
            post(trusted_uploader_promote),
        )
        .route(
            "/episodes/{episode_id}/candidates/{bvid}/accept-intent",
            post(candidate_accept_intent),
        )
        .route(
            "/episodes/{episode_id}/candidates/{bvid}/accept",
            post(candidate_accept),
        )
        .route(
            "/episodes/{episode_id}/candidates/{bvid}/reject",
            post(candidate_reject),
        )
        .route("/review/episodes/{id}", get(review_episode))
        .route(
            "/episodes/{id}/candidates/reject-all-intent",
            post(reject_all_intent),
        )
        .route("/episodes/{id}/candidates/reject-all", post(reject_all))
        .route("/episodes/{id}/watched", post(episode_watched))
        .route(
            "/episodes/{id}/candidates/from-url",
            post(candidate_from_url),
        )
        .route(
            "/anime/{anime_id}/uploaders/{mid}/trust",
            post(uploader_trust),
        )
        .route(
            "/anime/{anime_id}/uploaders/{mid}/trust-global",
            post(uploader_trust_global),
        )
        .route(
            "/anime/{anime_id}/uploaders/{mid}/block",
            post(uploader_block),
        )
        .route("/jobs", get(job_list))
        .route("/audit", get(audit_list))
        .route("/settings/status", get(settings_status))
        .route("/notification/test", post(notification_test))
        .route("/logout", post(logout))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));
    Router::new()
        .route("/healthz", get(healthz))
        .route("/static/app.css", get(stylesheet))
        .route("/static/favicon.svg", get(favicon))
        .route("/favicon.ico", get(favicon))
        .route("/login", get(login_page).post(login_submit))
        .merge(protected)
        .fallback(not_found)
        .with_state(state)
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TraceLayer::new_for_http())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(config.web.request_timeout_secs),
        ))
        .layer(RequestBodyLimitLayer::new(config.web.max_body_bytes))
        .layer(CatchPanicLayer::new())
        .layer(middleware::from_fn(security_headers))
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "failed to listen for web shutdown signal");
    }
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    insert_header(
        headers,
        "content-security-policy",
        "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: https://i0.hdslb.com https://i1.hdslb.com https://i2.hdslb.com; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
    );
    insert_header(headers, "x-content-type-options", "nosniff");
    insert_header(
        headers,
        "referrer-policy",
        "strict-origin-when-cross-origin",
    );
    insert_header(
        headers,
        "permissions-policy",
        "camera=(), microphone=(), geolocation=()",
    );
    insert_header(headers, "cross-origin-opener-policy", "same-origin");
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: &'static str) {
    headers.insert(name, HeaderValue::from_static(value));
}

async fn require_auth(State(state): State<WebState>, mut request: Request, next: Next) -> Response {
    let Some(token) = cookie_value(request.headers(), SESSION_COOKIE) else {
        return Redirect::to("/login").into_response();
    };
    let identity = match state.auth.authenticate(&token).await {
        Ok(Some(identity)) => identity,
        Ok(None) => return clear_session_redirect(&state),
        Err(error) => return WebError::from(error).into_response(),
    };
    let renewed_token = identity.renewed_token.clone();
    request.extensions_mut().insert(identity);
    let mut response = next.run(request).await;
    if let Some(token) = renewed_token
        && let Ok(value) = HeaderValue::from_str(&session_cookie(&state, &token))
    {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn stylesheet() -> Response {
    let mut response = APP_CSS.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/css; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

async fn favicon() -> Response {
    let mut response = APP_FAVICON.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/svg+xml; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=604800"),
    );
    response
}

async fn cover_image(
    State(state): State<WebState>,
    Path(subject_id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if subject_id <= 0 {
        return StatusCode::NOT_FOUND.into_response();
    }
    match state.repository.has_bangumi_subject_id(subject_id).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            tracing::warn!(subject_id, %error, "failed to authorize cover cache lookup");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    match state.cover_cache.get(subject_id).await {
        Ok(asset) => cover_response(asset, &headers),
        Err(error) => {
            tracing::warn!(subject_id, %error, "cover unavailable; serving placeholder");
            cover_placeholder_response()
        }
    }
}

fn cover_response(asset: CoverAsset, request_headers: &HeaderMap) -> Response {
    let not_modified = request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|value| value == "*" || value == asset.etag)
        });
    let mut response = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        asset.bytes.into_response()
    };
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=86400"),
    );
    if let Ok(value) = HeaderValue::from_str(&asset.etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

fn cover_placeholder_response() -> Response {
    const PLACEHOLDER: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 320 440"><defs><linearGradient id="g" x2="1" y2="1"><stop stop-color="#f0e8e4"/><stop offset="1" stop-color="#dfe9e7"/></linearGradient></defs><rect width="320" height="440" rx="24" fill="url(#g)"/><circle cx="160" cy="196" r="62" fill="#fff" fill-opacity=".65"/><path d="M128 201h64M160 169v64" stroke="#8f817b" stroke-width="12" stroke-linecap="round"/><text x="160" y="310" text-anchor="middle" font-family="sans-serif" font-size="25" fill="#756b67">封面稍后重试</text></svg>"##;
    let mut response = PLACEHOLDER.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/svg+xml; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    initialized: bool,
    message: String,
}

async fn login_page(State(state): State<WebState>, headers: HeaderMap) -> WebResponse {
    if let Some(token) = cookie_value(&headers, SESSION_COOKIE)
        && let Some(identity) = state.auth.authenticate(&token).await?
    {
        let mut response = Redirect::to("/").into_response();
        if let Some(token) = identity.renewed_token {
            response.headers_mut().append(
                header::SET_COOKIE,
                HeaderValue::from_str(&session_cookie(&state, &token))
                    .map_err(|_| WebError::internal())?,
            );
        }
        return Ok(response);
    }
    render(LoginTemplate {
        initialized: state.repository.web_admin_count().await? > 0,
        message: String::new(),
    })
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn login_submit(
    State(state): State<WebState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> WebResponse {
    if !valid_request_origin(&state, &headers) {
        return Err(WebError::forbidden("请求来源校验失败"));
    }
    let source = effective_source_ip(&state, peer.ip(), &headers).to_string();
    match state
        .auth
        .login(
            &form.username,
            form.password,
            &source,
            headers
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok()),
        )
        .await?
    {
        LoginOutcome::Success(session) => {
            let mut response = Redirect::to("/").into_response();
            response.headers_mut().append(
                header::SET_COOKIE,
                HeaderValue::from_str(&session_cookie(&state, &session.token))
                    .map_err(|_| WebError::internal())?,
            );
            Ok(response)
        }
        LoginOutcome::Invalid => render_with_status(
            LoginTemplate {
                initialized: true,
                message: "用户名或密码错误".into(),
            },
            StatusCode::UNAUTHORIZED,
        ),
        LoginOutcome::Blocked(until) => render_with_status(
            LoginTemplate {
                initialized: true,
                message: format!(
                    "登录尝试过多，请在 {} 后重试",
                    format_time(Some(until), state.display_timezone)
                ),
            },
            StatusCode::TOO_MANY_REQUESTS,
        ),
        LoginOutcome::Uninitialized => render_with_status(
            LoginTemplate {
                initialized: false,
                message: "尚未创建管理员".into(),
            },
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    }
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    username: String,
    csrf_token: String,
    anime_count: i64,
    enabled_anime_count: i64,
    pending_candidate_count: i64,
    pending_notification_count: i64,
    failed_notification_count: i64,
    queued_job_count: i64,
    failed_job_count: i64,
    scheduler_status: String,
    provider_status: String,
    upcoming: Vec<UpcomingReleaseView>,
    watch_queue: Vec<WatchQueueView>,
    notice: String,
}

struct UpcomingReleaseView {
    anime_id: i64,
    title: String,
    cover_url: String,
    has_cover: bool,
    cover_initial: String,
    episode: String,
    day: String,
    time: String,
    enabled: bool,
}

struct WatchQueueView {
    episode_id: i64,
    anime_id: i64,
    title: String,
    episode: String,
    video_title: String,
    url: String,
    updated_at: String,
    cover_url: String,
    has_cover: bool,
    cover_initial: String,
}

#[derive(Deserialize, Default)]
struct DashboardQuery {
    result: Option<String>,
}

async fn dashboard(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Query(query): Query<DashboardQuery>,
) -> WebResponse {
    let stats = state.repository.dashboard_stats().await?;
    let now = Utc::now();
    let upcoming = state
        .repository
        .upcoming_releases(now, now + chrono::Duration::days(7))
        .await?
        .into_iter()
        .map(|release| {
            let local = release.expected_at.with_timezone(&state.display_timezone);
            let cover_url = release.bangumi_subject_id.and_then(bangumi_cover_url);
            UpcomingReleaseView {
                anime_id: release.anime_id,
                cover_initial: title_initial(&release.title),
                title: release.title,
                has_cover: cover_url.is_some(),
                cover_url: cover_url.unwrap_or_default(),
                episode: format!("EP{}", release.episode_no),
                day: format!(
                    "{} · {}",
                    local.format("%m月%d日"),
                    chinese_weekday(local.weekday())
                ),
                time: if release.schedule_confidence.as_deref() == Some("date_only") {
                    "时间待公布".into()
                } else {
                    local.format("%H:%M").to_string()
                },
                enabled: release.enabled,
            }
        })
        .collect();
    let watch_queue = state
        .repository
        .watch_queue(50)
        .await?
        .into_iter()
        .filter_map(|episode| {
            let url = canonical_bilibili_url(&episode.bvid)?;
            let cover_url = episode.bangumi_subject_id.and_then(bangumi_cover_url);
            Some(WatchQueueView {
                episode_id: episode.episode_id,
                anime_id: episode.anime_id,
                cover_initial: title_initial(&episode.anime_title),
                title: episode.anime_title,
                episode: format!("EP{}", episode.episode_no),
                video_title: clean_bilibili_title(&episode.video_title),
                url,
                updated_at: episode
                    .released_at
                    .with_timezone(&state.display_timezone)
                    .format("%Y-%m-%d %H:%M")
                    .to_string(),
                has_cover: cover_url.is_some(),
                cover_url: cover_url.unwrap_or_default(),
            })
        })
        .collect();
    let notice = match query.result.as_deref() {
        Some("episode-watched") => "已标记为已观看。",
        Some("episode-watched-archived") => "最后一集已经看完，本季已自动归档收藏。",
        _ => "",
    }
    .to_string();
    render(DashboardTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        anime_count: stats.anime_count,
        enabled_anime_count: stats.enabled_anime_count,
        pending_candidate_count: stats.pending_candidate_count,
        pending_notification_count: stats.pending_notification_count,
        failed_notification_count: stats.failed_notification_count,
        queued_job_count: stats.queued_job_count,
        failed_job_count: stats.failed_job_count,
        scheduler_status: heartbeat_status(stats.scheduler_heartbeat, state.display_timezone),
        provider_status: provider_status(stats.provider_backoff_until, state.display_timezone),
        upcoming,
        watch_queue,
        notice,
    })
}

async fn episode_watched(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(episode_id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let outcome = state.repository.mark_episode_watched(episode_id).await?;
    let auto_archived = outcome.auto_archive.is_some();
    audit_success(
        &state,
        &identity,
        "episode.watched",
        "episode",
        episode_id,
        serde_json::json!({
            "anime_id": outcome.anime_id,
            "episode_no": outcome.episode_no,
            "auto_archived": auto_archived,
        }),
    )
    .await?;
    let result = if auto_archived {
        "episode-watched-archived"
    } else {
        "episode-watched"
    };
    Ok(Redirect::to(&format!("/?result={result}#watch-queue")).into_response())
}

struct AnimeListItem {
    id: i64,
    display_no: usize,
    title: String,
    cover_url: String,
    has_cover: bool,
    cover_initial: String,
    episode: String,
    time_label: String,
    time_value: String,
    schedule: String,
    enabled: bool,
    released_complete: bool,
    archived: bool,
}

#[derive(Template)]
#[template(path = "anime_list.html")]
struct AnimeListTemplate {
    username: String,
    csrf_token: String,
    items: Vec<AnimeListItem>,
    current_selected: bool,
    archived_selected: bool,
    notice: String,
}

#[derive(Deserialize, Default)]
struct AnimeListQuery {
    view: Option<String>,
    result: Option<String>,
}

async fn anime_list(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Query(query): Query<AnimeListQuery>,
) -> WebResponse {
    let archived_selected = match query.view.as_deref().unwrap_or("current") {
        "current" => false,
        "archived" => true,
        _ => return Err(WebError::bad_request("无效的追番视图")),
    };
    let anime_rows = if archived_selected {
        state.repository.list_archived_anime().await?
    } else {
        state.repository.list_anime().await?
    };
    let mut items = Vec::new();
    for (index, anime) in anime_rows.into_iter().enumerate() {
        let episode = if anime.lifecycle == "tracking" {
            state.repository.active_episode(anime.id).await.ok()
        } else {
            None
        };
        let cover_url = anime.bangumi_subject_id.and_then(bangumi_cover_url);
        let cover_initial = title_initial(&anime.title);
        items.push(AnimeListItem {
            id: anime.id,
            display_no: index + 1,
            title: anime.title,
            has_cover: cover_url.is_some(),
            cover_url: cover_url.unwrap_or_default(),
            cover_initial,
            episode: if anime.lifecycle == "archived" {
                anime
                    .total_episodes
                    .map(|total| format!("全 {total} 集 · 已收藏"))
                    .unwrap_or_else(|| "已归档收藏".into())
            } else if anime.lifecycle == "released_complete" {
                anime
                    .total_episodes
                    .map(|total| format!("全 {total} 集 · 待看完"))
                    .unwrap_or_else(|| "已播完 · 待看完".into())
            } else {
                episode
                    .as_ref()
                    .map(|episode| {
                        format!(
                            "EP{} · {}",
                            episode.episode_no,
                            episode_state_label(&episode.state)
                        )
                    })
                    .unwrap_or_else(|| "—".into())
            },
            time_label: if anime.lifecycle == "archived" {
                "归档时间".into()
            } else if anime.lifecycle == "released_complete" {
                "更新状态".into()
            } else {
                "预计更新".into()
            },
            time_value: if anime.lifecycle == "released_complete" {
                "本季已播完".into()
            } else if anime.lifecycle == "archived" {
                anime
                    .archived_at
                    .map(|value| format_time(Some(value), state.display_timezone))
                    .unwrap_or_else(|| "—".into())
            } else {
                episode
                    .as_ref()
                    .map(|episode| {
                        format_schedule_time(
                            episode.expected_at,
                            anime.schedule_confidence.as_deref(),
                            state.display_timezone,
                        )
                    })
                    .unwrap_or_else(|| "—".into())
            },
            schedule: if anime.lifecycle == "archived" {
                anime
                    .bangumi_subject_id
                    .map(|id| format!("收藏 · #{id}"))
                    .unwrap_or_else(|| "收藏".into())
            } else if anime.auto_schedule {
                anime
                    .bangumi_subject_id
                    .map(|id| format!("自动 · #{id}"))
                    .unwrap_or_else(|| "自动".into())
            } else {
                "手工".into()
            },
            enabled: anime.enabled,
            released_complete: anime.lifecycle == "released_complete",
            archived: anime.lifecycle == "archived",
        });
    }
    let notice = match query.result.as_deref() {
        Some("archived") => "番剧已归档，运行中的集数、候选、通知和检查任务已清理。",
        _ => "",
    }
    .to_string();
    render(AnimeListTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        items,
        current_selected: !archived_selected,
        archived_selected,
        notice,
    })
}

#[derive(Template)]
#[template(path = "anime_new.html")]
struct AnimeNewTemplate {
    username: String,
    csrf_token: String,
}

async fn anime_new(Extension(identity): Extension<SessionIdentity>) -> WebResponse {
    render(AnimeNewTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
    })
}

#[derive(Deserialize)]
struct AnimeResolveForm {
    csrf_token: String,
    title: String,
    next_episode: i64,
    duration_min: i64,
    duration_max: i64,
    timezone: String,
    auto_schedule: Option<String>,
    bangumi_id: Option<String>,
    anilist_id: Option<String>,
    anime_schedule_route: Option<String>,
    search_episode_start: Option<String>,
    bangumi_episode_start: Option<String>,
}

fn parse_optional_bangumi_id(value: Option<&str>) -> Result<Option<i64>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let id = value
        .parse::<i64>()
        .map_err(|_| AppError::InvalidInput("Bangumi ID 必须是正整数".into()))?;
    if id <= 0 {
        return Err(AppError::InvalidInput("Bangumi ID 必须是正整数".into()));
    }
    Ok(Some(id))
}

fn parse_optional_anilist_id(value: Option<&str>) -> Result<Option<i64>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let id = value
        .parse::<i64>()
        .map_err(|_| AppError::InvalidInput("AniList ID 必须是正整数".into()))?;
    if id <= 0 {
        return Err(AppError::InvalidInput("AniList ID 必须是正整数".into()));
    }
    Ok(Some(id))
}

fn parse_episode_mapping(
    search_episode_start: Option<&str>,
    bangumi_episode_start: Option<&str>,
) -> Result<Option<EpisodeNumberMapping>> {
    let parse = |value: Option<&str>, label: &str| -> Result<Option<i64>> {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        let number = value
            .parse::<i64>()
            .map_err(|_| AppError::InvalidInput(format!("{label}必须是正整数")))?;
        if number <= 0 {
            return Err(AppError::InvalidInput(format!("{label}必须是正整数")));
        }
        Ok(Some(number))
    };
    let local_origin = parse(search_episode_start, "站内起始集数")?;
    let bangumi_origin = parse(bangumi_episode_start, "Bangumi 起始集数")?;
    match (local_origin, bangumi_origin) {
        (None, None) => Ok(None),
        (Some(local_origin), Some(bangumi_origin)) => Ok(Some(EpisodeNumberMapping {
            local_origin,
            bangumi_origin,
        })),
        _ => Err(AppError::InvalidInput(
            "集数映射的两个起始集数必须同时填写".into(),
        )),
    }
}

async fn anime_resolve(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    headers: HeaderMap,
    Form(form): Form<AnimeResolveForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let duration_min_sec = form
        .duration_min
        .checked_mul(60)
        .ok_or_else(|| WebError::bad_request("时长超出范围"))?;
    let duration_max_sec = form
        .duration_max
        .checked_mul(60)
        .ok_or_else(|| WebError::bad_request("时长超出范围"))?;
    let auto_schedule = form.auto_schedule.as_deref() == Some("yes");
    let bangumi_id = parse_optional_bangumi_id(form.bangumi_id.as_deref())?;
    let anilist_id = parse_optional_anilist_id(form.anilist_id.as_deref())?;
    let anime_schedule_route = form
        .anime_schedule_route
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let episode_mapping = parse_episode_mapping(
        form.search_episode_start.as_deref(),
        form.bangumi_episode_start.as_deref(),
    )?;
    let id = state
        .application
        .create_anime_draft(
            identity.admin_id,
            &identity.token_hmac,
            AnimeDraftRequest {
                title: form.title,
                next_episode: form.next_episode,
                timezone: form.timezone,
                duration_min_sec,
                duration_max_sec,
                auto_schedule,
                bangumi_id: auto_schedule.then_some(bangumi_id).flatten(),
                anilist_id: auto_schedule.then_some(anilist_id).flatten(),
                anime_schedule_route: auto_schedule.then_some(anime_schedule_route).flatten(),
                episode_mapping,
            },
        )
        .await?;
    audit_success_string(
        &state,
        &identity,
        "anime.draft.create",
        "anime_draft",
        &id,
        serde_json::json!({"auto_schedule": auto_schedule}),
    )
    .await?;
    Ok(Redirect::to(&format!("/anime/drafts/{id}")).into_response())
}

#[derive(Template)]
#[template(path = "anime_draft.html")]
struct AnimeDraftTemplate {
    username: String,
    csrf_token: String,
    draft_id: String,
    queued: bool,
    failed: bool,
    error: String,
    title: String,
    matched_title: String,
    bangumi_id: String,
    anilist_id: String,
    anime_schedule_route: String,
    next_episode: i64,
    total_episodes: String,
    has_total_episodes: bool,
    expected_at: String,
    schedule_source: String,
    schedule_confidence: String,
    aliases: String,
    duration: String,
    episode_mapping: String,
    has_episode_mapping: bool,
    has_warning: bool,
    warning: String,
}

async fn anime_draft(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<String>,
) -> WebResponse {
    let draft = state
        .repository
        .anime_draft(&id, identity.admin_id, &identity.token_hmac)
        .await?;
    let resolved = draft
        .resolved_json
        .as_deref()
        .and_then(|value| serde_json::from_str::<AnimeDraftResolution>(value).ok());
    let (
        title,
        matched_title,
        bangumi_id,
        anilist_id,
        anime_schedule_route,
        next_episode,
        total_episodes,
        expected_at,
        schedule_source,
        schedule_confidence,
        aliases,
        duration,
        episode_mapping,
        warning,
    ) = if let Some(resolved) = resolved {
        let expected_at = format_schedule_time(
            resolved.expected_at,
            resolved.schedule_confidence.as_deref(),
            state.display_timezone,
        );
        let total_episodes = resolved
            .total_episodes
            .map(|total| {
                resolved
                    .episode_mapping
                    .and_then(|mapping| mapping.final_local_episode(total).ok())
                    .map(|final_episode| format!("{total} 集（站内最终 EP{final_episode}）"))
                    .unwrap_or_else(|| format!("{total} 集"))
            })
            .unwrap_or_default();
        let episode_mapping = resolved
            .episode_mapping
            .and_then(|mapping| {
                mapping
                    .mapped_numbers(resolved.next_episode)
                    .ok()
                    .map(|(_, bangumi_episode)| {
                        format!(
                            "站内 EP{} ↔ Bangumi EP{}；当前 EP{} ↔ EP{}",
                            mapping.local_origin,
                            mapping.bangumi_origin,
                            resolved.next_episode,
                            bangumi_episode
                        )
                    })
            })
            .unwrap_or_default();
        (
            resolved.title,
            resolved.matched_title.unwrap_or_else(|| "手工排期".into()),
            resolved
                .bangumi_subject_id
                .map(|id| format!("#{id}"))
                .unwrap_or_else(|| "未绑定".into()),
            resolved
                .anilist_media_id
                .map(|id| format!("#{id}"))
                .unwrap_or_else(|| "未使用".into()),
            resolved
                .anime_schedule_route
                .unwrap_or_else(|| "自动匹配".into()),
            resolved.next_episode,
            total_episodes,
            expected_at,
            resolved
                .schedule_source
                .as_deref()
                .map(schedule_source_label)
                .unwrap_or_else(|| "手工排期".into()),
            resolved
                .schedule_confidence
                .as_deref()
                .map(schedule_confidence_label)
                .unwrap_or_else(|| "—".into()),
            if resolved.aliases.is_empty() {
                "—".into()
            } else {
                resolved.aliases.join("、")
            },
            format!(
                "{}–{} 分钟",
                resolved.duration_min_sec / 60,
                resolved.duration_max_sec / 60
            ),
            episode_mapping,
            resolved.warning.unwrap_or_default(),
        )
    } else {
        (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            0,
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        )
    };
    render(AnimeDraftTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        draft_id: id,
        queued: draft.state == "queued",
        failed: draft.state == "failed",
        error: draft.error.unwrap_or_default(),
        title,
        matched_title,
        bangumi_id,
        anilist_id,
        anime_schedule_route,
        next_episode,
        has_total_episodes: !total_episodes.is_empty(),
        total_episodes,
        expected_at,
        schedule_source,
        schedule_confidence,
        aliases,
        duration,
        has_episode_mapping: !episode_mapping.is_empty(),
        episode_mapping,
        has_warning: !warning.is_empty(),
        warning,
    })
}

#[derive(Deserialize)]
struct AnimeCreateForm {
    csrf_token: String,
    draft_id: String,
    confirm_warning: Option<String>,
}

async fn anime_create(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    headers: HeaderMap,
    Form(form): Form<AnimeCreateForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let anime_id = state
        .application
        .confirm_anime_draft(
            &form.draft_id,
            identity.admin_id,
            &identity.token_hmac,
            form.confirm_warning.as_deref() == Some("yes"),
        )
        .await?;
    audit_success(
        &state,
        &identity,
        "anime.create",
        "anime",
        anime_id,
        serde_json::json!({"draft_id": form.draft_id}),
    )
    .await?;
    Ok(Redirect::to(&format!("/anime/{anime_id}")).into_response())
}

#[derive(Template)]
#[template(path = "anime_detail.html")]
struct AnimeDetailTemplate {
    username: String,
    csrf_token: String,
    id: i64,
    display_no: i64,
    title: String,
    cover_url: String,
    has_cover: bool,
    cover_initial: String,
    aliases: String,
    episode: String,
    expected_at: String,
    schedule_source: String,
    schedule_confidence: String,
    schedule_warning: String,
    has_schedule_warning: bool,
    next_check: String,
    duration: String,
    bangumi: String,
    anilist: String,
    anime_schedule_route: String,
    total_episodes: String,
    has_total_episodes: bool,
    episode_mapping: String,
    has_episode_mapping: bool,
    mapping_search_start: String,
    mapping_bangumi_start: String,
    enabled: bool,
    auto_schedule: bool,
    episode_id: i64,
    history: Vec<EpisodeHistoryView>,
    notice: String,
    video_job_enqueued: bool,
    tracking: bool,
    released_complete: bool,
    lifecycle_label: String,
    suggested_total: i64,
    resume_episode: i64,
}

#[derive(Template)]
#[template(path = "anime_archive_detail.html")]
struct AnimeArchiveDetailTemplate {
    username: String,
    csrf_token: String,
    id: i64,
    title: String,
    cover_url: String,
    has_cover: bool,
    cover_initial: String,
    aliases: String,
    summary: String,
    has_summary: bool,
    total_episodes: String,
    archived_at: String,
    bangumi: String,
}

#[derive(Deserialize, Default)]
struct AnimeDetailQuery {
    result: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct EpisodeVideoView {
    id: i64,
    bvid: String,
    title: String,
    uploader_mid: i64,
    uploader_name: String,
    duration: String,
    score: i64,
    url: String,
    preferred: bool,
    blocked: bool,
    manually_trusted: bool,
    globally_trusted: bool,
    trust_label: String,
    updated_at: String,
}

struct EpisodeHistoryView {
    episode_id: i64,
    episode_no: i64,
    primary: EpisodeVideoView,
    has_primary: bool,
    has_explicit_preferred: bool,
    videos: Vec<EpisodeVideoView>,
}

async fn anime_detail(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    Query(query): Query<AnimeDetailQuery>,
) -> WebResponse {
    let anime = state.repository.get_anime(id).await?;
    let cover_url = anime.anime.bangumi_subject_id.and_then(bangumi_cover_url);
    let cover_initial = title_initial(&anime.anime.title);
    if anime.anime.lifecycle == "archived" {
        return render(AnimeArchiveDetailTemplate {
            username: identity.username,
            csrf_token: identity.csrf_token,
            id,
            title: anime.anime.title,
            has_cover: cover_url.is_some(),
            cover_url: cover_url.unwrap_or_default(),
            cover_initial,
            aliases: anime.aliases.join("、"),
            has_summary: !anime.anime.summary.is_empty(),
            summary: anime.anime.summary,
            total_episodes: anime
                .anime
                .total_episodes
                .map(|value| format!("全 {value} 集"))
                .unwrap_or_else(|| "未知".into()),
            archived_at: format_time(anime.anime.archived_at, state.display_timezone),
            bangumi: anime
                .anime
                .bangumi_subject_id
                .map(|value| format!("#{value}"))
                .unwrap_or_else(|| "未绑定".into()),
        });
    }
    let episode = state.repository.active_episode(id).await.ok();
    let history = episode_history_views(
        state.repository.list_episode_videos(id).await?,
        state.config.confirmation.trusted_confirmed_count,
        state.display_timezone,
    );
    let episode_mapping = anime
        .anime
        .local_episode_origin
        .zip(anime.anime.bangumi_episode_origin)
        .map(|(local_origin, bangumi_origin)| {
            let mapping = EpisodeNumberMapping {
                local_origin,
                bangumi_origin,
            };
            let current = episode
                .as_ref()
                .and_then(|episode| {
                    mapping
                        .mapped_numbers(episode.episode_no)
                        .ok()
                        .map(|(_, bangumi_episode)| {
                            format!("；当前 EP{} ↔ EP{}", episode.episode_no, bangumi_episode)
                        })
                })
                .unwrap_or_default();
            format!("站内 EP{local_origin} ↔ Bangumi EP{bangumi_origin}{current}")
        })
        .unwrap_or_default();
    let total_episodes = anime
        .anime
        .total_episodes
        .map(|total| {
            anime
                .anime
                .final_episode_no()
                .ok()
                .flatten()
                .filter(|final_episode| *final_episode != total)
                .map(|final_episode| format!("{total} 集 · 站内最终 EP{final_episode}"))
                .unwrap_or_else(|| format!("{total} 集"))
        })
        .unwrap_or_default();
    let video_job_enqueued = query.result.as_deref() == Some("video-enqueued");
    let notice = match query.result.as_deref() {
        Some("video-enqueued") => "视频已加入处理队列，元数据读取完成后会显示在本页。",
        Some("video-update-enqueued") => "视频地址已加入更新队列，处理成功后会替换原来源。",
        Some("mapping-updated") => "集数映射已保存，新的 Bangumi 排期正在后台同步。",
        Some("preferred-set") => "已选择这条来源作为本集最佳视频。",
        Some("preferred-cleared") => "已恢复自动选择，将显示未屏蔽来源中评分最高的视频。",
        Some("uploader-trusted") => "已信任此 UP。",
        Some("uploader-globally-trusted") => "已将此 UP 设为全局信任。",
        Some("uploader-blocked") => "已屏蔽此 UP；其视频不会再被自动展示或选为最佳。",
        Some("released-complete") => "已标记为本季播完，并停止查找下一集。等实际看完后再归档即可。",
        Some("tracking-resumed") => "已恢复监控，系统将从指定的下一集继续检查。",
        _ => "",
    }
    .to_string();
    let suggested_total = anime
        .anime
        .total_episodes
        .or_else(|| history.first().map(|item| item.episode_no))
        .or_else(|| {
            episode
                .as_ref()
                .map(|episode| (episode.episode_no - 1).max(1))
        })
        .unwrap_or(1);
    let resume_episode = anime.anime.final_episode_no()?.unwrap_or(suggested_total) + 1;
    render(AnimeDetailTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        id,
        display_no: state.repository.anime_display_number(id).await?,
        title: anime.anime.title,
        has_cover: cover_url.is_some(),
        cover_url: cover_url.unwrap_or_default(),
        cover_initial,
        aliases: anime.aliases.join("、"),
        episode: if anime.anime.lifecycle == "released_complete" {
            anime
                .anime
                .total_episodes
                .map(|total| format!("全 {total} 集 · 已播完，待看完"))
                .unwrap_or_else(|| "已播完 · 待看完".into())
        } else {
            episode
                .as_ref()
                .map(|episode| {
                    format!(
                        "EP{} · {}",
                        episode.episode_no,
                        episode_state_label(&episode.state)
                    )
                })
                .unwrap_or_else(|| "—".into())
        },
        expected_at: if anime.anime.lifecycle == "released_complete" {
            "本季已停止排期".into()
        } else {
            episode
                .as_ref()
                .map(|episode| {
                    format_schedule_time(
                        episode.expected_at,
                        anime.anime.schedule_confidence.as_deref(),
                        state.display_timezone,
                    )
                })
                .unwrap_or_else(|| "未知".into())
        },
        schedule_source: anime
            .anime
            .schedule_source
            .as_deref()
            .map(schedule_source_label)
            .unwrap_or_else(|| {
                if anime.anime.auto_schedule {
                    "未知".into()
                } else {
                    "手工排期".into()
                }
            }),
        schedule_confidence: anime
            .anime
            .schedule_confidence
            .as_deref()
            .map(schedule_confidence_label)
            .unwrap_or_else(|| "—".into()),
        has_schedule_warning: anime.anime.schedule_warning.is_some(),
        schedule_warning: anime.anime.schedule_warning.unwrap_or_default(),
        next_check: if anime.anime.lifecycle == "released_complete" {
            "已停止检查".into()
        } else {
            episode
                .as_ref()
                .map(|episode| format_time(Some(episode.next_check_at), state.display_timezone))
                .unwrap_or_else(|| "—".into())
        },
        duration: format!(
            "{}–{} 分钟",
            anime.anime.duration_min_sec / 60,
            anime.anime.duration_max_sec / 60
        ),
        bangumi: anime
            .anime
            .bangumi_subject_id
            .map(|id| format!("#{id}"))
            .unwrap_or_else(|| "未绑定".into()),
        anilist: anime
            .anime
            .anilist_media_id
            .map(|id| format!("#{id}"))
            .unwrap_or_else(|| "未使用".into()),
        anime_schedule_route: anime
            .anime
            .anime_schedule_route
            .unwrap_or_else(|| "未绑定".into()),
        has_total_episodes: !total_episodes.is_empty(),
        total_episodes,
        has_episode_mapping: !episode_mapping.is_empty(),
        episode_mapping,
        mapping_search_start: anime
            .anime
            .local_episode_origin
            .map(|value| value.to_string())
            .unwrap_or_default(),
        mapping_bangumi_start: anime
            .anime
            .bangumi_episode_origin
            .map(|value| value.to_string())
            .unwrap_or_default(),
        enabled: anime.anime.enabled,
        auto_schedule: anime.anime.auto_schedule,
        episode_id: episode.as_ref().map(|episode| episode.id).unwrap_or(0),
        history,
        notice,
        video_job_enqueued,
        tracking: anime.anime.lifecycle == "tracking",
        released_complete: anime.anime.lifecycle == "released_complete",
        lifecycle_label: if anime.anime.lifecycle == "released_complete" {
            "已播完 · 待看完".into()
        } else if anime.anime.enabled {
            "监控中".into()
        } else {
            "已暂停".into()
        },
        suggested_total,
        resume_episode,
    })
}

fn episode_history_views(
    rows: Vec<EpisodeVideoRow>,
    trusted_confirmed_count: i64,
    timezone: Tz,
) -> Vec<EpisodeHistoryView> {
    let mut groups: Vec<(i64, i64, Vec<EpisodeVideoView>)> = Vec::new();
    for row in rows {
        let trusted = !row.manually_blocked
            && (row.globally_trusted
                || row.manually_trusted
                || (row.confirmed_count >= trusted_confirmed_count && row.rejected_count == 0));
        let trust_label = if row.manually_blocked {
            "已屏蔽"
        } else if row.globally_trusted {
            "全局信任"
        } else if row.manually_trusted {
            "仅此番信任"
        } else if trusted {
            "自动信任"
        } else {
            "未信任"
        };
        let video = EpisodeVideoView {
            id: row.id,
            bvid: row.bvid.clone(),
            title: clean_bilibili_title(&row.title),
            uploader_mid: row.uploader_mid,
            uploader_name: row.uploader_name,
            duration: format_duration(row.duration_sec),
            score: row.score,
            url: canonical_bilibili_url(&row.bvid).unwrap_or_default(),
            preferred: row.is_preferred,
            blocked: row.manually_blocked,
            manually_trusted: row.manually_trusted,
            globally_trusted: row.globally_trusted,
            trust_label: trust_label.into(),
            updated_at: format_time(Some(row.updated_at), timezone),
        };
        if let Some((_, _, videos)) = groups
            .iter_mut()
            .find(|(_, episode_no, _)| *episode_no == row.episode_no)
        {
            videos.push(video);
        } else {
            groups.push((row.episode_id, row.episode_no, vec![video]));
        }
    }
    groups
        .into_iter()
        .map(|(episode_id, episode_no, videos)| {
            let explicit = videos
                .iter()
                .find(|video| video.preferred && !video.blocked)
                .cloned();
            let primary = explicit
                .clone()
                .or_else(|| videos.iter().find(|video| !video.blocked).cloned());
            EpisodeHistoryView {
                episode_id,
                episode_no,
                has_primary: primary.is_some(),
                primary: primary.unwrap_or_default(),
                has_explicit_preferred: explicit.is_some(),
                videos,
            }
        })
        .collect()
}

#[derive(Deserialize)]
struct HistoryVideoAddForm {
    csrf_token: String,
    episode_no: i64,
    url: String,
}

#[derive(Deserialize)]
struct HistoryVideoUpdateForm {
    csrf_token: String,
    url: String,
}

async fn history_video_add(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(anime_id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<HistoryVideoAddForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let bvid = parse_bilibili_bvid(&form.url)?;
    let episode = state
        .repository
        .ensure_historical_episode(anime_id, form.episode_no)
        .await?;
    let payload = serde_json::to_string(&serde_json::json!({
        "purpose": "episode_video",
        "episode_id": episode.id,
        "url": form.url
    }))
    .map_err(|_| WebError::internal())?;
    let job_id = state
        .application
        .enqueue_job(
            ManagementJobKind::AcceptBilibiliUrl,
            Some("anime"),
            Some(&anime_id.to_string()),
            &payload,
            Some(identity.admin_id),
            Some(&format!("episode_video:add:{}:{bvid}", episode.id)),
        )
        .await?;
    audit_success(
        &state,
        &identity,
        "episode_video.add.enqueue",
        "management_job",
        job_id,
        serde_json::json!({"anime_id": anime_id, "episode_id": episode.id, "bvid": bvid}),
    )
    .await?;
    Ok(Redirect::to(&format!("/anime/{anime_id}?result=video-enqueued")).into_response())
}

async fn history_video_update(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path((anime_id, video_id)): Path<(i64, i64)>,
    headers: HeaderMap,
    Form(form): Form<HistoryVideoUpdateForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let bvid = parse_bilibili_bvid(&form.url)?;
    let video = state
        .repository
        .list_episode_videos(anime_id)
        .await?
        .into_iter()
        .find(|video| video.id == video_id)
        .ok_or_else(|| WebError::not_found("未找到这个历史视频"))?;
    let payload = serde_json::to_string(&serde_json::json!({
        "purpose": "episode_video",
        "episode_id": video.episode_id,
        "video_id": video_id,
        "url": form.url
    }))
    .map_err(|_| WebError::internal())?;
    let job_id = state
        .application
        .enqueue_job(
            ManagementJobKind::AcceptBilibiliUrl,
            Some("anime"),
            Some(&anime_id.to_string()),
            &payload,
            Some(identity.admin_id),
            Some(&format!("episode_video:update:{video_id}:{bvid}")),
        )
        .await?;
    audit_success(
        &state,
        &identity,
        "episode_video.update.enqueue",
        "management_job",
        job_id,
        serde_json::json!({"anime_id": anime_id, "video_id": video_id, "bvid": bvid}),
    )
    .await?;
    Ok(Redirect::to(&format!("/anime/{anime_id}?result=video-update-enqueued")).into_response())
}

async fn history_video_prefer(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path((anime_id, episode_id, video_id)): Path<(i64, i64, i64)>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state
        .repository
        .set_episode_video_preferred(anime_id, episode_id, Some(video_id))
        .await?;
    audit_success(
        &state,
        &identity,
        "episode_video.prefer",
        "episode_video",
        video_id,
        serde_json::json!({"anime_id": anime_id, "episode_id": episode_id}),
    )
    .await?;
    Ok(Redirect::to(&format!("/anime/{anime_id}?result=preferred-set")).into_response())
}

async fn history_video_preferred_clear(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path((anime_id, episode_id)): Path<(i64, i64)>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state
        .repository
        .set_episode_video_preferred(anime_id, episode_id, None)
        .await?;
    audit_success(
        &state,
        &identity,
        "episode_video.preferred.clear",
        "episode",
        episode_id,
        serde_json::json!({"anime_id": anime_id}),
    )
    .await?;
    Ok(Redirect::to(&format!("/anime/{anime_id}?result=preferred-cleared")).into_response())
}

#[derive(Deserialize)]
struct CsrfForm {
    csrf_token: String,
}

#[derive(Deserialize)]
struct AnimeTitleForm {
    csrf_token: String,
    title: String,
}

async fn anime_title(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<AnimeTitleForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let previous = state.application.rename_anime(id, &form.title).await?;
    audit_success(
        &state,
        &identity,
        "anime.rename",
        "anime",
        id,
        serde_json::json!({"previous_title": previous.title}),
    )
    .await?;
    Ok(Redirect::to(&format!("/anime/{id}")).into_response())
}

async fn anime_enable(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state.application.set_anime_enabled(id, true).await?;
    audit_success(
        &state,
        &identity,
        "anime.enable",
        "anime",
        id,
        serde_json::json!({}),
    )
    .await?;
    Ok(Redirect::to(&format!("/anime/{id}")).into_response())
}

async fn anime_disable(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state.application.set_anime_enabled(id, false).await?;
    audit_success(
        &state,
        &identity,
        "anime.disable",
        "anime",
        id,
        serde_json::json!({}),
    )
    .await?;
    Ok(Redirect::to(&format!("/anime/{id}")).into_response())
}

async fn anime_check(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let target = id.to_string();
    let job_id = state
        .application
        .enqueue_job(
            ManagementJobKind::CheckAnime,
            Some("anime"),
            Some(&target),
            "{}",
            Some(identity.admin_id),
            Some(&format!("check_anime:{id}")),
        )
        .await?;
    audit_success(
        &state,
        &identity,
        "anime.check.enqueue",
        "management_job",
        job_id,
        serde_json::json!({"anime_id": id}),
    )
    .await?;
    Ok(Redirect::to(&format!("/anime/{id}")).into_response())
}

async fn anime_sync(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let target = id.to_string();
    let job_id = state
        .application
        .enqueue_job(
            ManagementJobKind::SyncSchedule,
            Some("anime"),
            Some(&target),
            "{}",
            Some(identity.admin_id),
            Some(&format!("sync_schedule:{id}")),
        )
        .await?;
    audit_success(
        &state,
        &identity,
        "anime.sync.enqueue",
        "management_job",
        job_id,
        serde_json::json!({"anime_id": id}),
    )
    .await?;
    Ok(Redirect::to(&format!("/anime/{id}")).into_response())
}

#[derive(Deserialize)]
struct AnimeEpisodeMappingForm {
    csrf_token: String,
    search_episode_start: Option<String>,
    bangumi_episode_start: Option<String>,
}

async fn anime_episode_mapping(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<AnimeEpisodeMappingForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let mapping = parse_episode_mapping(
        form.search_episode_start.as_deref(),
        form.bangumi_episode_start.as_deref(),
    )?;
    state
        .application
        .set_episode_number_mapping(id, mapping)
        .await?;
    let target = id.to_string();
    let job_id = state
        .application
        .enqueue_job(
            ManagementJobKind::SyncSchedule,
            Some("anime"),
            Some(&target),
            "{}",
            Some(identity.admin_id),
            Some(&format!("sync_schedule:{id}")),
        )
        .await?;
    audit_success(
        &state,
        &identity,
        "anime.episode_mapping.update",
        "anime",
        id,
        serde_json::json!({
            "job_id": job_id,
            "local_origin": mapping.map(|value| value.local_origin),
            "bangumi_origin": mapping.map(|value| value.bangumi_origin),
        }),
    )
    .await?;
    Ok(Redirect::to(&format!("/anime/{id}?result=mapping-updated")).into_response())
}

#[derive(Deserialize)]
struct AnimeReleasedCompleteForm {
    csrf_token: String,
    total_episodes: i64,
}

async fn anime_released_complete(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<AnimeReleasedCompleteForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let total = state
        .application
        .mark_anime_released_complete(id, Some(form.total_episodes))
        .await?;
    audit_success(
        &state,
        &identity,
        "anime.released_complete",
        "anime",
        id,
        serde_json::json!({"total_episodes": total}),
    )
    .await?;
    Ok(Redirect::to(&format!("/anime/{id}?result=released-complete")).into_response())
}

#[derive(Deserialize)]
struct AnimeResumeForm {
    csrf_token: String,
    next_episode: i64,
}

async fn anime_resume_tracking(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<AnimeResumeForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state
        .application
        .resume_anime_tracking(id, form.next_episode)
        .await?;
    audit_success(
        &state,
        &identity,
        "anime.tracking.resume",
        "anime",
        id,
        serde_json::json!({"next_episode": form.next_episode}),
    )
    .await?;
    Ok(Redirect::to(&format!("/anime/{id}?result=tracking-resumed")).into_response())
}

#[derive(Template)]
#[template(path = "anime_archive.html")]
struct AnimeArchiveTemplate {
    username: String,
    csrf_token: String,
    id: i64,
    title: String,
    nonce: String,
    suggested_total: i64,
    existing_summary: String,
}

async fn anime_archive_intent(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let anime = state.repository.get_anime(id).await?;
    if anime.anime.lifecycle != "released_complete" {
        return Err(WebError::bad_request("请先标记本季已播完，再归档收藏"));
    }
    let entity = id.to_string();
    let nonce = state
        .auth
        .issue_action_nonce(&identity, "anime.archive", Some(&entity))
        .await?;
    render(AnimeArchiveTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        id,
        title: anime.anime.title,
        nonce,
        suggested_total: anime.anime.total_episodes.unwrap_or(1),
        existing_summary: anime.anime.summary,
    })
}

#[derive(Deserialize)]
struct AnimeArchiveForm {
    csrf_token: String,
    nonce: String,
    total_episodes: i64,
    summary: String,
    confirm_watched: Option<String>,
}

async fn anime_archive(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<AnimeArchiveForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    if form.confirm_watched.as_deref() != Some("yes") {
        return Err(WebError::bad_request("请确认已经实际看完本季"));
    }
    let entity = id.to_string();
    if !state
        .auth
        .consume_action_nonce(&identity, &form.nonce, "anime.archive", Some(&entity))
        .await?
    {
        return Err(WebError::forbidden("归档确认已使用或过期，请重新开始"));
    }
    let archived = state
        .application
        .archive_anime(id, Some(form.total_episodes), &form.summary)
        .await?;
    audit_success(
        &state,
        &identity,
        "anime.archive",
        "anime",
        id,
        serde_json::json!({
            "total_episodes": archived.total_episodes,
            "removed_episodes": archived.removed_episodes,
            "removed_candidates": archived.removed_candidates,
            "removed_notifications": archived.removed_notifications,
            "removed_jobs": archived.removed_jobs
        }),
    )
    .await?;
    Ok(Redirect::to("/anime?view=archived&result=archived").into_response())
}

async fn anime_delete_intent(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let anime = state.repository.get_anime(id).await?;
    if anime.anime.enabled {
        return Err(WebError::bad_request("永久删除前必须先暂停监控"));
    }
    let entity = id.to_string();
    let nonce = state
        .auth
        .issue_action_nonce(&identity, "anime.delete", Some(&entity))
        .await?;
    render(AnimeDeleteTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        id,
        title: anime.anime.title,
        bangumi: anime
            .anime
            .bangumi_subject_id
            .map(|value| format!("#{value}"))
            .unwrap_or_else(|| "未绑定".into()),
        nonce,
        updated_at: anime.anime.updated_at.to_rfc3339(),
    })
}

#[derive(Template)]
#[template(path = "anime_delete.html")]
struct AnimeDeleteTemplate {
    username: String,
    csrf_token: String,
    id: i64,
    title: String,
    bangumi: String,
    nonce: String,
    updated_at: String,
}

#[derive(Deserialize)]
struct AnimeDeleteForm {
    csrf_token: String,
    nonce: String,
    title: String,
    updated_at: String,
}

async fn anime_delete(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<AnimeDeleteForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let entity = id.to_string();
    if !state
        .auth
        .consume_action_nonce(&identity, &form.nonce, "anime.delete", Some(&entity))
        .await?
    {
        return Err(WebError::forbidden("删除确认已使用或过期，请重新开始"));
    }
    let updated_at = DateTime::parse_from_rfc3339(&form.updated_at)
        .map_err(|_| WebError::bad_request("删除版本信息无效"))?
        .with_timezone(&Utc);
    let deleted = state
        .application
        .delete_anime_checked(id, &form.title, updated_at)
        .await?;
    audit_success(
        &state,
        &identity,
        "anime.delete",
        "anime",
        id,
        serde_json::json!({"title": deleted.title}),
    )
    .await?;
    let cover_cache = state.cover_cache.clone();
    let repository = state.repository.clone();
    tokio::spawn(async move {
        if let Err(error) = cover_cache.prune(&repository).await {
            tracing::warn!(%error, "cover cache cleanup after anime deletion failed");
        }
    });
    Ok(Redirect::to("/anime").into_response())
}

#[derive(Deserialize, Default)]
struct CandidateQuery {
    state: Option<String>,
    result: Option<String>,
    changed: Option<u64>,
}

struct CandidateView {
    episode_id: i64,
    bvid: String,
    anime_id: i64,
    uploader_mid: i64,
    anime_title: String,
    episode_no: i64,
    title: String,
    uploader: String,
    duration: String,
    score: i64,
    state: String,
    evaluation: String,
    reputation: String,
    url: String,
    has_video_url: bool,
    pending: bool,
    manually_trusted: bool,
    globally_trusted: bool,
}

#[derive(Template)]
#[template(path = "candidates.html")]
struct CandidateTemplate {
    username: String,
    csrf_token: String,
    items: Vec<CandidateView>,
    pending_selected: bool,
    all_selected: bool,
    notice: String,
}

async fn candidate_list(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Query(query): Query<CandidateQuery>,
) -> WebResponse {
    let selected = query.state.as_deref().unwrap_or("pending");
    let filter = match selected {
        "all" => None,
        "pending" | "confirmed" | "rejected" | "expired" => Some(selected),
        _ => return Err(WebError::bad_request("无效的候选筛选条件")),
    };
    let items = state
        .repository
        .list_candidates(filter)
        .await?
        .into_iter()
        .map(candidate_view)
        .collect();
    let notice = match query.result.as_deref() {
        Some("uploader-blocked") => format!(
            "已屏蔽此 UP，并拒绝其当前 {} 个待审核候选。以后发现该 UP 的视频也会自动排除。",
            query.changed.unwrap_or(0)
        ),
        Some("uploader-trusted") => {
            "已信任此 UP；以后符合番剧、集数和时长条件的视频可以自动确认。".into()
        }
        Some("uploader-globally-trusted") => {
            "已全局信任此 UP；其为其他番剧上传的合格视频也可以自动确认。".into()
        }
        Some("candidate-rejected") => "已拒绝该候选。".into(),
        _ => String::new(),
    };
    render(CandidateTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        items,
        pending_selected: selected == "pending",
        all_selected: selected == "all",
        notice,
    })
}

fn candidate_view(row: CandidateListRow) -> CandidateView {
    let reputation = candidate_reputation(&row.evaluation_json);
    let evaluation = serde_json::from_str::<serde_json::Value>(&row.evaluation_json)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or_else(|_| "判定详情不可用".into());
    let url = canonical_bilibili_url(&row.bvid);
    CandidateView {
        episode_id: row.episode_id,
        bvid: row.bvid,
        anime_id: row.anime_id,
        uploader_mid: row.uploader_mid,
        anime_title: row.anime_title,
        episode_no: row.episode_no,
        title: clean_bilibili_title(&row.title),
        uploader: row.uploader_name,
        duration: format_duration(row.duration_sec),
        score: row.score,
        pending: row.state == "pending",
        state: candidate_state_label(&row.state).into(),
        evaluation,
        reputation,
        has_video_url: url.is_some(),
        url: url.unwrap_or_default(),
        manually_trusted: row.manually_trusted,
        globally_trusted: row.globally_trusted,
    }
}

#[derive(Deserialize, Default)]
struct RuleQuery {
    result: Option<String>,
}

struct RuleAnimeOption {
    id: i64,
    title: String,
}

struct TrustedUploaderView {
    anime_id: i64,
    uploader_mid: i64,
    uploader_name: String,
}

struct GlobalTrustedUploaderView {
    uploader_mid: i64,
    uploader_name: String,
}

#[derive(Template)]
#[template(path = "rules.html")]
struct RuleTemplate {
    username: String,
    csrf_token: String,
    keywords: Vec<crate::repository::BlockedKeywordRow>,
    global_uploaders: Vec<GlobalTrustedUploaderView>,
    uploaders: Vec<TrustedUploaderView>,
    trust_count: usize,
    anime: Vec<RuleAnimeOption>,
    notice: String,
}

async fn rule_list(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Query(query): Query<RuleQuery>,
) -> WebResponse {
    let keywords = state.repository.list_blocked_keywords().await?;
    let global_uploaders = state
        .repository
        .list_globally_trusted_uploaders()
        .await?
        .into_iter()
        .map(|row| GlobalTrustedUploaderView {
            uploader_mid: row.uploader_mid,
            uploader_name: row
                .uploader_name
                .unwrap_or_else(|| format!("UID {}", row.uploader_mid)),
        })
        .collect::<Vec<_>>();
    let uploaders = state
        .repository
        .list_manually_trusted_uploaders()
        .await?
        .into_iter()
        .map(|row| TrustedUploaderView {
            anime_id: row.anime_id,
            uploader_mid: row.uploader_mid,
            uploader_name: row
                .uploader_name
                .unwrap_or_else(|| format!("UID {}", row.uploader_mid)),
        })
        .collect::<Vec<_>>();
    let trust_count = global_uploaders.len() + uploaders.len();
    let anime = state
        .repository
        .list_anime()
        .await?
        .into_iter()
        .map(|item| RuleAnimeOption {
            id: item.id,
            title: item.title,
        })
        .collect();
    let notice = match query.result.as_deref() {
        Some("keyword-added") => "屏蔽词已添加，之后的候选会立即应用这条规则。",
        Some("keyword-updated") => "屏蔽词已更新。",
        Some("keyword-deleted") => "屏蔽词已删除。",
        Some("uploader-added") => "信任 UP 已添加。",
        Some("uploader-updated") => "信任 UP 已更新。",
        Some("uploader-deleted") => "信任 UP 已移除。",
        Some("global-uploader-added") => "全局信任 UP 已添加。",
        Some("global-uploader-updated") => "全局信任 UP 已更新。",
        Some("global-uploader-deleted") => "全局信任 UP 已移除。",
        Some("uploader-promoted") => "已升级为全局信任，重复的按番信任已自动清理。",
        _ => "",
    }
    .to_string();
    render(RuleTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        keywords,
        global_uploaders,
        uploaders,
        trust_count,
        anime,
        notice,
    })
}

#[derive(Deserialize)]
struct KeywordForm {
    csrf_token: String,
    keyword: String,
}

async fn keyword_create(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    headers: HeaderMap,
    Form(form): Form<KeywordForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let id = state.application.add_blocked_keyword(&form.keyword).await?;
    audit_success(
        &state,
        &identity,
        "rule.keyword.create",
        "blocked_keyword",
        id,
        serde_json::json!({"keyword": form.keyword.trim()}),
    )
    .await?;
    Ok(Redirect::to("/rules?result=keyword-added").into_response())
}

async fn keyword_update(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<KeywordForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state
        .application
        .update_blocked_keyword(id, &form.keyword)
        .await?;
    audit_success(
        &state,
        &identity,
        "rule.keyword.update",
        "blocked_keyword",
        id,
        serde_json::json!({"keyword": form.keyword.trim()}),
    )
    .await?;
    Ok(Redirect::to("/rules?result=keyword-updated").into_response())
}

async fn keyword_delete(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state.application.delete_blocked_keyword(id).await?;
    audit_success(
        &state,
        &identity,
        "rule.keyword.delete",
        "blocked_keyword",
        id,
        serde_json::json!({}),
    )
    .await?;
    Ok(Redirect::to("/rules?result=keyword-deleted").into_response())
}

#[derive(Deserialize)]
struct TrustedUploaderForm {
    csrf_token: String,
    anime_id: i64,
    mid: i64,
    name: String,
}

#[derive(Deserialize)]
struct GlobalTrustedUploaderForm {
    csrf_token: String,
    mid: i64,
    name: String,
}

async fn global_trusted_uploader_create(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    headers: HeaderMap,
    Form(form): Form<GlobalTrustedUploaderForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state
        .application
        .add_global_trusted_uploader(form.mid, &form.name)
        .await?;
    audit_success_string(
        &state,
        &identity,
        "rule.uploader.global.create",
        "global_trusted_uploader",
        &form.mid.to_string(),
        serde_json::json!({"name": form.name.trim()}),
    )
    .await?;
    Ok(Redirect::to("/rules?result=global-uploader-added").into_response())
}

async fn global_trusted_uploader_update(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(old_mid): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<GlobalTrustedUploaderForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state
        .application
        .update_global_trusted_uploader(old_mid, form.mid, &form.name)
        .await?;
    audit_success_string(
        &state,
        &identity,
        "rule.uploader.global.update",
        "global_trusted_uploader",
        &old_mid.to_string(),
        serde_json::json!({"mid": form.mid, "name": form.name.trim()}),
    )
    .await?;
    Ok(Redirect::to("/rules?result=global-uploader-updated").into_response())
}

async fn global_trusted_uploader_delete(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(mid): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state
        .application
        .remove_global_trusted_uploader(mid)
        .await?;
    audit_success_string(
        &state,
        &identity,
        "rule.uploader.global.delete",
        "global_trusted_uploader",
        &mid.to_string(),
        serde_json::json!({}),
    )
    .await?;
    Ok(Redirect::to("/rules?result=global-uploader-deleted").into_response())
}

async fn trusted_uploader_create(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    headers: HeaderMap,
    Form(form): Form<TrustedUploaderForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state
        .application
        .add_trusted_uploader(form.anime_id, form.mid, &form.name)
        .await?;
    let entity = format!("{}:{}", form.anime_id, form.mid);
    audit_success_string(
        &state,
        &identity,
        "rule.uploader.create",
        "trusted_uploader",
        &entity,
        serde_json::json!({"name": form.name.trim()}),
    )
    .await?;
    Ok(Redirect::to("/rules?result=uploader-added").into_response())
}

async fn trusted_uploader_update(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path((old_anime_id, old_mid)): Path<(i64, i64)>,
    headers: HeaderMap,
    Form(form): Form<TrustedUploaderForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state
        .application
        .update_trusted_uploader(old_anime_id, old_mid, form.anime_id, form.mid, &form.name)
        .await?;
    let old_entity = format!("{old_anime_id}:{old_mid}");
    audit_success_string(
        &state,
        &identity,
        "rule.uploader.update",
        "trusted_uploader",
        &old_entity,
        serde_json::json!({"anime_id": form.anime_id, "mid": form.mid, "name": form.name.trim()}),
    )
    .await?;
    Ok(Redirect::to("/rules?result=uploader-updated").into_response())
}

async fn trusted_uploader_delete(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path((anime_id, mid)): Path<(i64, i64)>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state
        .application
        .remove_trusted_uploader(anime_id, mid)
        .await?;
    let entity = format!("{anime_id}:{mid}");
    audit_success_string(
        &state,
        &identity,
        "rule.uploader.delete",
        "trusted_uploader",
        &entity,
        serde_json::json!({}),
    )
    .await?;
    Ok(Redirect::to("/rules?result=uploader-deleted").into_response())
}

async fn trusted_uploader_promote(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path((anime_id, mid)): Path<(i64, i64)>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state
        .application
        .promote_uploader_from_anime(anime_id, mid)
        .await?;
    audit_success_string(
        &state,
        &identity,
        "rule.uploader.promote",
        "global_trusted_uploader",
        &mid.to_string(),
        serde_json::json!({"source_anime_id": anime_id}),
    )
    .await?;
    Ok(Redirect::to("/rules?result=uploader-promoted").into_response())
}

async fn candidate_accept_intent(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path((episode_id, bvid)): Path<(i64, String)>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let (candidate, episode) = state
        .repository
        .candidate_context_for_episode(episode_id, &bvid)
        .await?;
    if candidate.state != "pending" {
        return Err(WebError::bad_request("该候选已经处理，不能再次确认"));
    }
    let active = state.repository.active_episode(episode.anime_id).await?;
    if active.id != episode.id {
        return Err(WebError::bad_request(
            "该候选不属于当前待更新集，已拒绝操作",
        ));
    }
    let entity = format!("{episode_id}:{bvid}");
    let nonce = state
        .auth
        .issue_action_nonce(&identity, "candidate.accept", Some(&entity))
        .await?;
    let anime = state.repository.get_anime(episode.anime_id).await?;
    render(CandidateAcceptTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        nonce,
        episode_id,
        bvid,
        anime_title: anime.anime.title,
        episode_no: episode.episode_no,
        title: candidate.title,
        uploader: candidate.uploader_name,
    })
}

#[derive(Template)]
#[template(path = "candidate_accept.html")]
struct CandidateAcceptTemplate {
    username: String,
    csrf_token: String,
    nonce: String,
    episode_id: i64,
    bvid: String,
    anime_title: String,
    episode_no: i64,
    title: String,
    uploader: String,
}

#[derive(Deserialize)]
struct CandidateAcceptForm {
    csrf_token: String,
    nonce: String,
}

async fn candidate_accept(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path((episode_id, bvid)): Path<(i64, String)>,
    headers: HeaderMap,
    Form(form): Form<CandidateAcceptForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let entity = format!("{episode_id}:{bvid}");
    if !state
        .auth
        .consume_action_nonce(&identity, &form.nonce, "candidate.accept", Some(&entity))
        .await?
    {
        return Err(WebError::forbidden("候选确认已使用或过期，请重新开始"));
    }
    state
        .application
        .accept_candidate_for_episode(episode_id, &bvid)
        .await?;
    audit_success_string(
        &state,
        &identity,
        "candidate.accept",
        "candidate",
        &entity,
        serde_json::json!({"episode_id": episode_id, "bvid": bvid}),
    )
    .await?;
    Ok(Redirect::to("/candidates?state=pending").into_response())
}

#[derive(Deserialize)]
struct UploaderActionForm {
    csrf_token: String,
    return_to: Option<String>,
}

fn uploader_action_redirect(
    anime_id: i64,
    return_to: Option<&str>,
    result: &str,
    changed: Option<u64>,
) -> String {
    let detail_path = format!("/anime/{anime_id}");
    if return_to == Some(detail_path.as_str()) {
        format!("{detail_path}?result={result}")
    } else if let Some(changed) = changed {
        format!("/candidates?state=pending&result={result}&changed={changed}")
    } else {
        format!("/candidates?state=pending&result={result}")
    }
}

async fn uploader_trust(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path((anime_id, mid)): Path<(i64, i64)>,
    headers: HeaderMap,
    Form(form): Form<UploaderActionForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state
        .application
        .set_uploader_flag(anime_id, mid, true, false)
        .await?;
    audit_success_string(
        &state,
        &identity,
        "uploader.trust",
        "uploader",
        &format!("{anime_id}:{mid}"),
        serde_json::json!({}),
    )
    .await?;
    Ok(Redirect::to(&uploader_action_redirect(
        anime_id,
        form.return_to.as_deref(),
        "uploader-trusted",
        None,
    ))
    .into_response())
}

async fn uploader_trust_global(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path((anime_id, mid)): Path<(i64, i64)>,
    headers: HeaderMap,
    Form(form): Form<UploaderActionForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state
        .application
        .promote_uploader_from_anime(anime_id, mid)
        .await?;
    audit_success_string(
        &state,
        &identity,
        "uploader.trust_global",
        "global_trusted_uploader",
        &mid.to_string(),
        serde_json::json!({"source_anime_id": anime_id}),
    )
    .await?;
    Ok(Redirect::to(&uploader_action_redirect(
        anime_id,
        form.return_to.as_deref(),
        "uploader-globally-trusted",
        None,
    ))
    .into_response())
}

async fn uploader_block(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path((anime_id, mid)): Path<(i64, i64)>,
    headers: HeaderMap,
    Form(form): Form<UploaderActionForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let rejected = state.application.block_uploader(anime_id, mid).await?;
    audit_success_string(
        &state,
        &identity,
        "uploader.block",
        "uploader",
        &format!("{anime_id}:{mid}"),
        serde_json::json!({"rejected_candidates": rejected}),
    )
    .await?;
    Ok(Redirect::to(&uploader_action_redirect(
        anime_id,
        form.return_to.as_deref(),
        "uploader-blocked",
        Some(rejected),
    ))
    .into_response())
}

async fn candidate_reject(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path((episode_id, bvid)): Path<(i64, String)>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state
        .application
        .reject_candidate_for_episode(episode_id, &bvid)
        .await?;
    let entity = format!("{episode_id}:{bvid}");
    audit_success_string(
        &state,
        &identity,
        "candidate.reject",
        "candidate",
        &entity,
        serde_json::json!({"episode_id": episode_id, "bvid": bvid}),
    )
    .await?;
    Ok(Redirect::to("/candidates?state=pending&result=candidate-rejected").into_response())
}

struct ReviewCandidate {
    bvid: String,
    title: String,
    uploader: String,
    duration: String,
    score: i64,
    reputation: String,
    url: String,
    has_video_url: bool,
}

#[derive(Template)]
#[template(path = "review_episode.html")]
struct ReviewEpisodeTemplate {
    username: String,
    csrf_token: String,
    episode_id: i64,
    anime_id: i64,
    anime_title: String,
    episode_no: i64,
    candidates: Vec<ReviewCandidate>,
}

async fn review_episode(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
) -> WebResponse {
    let episode = state.repository.episode(id).await?;
    let anime = state.repository.get_anime(episode.anime_id).await?;
    let candidates = state
        .repository
        .active_candidates(id)
        .await?
        .into_iter()
        .map(|candidate| {
            let url = canonical_bilibili_url(&candidate.bvid);
            let reputation = candidate_reputation(&candidate.evaluation_json);
            ReviewCandidate {
                bvid: candidate.bvid,
                title: clean_bilibili_title(&candidate.title),
                uploader: candidate.uploader_name,
                duration: format_duration(candidate.duration_sec),
                score: candidate.score,
                reputation,
                has_video_url: url.is_some(),
                url: url.unwrap_or_default(),
            }
        })
        .collect();
    render(ReviewEpisodeTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        episode_id: id,
        anime_id: episode.anime_id,
        anime_title: anime.anime.title,
        episode_no: episode.episode_no,
        candidates,
    })
}

fn candidate_reputation(evaluation_json: &str) -> String {
    let Ok(evaluation) = serde_json::from_str::<Evaluation>(evaluation_json) else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let Some(followers) = evaluation.uploader_follower_count {
        parts.push(format!("UP 粉丝 {followers}"));
    }
    if let Some(views) = evaluation.view_count {
        parts.push(format!("播放 {views}"));
    }
    if let Some(replies) = evaluation.reply_count {
        parts.push(format!("评论 {replies}"));
    }
    parts.join(" · ")
}

async fn reject_all_intent(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state.repository.episode(id).await?;
    let fingerprint = state.repository.pending_candidate_fingerprint(id).await?;
    if state.repository.active_candidates(id).await?.is_empty() {
        return Err(WebError::bad_request("当前集没有待拒绝候选"));
    }
    let entity = format!("{id}:{fingerprint}");
    let nonce = state
        .auth
        .issue_action_nonce(&identity, "candidate.reject_all", Some(&entity))
        .await?;
    let candidates = state.repository.active_candidates(id).await?;
    render(RejectAllTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        episode_id: id,
        candidate_count: candidates.len(),
        nonce,
        fingerprint,
    })
}

#[derive(Template)]
#[template(path = "reject_all.html")]
struct RejectAllTemplate {
    username: String,
    csrf_token: String,
    episode_id: i64,
    candidate_count: usize,
    nonce: String,
    fingerprint: String,
}

#[derive(Deserialize)]
struct RejectAllForm {
    csrf_token: String,
    nonce: String,
    fingerprint: String,
}

async fn reject_all(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<RejectAllForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let entity = format!("{}:{}", id, form.fingerprint);
    if !state
        .auth
        .consume_action_nonce(
            &identity,
            &form.nonce,
            "candidate.reject_all",
            Some(&entity),
        )
        .await?
    {
        return Err(WebError::forbidden("全部拒绝确认已使用或过期，请重新开始"));
    }
    let count = state
        .repository
        .reject_all_candidates_checked(id, &form.fingerprint)
        .await?;
    audit_success(
        &state,
        &identity,
        "candidate.reject_all",
        "episode",
        id,
        serde_json::json!({"count": count}),
    )
    .await?;
    Ok(Redirect::to(&format!("/review/episodes/{id}")).into_response())
}

#[derive(Deserialize)]
struct CandidateUrlForm {
    csrf_token: String,
    url: String,
}

async fn candidate_from_url(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CandidateUrlForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let episode = state.repository.episode(id).await?;
    let bvid = parse_bilibili_bvid(&form.url)?;
    let target = episode.anime_id.to_string();
    let payload = serde_json::to_string(&serde_json::json!({
        "url": form.url,
        "episode_id": id
    }))
    .map_err(|_| WebError::internal())?;
    let job_id = state
        .application
        .enqueue_job(
            ManagementJobKind::AcceptBilibiliUrl,
            Some("anime"),
            Some(&target),
            &payload,
            Some(identity.admin_id),
            Some(&format!("accept_bilibili_url:{id}:{bvid}")),
        )
        .await?;
    audit_success(
        &state,
        &identity,
        "candidate.url.enqueue",
        "management_job",
        job_id,
        serde_json::json!({"episode_id": id, "bvid": bvid}),
    )
    .await?;
    Ok(Redirect::to("/jobs").into_response())
}

struct JobView {
    id: i64,
    kind: String,
    target: String,
    target_url: String,
    has_target_url: bool,
    state: String,
    created_at: String,
    error: String,
}
#[derive(Template)]
#[template(path = "jobs.html")]
struct JobTemplate {
    username: String,
    csrf_token: String,
    items: Vec<JobView>,
}

async fn job_list(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
) -> WebResponse {
    let items = state
        .repository
        .list_management_jobs_with_targets(100)
        .await?
        .into_iter()
        .map(|job| job_view(job, state.display_timezone))
        .collect();
    render(JobTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        items,
    })
}

fn job_view(job: ManagementJobListRow, timezone: Tz) -> JobView {
    let anime_id = if job.target_type.as_deref() == Some("anime") {
        job.target_id
            .as_deref()
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|value| *value > 0)
    } else {
        None
    };
    let target = match (job.anime_title.as_deref(), anime_id) {
        (Some(title), Some(id)) => format!("{title} · #{id}"),
        (None, Some(id)) => format!("番剧 #{id}"),
        _ => job.target_id.clone().unwrap_or_else(|| "—".into()),
    };
    let target_url = anime_id
        .map(|id| format!("/anime/{id}"))
        .unwrap_or_default();
    JobView {
        id: job.id,
        kind: job.kind,
        target,
        has_target_url: !target_url.is_empty(),
        target_url,
        state: job.state,
        created_at: format_time(Some(job.created_at), timezone),
        error: job.error.unwrap_or_default(),
    }
}

struct AuditView {
    created_at: String,
    actor: String,
    action: String,
    entity: String,
    outcome: String,
}
#[derive(Template)]
#[template(path = "audit.html")]
struct AuditTemplate {
    username: String,
    csrf_token: String,
    items: Vec<AuditView>,
}

async fn audit_list(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
) -> WebResponse {
    let items = state
        .repository
        .list_audit_events(100)
        .await?
        .into_iter()
        .map(|event| audit_view(event, state.display_timezone))
        .collect();
    render(AuditTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        items,
    })
}

fn audit_view(event: AuditEventRow, timezone: Tz) -> AuditView {
    AuditView {
        created_at: format_time(Some(event.created_at), timezone),
        actor: event
            .actor_admin_id
            .map(|id| format!("{} #{id}", event.actor_type))
            .unwrap_or(event.actor_type),
        action: event.action,
        entity: format!(
            "{} {}",
            event.entity_type.unwrap_or_default(),
            event.entity_id.unwrap_or_default()
        ),
        outcome: event.outcome,
    }
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    username: String,
    csrf_token: String,
    scheduler: String,
    provider: String,
    notification_provider: String,
    schedule_source: String,
    public_url: String,
    timezone: String,
}

async fn settings_status(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
) -> WebResponse {
    let stats = state.repository.dashboard_stats().await?;
    render(SettingsTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        scheduler: heartbeat_status(stats.scheduler_heartbeat, state.display_timezone),
        provider: provider_status(stats.provider_backoff_until, state.display_timezone),
        notification_provider: state.config.notification.provider.clone(),
        schedule_source: state.config.schedule.bangumi_data_url.clone(),
        public_url: state.config.web.public_url.clone(),
        timezone: state.config.web.timezone.clone(),
    })
}

async fn notification_test(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    let job_id = state
        .application
        .enqueue_job(
            ManagementJobKind::NotificationTest,
            None,
            None,
            "{}",
            Some(identity.admin_id),
            Some("notification_test"),
        )
        .await?;
    audit_success(
        &state,
        &identity,
        "notification.test.enqueue",
        "management_job",
        job_id,
        serde_json::json!({}),
    )
    .await?;
    Ok(Redirect::to("/jobs").into_response())
}

async fn logout(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    audit_success(
        &state,
        &identity,
        "web.logout",
        "web_admin",
        identity.admin_id,
        serde_json::json!({}),
    )
    .await?;
    state.auth.logout(&identity).await?;
    let mut response = Redirect::to("/login").into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear_session_cookie(&state)).map_err(|_| WebError::internal())?,
    );
    Ok(response)
}

async fn not_found() -> Response {
    WebError::not_found("页面不存在").into_response()
}

fn validate_write(
    state: &WebState,
    identity: &SessionIdentity,
    headers: &HeaderMap,
    csrf: &str,
) -> WebResult<()> {
    if !valid_request_origin(state, headers) {
        return Err(WebError::forbidden("请求来源校验失败"));
    }
    if !state.auth.verify_form_csrf(identity, csrf) {
        return Err(WebError::forbidden("CSRF 校验失败，请刷新页面重试"));
    }
    Ok(())
}

async fn audit_success(
    state: &WebState,
    identity: &SessionIdentity,
    action: &str,
    entity_type: &str,
    entity_id: i64,
    metadata: serde_json::Value,
) -> WebResult<()> {
    audit_success_string(
        state,
        identity,
        action,
        entity_type,
        &entity_id.to_string(),
        metadata,
    )
    .await
}

async fn audit_success_string(
    state: &WebState,
    identity: &SessionIdentity,
    action: &str,
    entity_type: &str,
    entity_id: &str,
    metadata: serde_json::Value,
) -> WebResult<()> {
    state
        .repository
        .record_audit(
            "admin",
            Some(identity.admin_id),
            action,
            Some(entity_type),
            Some(entity_id),
            "success",
            None,
            None,
            &serde_json::to_string(&metadata).map_err(|_| WebError::internal())?,
        )
        .await?;
    Ok(())
}

fn valid_request_origin(state: &WebState, headers: &HeaderMap) -> bool {
    if let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        return origin.trim_end_matches('/') == state.public_origin;
    }
    headers
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| origin_of(value).ok())
        .is_some_and(|origin| origin == state.public_origin)
}

fn effective_source_ip(state: &WebState, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
    let trusted = state
        .config
        .web
        .trusted_proxy_cidrs
        .iter()
        .any(|network| network.contains(&peer));
    if trusted
        && let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .and_then(|value| value.trim().parse().ok())
    {
        return forwarded;
    }
    peer
}

fn origin_of(value: &str) -> Result<String> {
    let url =
        url::Url::parse(value).map_err(|_| AppError::Config("web public URL is invalid".into()))?;
    if !matches!(url.origin(), url::Origin::Tuple(..)) {
        return Err(AppError::Config("web public URL has no HTTP origin".into()));
    }
    Ok(url.origin().ascii_serialization())
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == name).then(|| value.to_string())
        })
}

fn session_cookie(state: &WebState, token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={}",
        state.config.web.session_absolute_secs
    )
}

fn clear_session_cookie(_state: &WebState) -> String {
    format!("{SESSION_COOKIE}=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0")
}

fn clear_session_redirect(state: &WebState) -> Response {
    let mut response = Redirect::to("/login").into_response();
    if let Ok(value) = HeaderValue::from_str(&clear_session_cookie(state)) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

fn heartbeat_status(value: Option<DateTime<Utc>>, timezone: Tz) -> String {
    match value {
        Some(value) if value + chrono::Duration::minutes(2) > Utc::now() => {
            format!("正常 · {}", format_time(Some(value), timezone))
        }
        Some(value) => format!("已停止或延迟 · {}", format_time(Some(value), timezone)),
        None => "尚无心跳".into(),
    }
}

fn provider_status(value: Option<DateTime<Utc>>, timezone: Tz) -> String {
    match value {
        Some(value) if value > Utc::now() => {
            format!("退避至 {}", format_time(Some(value), timezone))
        }
        _ => "可用".into(),
    }
}

fn canonical_bilibili_url(bvid: &str) -> Option<String> {
    parse_bilibili_bvid(bvid)
        .ok()
        .map(|bvid| format!("https://www.bilibili.com/video/{bvid}"))
}

fn bangumi_cover_url(subject_id: i64) -> Option<String> {
    (subject_id > 0).then(|| format!("/covers/{subject_id}"))
}

fn title_initial(title: &str) -> String {
    title
        .trim()
        .chars()
        .next()
        .map(|character| character.to_string())
        .unwrap_or_else(|| "番".into())
}

fn clean_bilibili_title(value: &str) -> String {
    let mut cleaned = String::with_capacity(value.len());
    let mut inside_tag = false;
    for character in value.chars() {
        match character {
            '<' => inside_tag = true,
            '>' if inside_tag => inside_tag = false,
            _ if !inside_tag => cleaned.push(character),
            _ => {}
        }
    }
    let cleaned = cleaned
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    cleaned.trim().to_string()
}

fn format_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds >= 3_600 {
        let hours = seconds / 3_600;
        let minutes = seconds % 3_600 / 60;
        format!("{hours} 小时 {minutes} 分")
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

fn candidate_state_label(state: &str) -> &str {
    match state {
        "pending" => "待审核",
        "confirmed" => "已确认",
        "rejected" => "已拒绝",
        "expired" => "已过期",
        _ => "未知状态",
    }
}

fn episode_state_label(state: &str) -> &str {
    match state {
        "waiting" => "等待更新",
        "watching" => "监控中",
        "candidate_found" => "发现候选",
        "confirmed" => "已确认",
        "notified" => "已通知",
        "needs_manual_review" => "需要审核",
        _ => "未知状态",
    }
}

fn schedule_source_label(source: &str) -> String {
    match source {
        "bilibili" => "Bilibili".into(),
        "unext" => "U-NEXT".into(),
        "danime" => "d Anime".into(),
        "abema" => "ABEMA".into(),
        "gamer" => "巴哈姆特动画疯".into(),
        "gamer_hk" => "巴哈姆特动画疯（香港）".into(),
        "anime_schedule" => "AnimeSchedule".into(),
        "anilist" => "AniList（旧数据）".into(),
        "bangumi-data" => "bangumi-data 默认时段".into(),
        "unknown" => "未知".into(),
        value => value.to_string(),
    }
}

fn schedule_confidence_label(confidence: &str) -> String {
    match confidence {
        "calibrated" => "章节日期 + 网络时段校准".into(),
        "stale" => "保留上次校准值".into(),
        "estimated" => "估算".into(),
        "date_only" => "仅开播日期，时间待公布".into(),
        "unavailable" => "时间不可用".into(),
        value => value.to_string(),
    }
}

fn format_time(value: Option<DateTime<Utc>>, timezone: Tz) -> String {
    value
        .map(|value| {
            value
                .with_timezone(&timezone)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "—".into())
}

fn format_schedule_time(
    value: Option<DateTime<Utc>>,
    confidence: Option<&str>,
    timezone: Tz,
) -> String {
    if confidence == Some("date_only") {
        return value
            .map(|value| {
                format!(
                    "{}（时间待公布）",
                    value.with_timezone(&timezone).format("%Y-%m-%d")
                )
            })
            .unwrap_or_else(|| "日期待公布".into());
    }
    format_time(value, timezone)
}

fn chinese_weekday(weekday: Weekday) -> &'static str {
    match weekday {
        Weekday::Mon => "周一",
        Weekday::Tue => "周二",
        Weekday::Wed => "周三",
        Weekday::Thu => "周四",
        Weekday::Fri => "周五",
        Weekday::Sat => "周六",
        Weekday::Sun => "周日",
    }
}

type WebResult<T> = std::result::Result<T, WebError>;
type WebResponse = WebResult<Response>;

fn render(template: impl Template) -> WebResponse {
    render_with_status(template, StatusCode::OK)
}
fn render_with_status(template: impl Template, status: StatusCode) -> WebResponse {
    let html = template.render().map_err(|_| WebError::internal())?;
    Ok((status, Html(html)).into_response())
}

#[derive(Debug)]
struct WebError {
    status: StatusCode,
    message: String,
}

impl WebError {
    fn bad_request(message: &str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
    fn forbidden(message: &str) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }
    fn not_found(message: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
    fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "服务器内部错误".into(),
        }
    }
}

impl From<AppError> for WebError {
    fn from(error: AppError) -> Self {
        match error {
            AppError::InvalidInput(message) => Self {
                status: StatusCode::BAD_REQUEST,
                message,
            },
            AppError::NotFound(_) => Self::not_found("目标不存在或已经被删除"),
            AppError::Database(error) if sqlite_database_is_busy(&error) => {
                tracing::warn!(%error, "SQLite database is busy or locked");
                Self {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    message: "数据库暂时忙，请稍后重试".into(),
                }
            }
            AppError::Database(error) => {
                tracing::error!(%error, "database operation failed");
                Self {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: "数据库操作失败，详细原因已写入服务日志".into(),
                }
            }
            _ => Self::internal(),
        }
    }
}

fn sqlite_database_is_busy(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(error) = error else {
        return false;
    };
    let code = error.code();
    let busy_code = sqlite_error_code_is_busy(code.as_deref());
    if busy_code {
        return true;
    }
    let message = error.message().to_ascii_lowercase();
    message.contains("database is locked") || message.contains("database is busy")
}

fn sqlite_error_code_is_busy(code: Option<&str>) -> bool {
    code.and_then(|code| code.parse::<i32>().ok())
        .is_some_and(|code| matches!(code & 0xff, 5 | 6))
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        #[derive(Template)]
        #[template(path = "error.html")]
        struct ErrorTemplate {
            status: u16,
            message: String,
        }
        let body = ErrorTemplate {
            status: self.status.as_u16(),
            message: self.message,
        }
        .render()
        .unwrap_or_else(|_| "请求失败".into());
        (self.status, Html(body)).into_response()
    }
}

impl From<WebError> for Response {
    fn from(error: WebError) -> Self {
        error.into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tempfile::TempDir;
    use tower::ServiceExt;

    use super::*;
    use crate::{
        auth::create_admin,
        domain::{AutoScheduleMetadata, NewAnime},
    };

    #[test]
    fn only_sqlite_busy_and_locked_codes_are_classified_as_database_busy() {
        assert!(sqlite_error_code_is_busy(Some("5")));
        assert!(sqlite_error_code_is_busy(Some("6")));
        assert!(sqlite_error_code_is_busy(Some("517")));
        assert!(sqlite_error_code_is_busy(Some("773")));
        assert!(!sqlite_error_code_is_busy(Some("19")));
        assert!(!sqlite_error_code_is_busy(Some("787")));
        assert!(!sqlite_error_code_is_busy(None));
    }

    #[test]
    fn cookie_parser_only_reads_exact_name() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("x=1; __Host-anipulse_session=token; other=2"),
        );
        assert_eq!(
            cookie_value(&headers, SESSION_COOKIE).as_deref(),
            Some("token")
        );
    }

    #[test]
    fn public_origin_drops_paths() {
        assert_eq!(
            origin_of("https://anime.example.com/path").unwrap(),
            "https://anime.example.com"
        );
    }

    #[test]
    fn candidate_link_uses_validated_bvid_instead_of_provider_url() {
        let view = candidate_view(CandidateListRow {
            episode_id: 1,
            anime_id: 2,
            bvid: "BV1Es8A6UEnr".into(),
            anime_title: "测试番剧".into(),
            episode_no: 7,
            uploader_mid: 3,
            uploader_name: "测试 UP".into(),
            title: "第7话 <em class=\"keyword\">测试番剧</em> &amp; 新内容".into(),
            duration_sec: 22_263,
            published_at: Utc::now(),
            score: -5,
            state: "pending".into(),
            seen_count: 1,
            evaluation_json: "{}".into(),
            url: "https://www.bilibili.com".into(),
            manually_trusted: false,
            globally_trusted: false,
        });

        assert_eq!(view.url, "https://www.bilibili.com/video/BV1Es8A6UEnr");
        assert!(view.has_video_url);
        assert_eq!(view.title, "第7话 测试番剧 & 新内容");
        assert_eq!(view.duration, "6 小时 11 分");
        assert_eq!(view.state, "待审核");
    }

    #[test]
    fn invalid_candidate_bvid_never_falls_back_to_bilibili_homepage() {
        assert!(canonical_bilibili_url("not-a-bvid").is_none());
    }

    #[test]
    fn bangumi_cover_uses_local_cache_endpoint() {
        assert_eq!(bangumi_cover_url(622206).as_deref(), Some("/covers/622206"));
        assert!(bangumi_cover_url(0).is_none());
        assert_eq!(title_initial("  恶女不才"), "恶");
        assert_eq!(title_initial("  "), "番");
    }

    #[tokio::test]
    async fn local_cover_response_supports_browser_cache_validation() {
        let asset = CoverAsset {
            bytes: vec![0xff, 0xd8, 0xff, 0xd9],
            content_type: "image/jpeg",
            etag: "\"cover-etag\"".into(),
        };
        let response = cover_response(asset, &HeaderMap::new());
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, max-age=86400"
        );
        assert_eq!(response.headers()[header::ETAG], "\"cover-etag\"");

        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"cover-etag\""),
        );
        let response = cover_response(
            CoverAsset {
                bytes: vec![0xff, 0xd8, 0xff, 0xd9],
                content_type: "image/jpeg",
                etag: "\"cover-etag\"".into(),
            },
            &headers,
        );
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn router_requires_auth_and_sets_security_headers() {
        let (_directory, repository, config, auth, app) = test_app().await;

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/anime")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/login");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_security_policy = response.headers()["content-security-policy"]
            .to_str()
            .unwrap();
        assert!(!content_security_policy.contains("https://api.bgm.tv"));
        assert!(!content_security_policy.contains("https://lain.bgm.tv"));
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/static/app.css?v=light-2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
        let body = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
        assert!(
            String::from_utf8(body.to_vec())
                .unwrap()
                .contains("color-scheme: light")
        );

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/static/favicon.svg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "image/svg+xml; charset=utf-8"
        );
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=604800"
        );
        let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
        assert!(String::from_utf8(body.to_vec()).unwrap().contains("<svg"));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
        assert!(
            String::from_utf8(body.to_vec())
                .unwrap()
                .contains("/static/favicon.svg")
        );

        drop((repository, config, auth));
    }

    #[tokio::test]
    async fn upcoming_anime_form_creates_a_draft_and_background_job() {
        let (_directory, repository, _config, auth, app) = test_app().await;
        create_admin(&repository, "admin", "correct-password-1234".to_string())
            .await
            .unwrap();
        let token = match auth
            .login(
                "admin",
                "correct-password-1234".to_string(),
                "127.0.0.1",
                Some("test"),
            )
            .await
            .unwrap()
        {
            LoginOutcome::Success(session) => session.token,
            _ => panic!("test login should succeed"),
        };
        let identity = auth.authenticate(&token).await.unwrap().unwrap();
        let mut form = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in [
            ("csrf_token", identity.csrf_token.as_str()),
            ("title", "一觉醒来坐拥神装和飞船"),
            ("next_episode", "1"),
            ("duration_min", "20"),
            ("duration_max", "40"),
            ("timezone", "Asia/Shanghai"),
            ("auto_schedule", "yes"),
            ("bangumi_id", "536270"),
            ("anilist_id", "186541"),
        ] {
            form.append_pair(name, value);
        }

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/anime/resolve")
                    .header(header::ORIGIN, "https://anime.example.com")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form.finish()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response.headers()[header::LOCATION].to_str().unwrap();
        let draft_id = location.strip_prefix("/anime/drafts/").unwrap();
        let draft = repository
            .anime_draft(draft_id, identity.admin_id, &identity.token_hmac)
            .await
            .unwrap();
        assert_eq!(draft.state, "queued");
    }

    #[tokio::test]
    async fn login_cookie_csrf_and_template_escaping_are_enforced() {
        let (_directory, repository, _config, auth, app) = test_app().await;
        create_admin(&repository, "admin", "correct-password-1234".to_string())
            .await
            .unwrap();
        repository
            .add_anime(NewAnime {
                title: "<script>alert(1)</script>".into(),
                aliases: Vec::new(),
                next_episode: 1,
                expected_at: None,
                expected_weekday: None,
                expected_time: None,
                timezone: "Asia/Shanghai".into(),
                duration_min_sec: 1_200,
                duration_max_sec: 1_680,
                auto_schedule: None,
            })
            .await
            .unwrap();
        repository
            .add_anime(NewAnime {
                title: "Re：从零开始的异世界生活 第四季 夺还篇".into(),
                aliases: Vec::new(),
                next_episode: 14,
                expected_at: None,
                expected_weekday: None,
                expected_time: None,
                timezone: "Asia/Shanghai".into(),
                duration_min_sec: 1_200,
                duration_max_sec: 1_680,
                auto_schedule: Some(AutoScheduleMetadata {
                    bangumi_subject_id: 633_836,
                    anilist_media_id: None,
                    anime_schedule_route: None,
                    total_episodes: None,
                    broadcast_pattern: "R/2026-08-12T13:00:00Z/P7D".into(),
                    next_sync_at: Utc::now(),
                    episode_mapping: None,
                    schedule_source: "unext".into(),
                    schedule_confidence: "calibrated".into(),
                    schedule_warning: None,
                }),
            })
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43210))))
                    .header(header::ORIGIN, "https://anime.example.com")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("username=admin&password=correct-password-1234"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let set_cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        for attribute in ["Secure", "HttpOnly", "SameSite=Lax", "Path=/"] {
            assert!(set_cookie.contains(attribute));
        }
        let cookie_pair = set_cookie.split(';').next().unwrap().to_owned();
        let raw_token = cookie_pair.split_once('=').unwrap().1;
        let identity = auth.authenticate(raw_token).await.unwrap().unwrap();

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/anime")
                    .header(header::COOKIE, &cookie_pair)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("&#60;script&#62;alert(1)&#60;/script&#62;"));
        assert!(!body.contains("<script>alert(1)</script>"));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/anime/1?result=video-enqueued")
                    .header(header::COOKIE, &cookie_pair)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("正在后台读取视频信息"));
        assert!(body.contains("这项任务可能需要几分钟"));
        assert!(body.contains("href=\"/jobs\""));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/anime/2/episode-mapping")
                    .header(header::COOKIE, &cookie_pair)
                    .header(header::ORIGIN, "https://anime.example.com")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token={}&search_episode_start=12&bangumi_episode_start=78",
                        identity.csrf_token
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers()[header::LOCATION],
            "/anime/2?result=mapping-updated"
        );
        let mapped = repository.get_anime(2).await.unwrap();
        assert_eq!(mapped.anime.local_episode_origin, Some(12));
        assert_eq!(mapped.anime.bangumi_episode_origin, Some(78));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/anime/2")
                    .header(header::COOKIE, &cookie_pair)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("站内 EP12 ↔ Bangumi EP78；当前 EP14 ↔ EP80"));
        assert!(body.contains("value=\"12\""));
        assert!(body.contains("value=\"78\""));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/anime/1/disable")
                    .header(header::COOKIE, &cookie_pair)
                    .header(header::ORIGIN, "https://anime.example.com")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("csrf_token=wrong"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/anime/1/disable")
                    .header(header::COOKIE, &cookie_pair)
                    .header(header::ORIGIN, "https://anime.example.com")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("csrf_token={}", identity.csrf_token)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(!repository.get_anime(1).await.unwrap().anime.enabled);
    }

    #[test]
    fn web_times_are_rendered_in_the_configured_timezone() {
        let value = DateTime::parse_from_rfc3339("2026-08-21T13:25:20Z")
            .unwrap()
            .with_timezone(&Utc);

        assert_eq!(
            format_time(Some(value), chrono_tz::Asia::Shanghai),
            "2026-08-21 21:25:20"
        );
        assert_eq!(
            format_schedule_time(Some(value), Some("date_only"), chrono_tz::Asia::Shanghai),
            "2026-08-21（时间待公布）"
        );
    }

    #[test]
    fn anime_management_job_target_includes_title_and_detail_link() {
        let view = job_view(
            ManagementJobListRow {
                id: 42,
                kind: "sync_schedule".into(),
                target_type: Some("anime".into()),
                target_id: Some("11".into()),
                state: "completed".into(),
                created_at: Utc::now(),
                error: None,
                anime_title: Some("药屋少女的呢喃 第三季".into()),
            },
            chrono_tz::Asia::Shanghai,
        );

        assert_eq!(view.target, "药屋少女的呢喃 第三季 · #11");
        assert_eq!(view.target_url, "/anime/11");
        assert!(view.has_target_url);
    }

    #[test]
    fn episode_history_uses_explicit_choice_then_highest_unblocked_score() {
        let row = |id: i64, score: i64, preferred: bool, blocked: bool| EpisodeVideoRow {
            id,
            episode_id: 70,
            episode_no: 7,
            bvid: format!("BVhistory{id:04}"),
            title: format!("episode seven source {id}"),
            uploader_mid: id + 100,
            uploader_name: format!("up {id}"),
            duration_sec: 1_400,
            score,
            is_preferred: preferred,
            updated_at: Utc::now(),
            confirmed_count: 0,
            rejected_count: 0,
            manually_trusted: false,
            globally_trusted: false,
            manually_blocked: blocked,
        };

        let automatic = episode_history_views(
            vec![row(2, 90, false, false), row(1, 40, false, false)],
            2,
            chrono_tz::Asia::Shanghai,
        );
        assert_eq!(automatic[0].primary.id, 2);
        assert!(!automatic[0].has_explicit_preferred);

        let explicit = episode_history_views(
            vec![row(1, 40, true, false), row(2, 90, false, false)],
            2,
            chrono_tz::Asia::Shanghai,
        );
        assert_eq!(explicit[0].primary.id, 1);
        assert!(explicit[0].has_explicit_preferred);

        let blocked_choice = episode_history_views(
            vec![row(1, 100, true, true), row(2, 90, false, false)],
            2,
            chrono_tz::Asia::Shanghai,
        );
        assert_eq!(blocked_choice[0].primary.id, 2);
        assert!(!blocked_choice[0].has_explicit_preferred);
    }

    #[test]
    fn uploader_return_path_is_limited_to_the_current_anime_detail() {
        assert_eq!(
            uploader_action_redirect(1, Some("/anime/1"), "uploader-trusted", None),
            "/anime/1?result=uploader-trusted"
        );
        assert_eq!(
            uploader_action_redirect(
                1,
                Some("https://example.com/steal"),
                "uploader-trusted",
                None,
            ),
            "/candidates?state=pending&result=uploader-trusted"
        );
    }

    #[test]
    fn empty_bangumi_id_is_treated_as_an_omitted_optional_field() {
        let form: AnimeResolveForm = serde_json::from_value(serde_json::json!({
            "csrf_token": "token",
            "title": "测试番剧",
            "next_episode": 1,
            "duration_min": 20,
            "duration_max": 30,
            "timezone": "Asia/Shanghai",
            "auto_schedule": "yes",
            "bangumi_id": ""
        }))
        .unwrap();

        assert_eq!(
            parse_optional_bangumi_id(form.bangumi_id.as_deref()).unwrap(),
            None
        );
        assert_eq!(
            parse_optional_bangumi_id(Some(" 622206 ")).unwrap(),
            Some(622_206)
        );
        assert!(parse_optional_bangumi_id(Some("not-an-id")).is_err());
        assert!(parse_optional_bangumi_id(Some("0")).is_err());
    }

    #[test]
    fn episode_mapping_requires_both_positive_origins() {
        assert_eq!(parse_episode_mapping(None, None).unwrap(), None);
        assert_eq!(
            parse_episode_mapping(Some("12"), Some("78")).unwrap(),
            Some(EpisodeNumberMapping {
                local_origin: 12,
                bangumi_origin: 78,
            })
        );
        assert!(parse_episode_mapping(Some("12"), None).is_err());
        assert!(parse_episode_mapping(Some("0"), Some("78")).is_err());
    }

    async fn test_app() -> (TempDir, Repository, Arc<AppConfig>, AuthService, Router) {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("test.db");
        let repository = Repository::connect(database.to_str().unwrap())
            .await
            .unwrap();
        let mut config = AppConfig::default();
        config.web.public_url = "https://anime.example.com".into();
        config.web.cover_cache_dir = directory.path().join("covers").display().to_string();
        let config = Arc::new(config);
        let auth = AuthService::for_test(repository.clone(), config.web.clone())
            .await
            .unwrap();
        let state = WebState {
            application: ApplicationService::new(repository.clone(), config.clone()),
            repository: repository.clone(),
            auth: auth.clone(),
            config: config.clone(),
            public_origin: origin_of(&config.web.public_url).unwrap(),
            display_timezone: config.web.timezone.parse().unwrap(),
            cover_cache: CoverCache::new(&config).unwrap(),
        };
        let app = build_router(state, &config);
        (directory, repository, config, auth, app)
    }
}
