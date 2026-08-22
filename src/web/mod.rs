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
use chrono::{DateTime, Utc};
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
    error::{AppError, Result},
    repository::{AuditEventRow, CandidateListRow, ManagementJob, Repository},
};
use cover_cache::{CoverAsset, CoverCache};

const SESSION_COOKIE: &str = "__Host-anipulse_session";
const APP_CSS: &str = include_str!("../../static/app.css");

#[derive(Clone)]
struct WebState {
    repository: Repository,
    application: ApplicationService,
    auth: AuthService,
    config: Arc<AppConfig>,
    public_origin: String,
    cover_cache: CoverCache,
}

pub async fn serve(repository: Repository, config: Arc<AppConfig>) -> Result<()> {
    repository.cleanup_web_ephemera().await?;
    let cover_cache = CoverCache::new(&config)?;
    cover_cache.initialize(&repository).await?;
    let auth = AuthService::from_env(repository.clone(), config.web.clone()).await?;
    let public_origin = origin_of(&config.web.public_url)?;
    let state = WebState {
        application: ApplicationService::new(repository.clone(), config.clone()),
        repository: repository.clone(),
        auth,
        config: config.clone(),
        public_origin,
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
        .route("/anime/{id}/delete-intent", post(anime_delete_intent))
        .route("/anime/{id}/delete", post(anime_delete))
        .route("/covers/{subject_id}", get(cover_image))
        .route("/candidates", get(candidate_list))
        .route(
            "/candidates/{bvid}/accept-intent",
            post(candidate_accept_intent),
        )
        .route("/candidates/{bvid}/accept", post(candidate_accept))
        .route("/candidates/{bvid}/reject", post(candidate_reject))
        .route("/review/episodes/{id}", get(review_episode))
        .route(
            "/episodes/{id}/candidates/reject-all-intent",
            post(reject_all_intent),
        )
        .route("/episodes/{id}/candidates/reject-all", post(reject_all))
        .route(
            "/episodes/{id}/candidates/from-url",
            post(candidate_from_url),
        )
        .route(
            "/anime/{anime_id}/uploaders/{mid}/trust",
            post(uploader_trust),
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
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=300"),
    );
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
                message: format!("登录尝试过多，请在 {} 后重试", format_time(Some(until))),
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
}

async fn dashboard(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
) -> WebResponse {
    let stats = state.repository.dashboard_stats().await?;
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
        scheduler_status: heartbeat_status(stats.scheduler_heartbeat),
        provider_status: provider_status(stats.provider_backoff_until),
    })
}

struct AnimeListItem {
    id: i64,
    title: String,
    cover_url: String,
    has_cover: bool,
    cover_initial: String,
    episode: String,
    next_check: String,
    schedule: String,
    enabled: bool,
}

#[derive(Template)]
#[template(path = "anime_list.html")]
struct AnimeListTemplate {
    username: String,
    csrf_token: String,
    items: Vec<AnimeListItem>,
}

async fn anime_list(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
) -> WebResponse {
    let mut items = Vec::new();
    for anime in state.repository.list_anime().await? {
        let episode = state.repository.active_episode(anime.id).await.ok();
        let cover_url = anime.bangumi_subject_id.and_then(bangumi_cover_url);
        let cover_initial = title_initial(&anime.title);
        items.push(AnimeListItem {
            id: anime.id,
            title: anime.title,
            has_cover: cover_url.is_some(),
            cover_url: cover_url.unwrap_or_default(),
            cover_initial,
            episode: episode
                .as_ref()
                .map(|episode| {
                    format!(
                        "EP{} · {}",
                        episode.episode_no,
                        episode_state_label(&episode.state)
                    )
                })
                .unwrap_or_else(|| "—".into()),
            next_check: episode
                .as_ref()
                .map(|episode| format_time(Some(episode.next_check_at)))
                .unwrap_or_else(|| "—".into()),
            schedule: if anime.auto_schedule {
                anime
                    .bangumi_subject_id
                    .map(|id| format!("自动 · #{id}"))
                    .unwrap_or_else(|| "自动".into())
            } else {
                "手工".into()
            },
            enabled: anime.enabled,
        });
    }
    render(AnimeListTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        items,
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
    bangumi_id: Option<i64>,
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
                bangumi_id: auto_schedule.then_some(form.bangumi_id).flatten(),
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
    next_episode: i64,
    expected_at: String,
    aliases: String,
    duration: String,
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
    let (title, matched_title, bangumi_id, next_episode, expected_at, aliases, duration, warning) =
        if let Some(resolved) = resolved {
            (
                resolved.title,
                resolved.matched_title.unwrap_or_else(|| "手工排期".into()),
                resolved
                    .bangumi_subject_id
                    .map(|id| format!("#{id}"))
                    .unwrap_or_else(|| "未绑定".into()),
                resolved.next_episode,
                format_time(resolved.expected_at),
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
                resolved.warning.unwrap_or_default(),
            )
        } else {
            (
                String::new(),
                String::new(),
                String::new(),
                0,
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
        next_episode,
        expected_at,
        aliases,
        duration,
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
    title: String,
    cover_url: String,
    has_cover: bool,
    cover_initial: String,
    aliases: String,
    episode: String,
    expected_at: String,
    next_check: String,
    duration: String,
    bangumi: String,
    enabled: bool,
    auto_schedule: bool,
    episode_id: i64,
}

async fn anime_detail(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<i64>,
) -> WebResponse {
    let anime = state.repository.get_anime(id).await?;
    let episode = state.repository.active_episode(id).await.ok();
    let cover_url = anime.anime.bangumi_subject_id.and_then(bangumi_cover_url);
    let cover_initial = title_initial(&anime.anime.title);
    render(AnimeDetailTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        id,
        title: anime.anime.title,
        has_cover: cover_url.is_some(),
        cover_url: cover_url.unwrap_or_default(),
        cover_initial,
        aliases: anime.aliases.join("、"),
        episode: episode
            .as_ref()
            .map(|episode| {
                format!(
                    "EP{} · {}",
                    episode.episode_no,
                    episode_state_label(&episode.state)
                )
            })
            .unwrap_or_else(|| "—".into()),
        expected_at: episode
            .as_ref()
            .and_then(|episode| episode.expected_at)
            .map(|value| format_time(Some(value)))
            .unwrap_or_else(|| "未知".into()),
        next_check: episode
            .as_ref()
            .map(|episode| format_time(Some(episode.next_check_at)))
            .unwrap_or_else(|| "—".into()),
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
        enabled: anime.anime.enabled,
        auto_schedule: anime.anime.auto_schedule,
        episode_id: episode.as_ref().map(|episode| episode.id).unwrap_or(0),
    })
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
}

struct CandidateView {
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
    url: String,
    has_video_url: bool,
    pending: bool,
}

#[derive(Template)]
#[template(path = "candidates.html")]
struct CandidateTemplate {
    username: String,
    csrf_token: String,
    items: Vec<CandidateView>,
    pending_selected: bool,
    all_selected: bool,
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
    render(CandidateTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        items,
        pending_selected: selected == "pending",
        all_selected: selected == "all",
    })
}

fn candidate_view(row: CandidateListRow) -> CandidateView {
    let evaluation = serde_json::from_str::<serde_json::Value>(&row.evaluation_json)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or_else(|_| "判定详情不可用".into());
    let url = canonical_bilibili_url(&row.bvid);
    CandidateView {
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
        has_video_url: url.is_some(),
        url: url.unwrap_or_default(),
    }
}

async fn candidate_accept_intent(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(bvid): Path<String>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state.repository.candidate_context(&bvid).await?;
    let nonce = state
        .auth
        .issue_action_nonce(&identity, "candidate.accept", Some(&bvid))
        .await?;
    let (candidate, episode) = state.repository.candidate_context(&bvid).await?;
    let anime = state.repository.get_anime(episode.anime_id).await?;
    render(CandidateAcceptTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        nonce,
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
    Path(bvid): Path<String>,
    headers: HeaderMap,
    Form(form): Form<CandidateAcceptForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    if !state
        .auth
        .consume_action_nonce(&identity, &form.nonce, "candidate.accept", Some(&bvid))
        .await?
    {
        return Err(WebError::forbidden("候选确认已使用或过期，请重新开始"));
    }
    state.application.accept_candidate(&bvid).await?;
    audit_success_string(
        &state,
        &identity,
        "candidate.accept",
        "candidate",
        &bvid,
        serde_json::json!({}),
    )
    .await?;
    Ok(Redirect::to("/candidates?state=pending").into_response())
}

async fn uploader_trust(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path((anime_id, mid)): Path<(i64, i64)>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
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
    Ok(Redirect::to("/candidates?state=pending").into_response())
}

async fn uploader_block(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path((anime_id, mid)): Path<(i64, i64)>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state
        .application
        .set_uploader_flag(anime_id, mid, false, true)
        .await?;
    audit_success_string(
        &state,
        &identity,
        "uploader.block",
        "uploader",
        &format!("{anime_id}:{mid}"),
        serde_json::json!({}),
    )
    .await?;
    Ok(Redirect::to("/candidates?state=pending").into_response())
}

async fn candidate_reject(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
    Path(bvid): Path<String>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> WebResponse {
    validate_write(&state, &identity, &headers, &form.csrf_token)?;
    state.application.reject_candidate(&bvid).await?;
    audit_success_string(
        &state,
        &identity,
        "candidate.reject",
        "candidate",
        &bvid,
        serde_json::json!({}),
    )
    .await?;
    Ok(Redirect::to("/candidates?state=pending").into_response())
}

struct ReviewCandidate {
    bvid: String,
    title: String,
    uploader: String,
    duration: String,
    score: i64,
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
            ReviewCandidate {
                bvid: candidate.bvid,
                title: clean_bilibili_title(&candidate.title),
                uploader: candidate.uploader_name,
                duration: format_duration(candidate.duration_sec),
                score: candidate.score,
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
        .list_management_jobs(100)
        .await?
        .into_iter()
        .map(job_view)
        .collect();
    render(JobTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        items,
    })
}

fn job_view(job: ManagementJob) -> JobView {
    JobView {
        id: job.id,
        kind: job.kind,
        target: job.target_id.unwrap_or_else(|| "—".into()),
        state: job.state,
        created_at: format_time(Some(job.created_at)),
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
        .map(audit_view)
        .collect();
    render(AuditTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        items,
    })
}

fn audit_view(event: AuditEventRow) -> AuditView {
    AuditView {
        created_at: format_time(Some(event.created_at)),
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
}

async fn settings_status(
    State(state): State<WebState>,
    Extension(identity): Extension<SessionIdentity>,
) -> WebResponse {
    let stats = state.repository.dashboard_stats().await?;
    render(SettingsTemplate {
        username: identity.username,
        csrf_token: identity.csrf_token,
        scheduler: heartbeat_status(stats.scheduler_heartbeat),
        provider: provider_status(stats.provider_backoff_until),
        notification_provider: state.config.notification.provider.clone(),
        schedule_source: state.config.schedule.bangumi_data_url.clone(),
        public_url: state.config.web.public_url.clone(),
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

fn heartbeat_status(value: Option<DateTime<Utc>>) -> String {
    match value {
        Some(value) if value + chrono::Duration::minutes(2) > Utc::now() => {
            format!("正常 · {}", format_time(Some(value)))
        }
        Some(value) => format!("已停止或延迟 · {}", format_time(Some(value))),
        None => "尚无心跳".into(),
    }
}

fn provider_status(value: Option<DateTime<Utc>>) -> String {
    match value {
        Some(value) if value > Utc::now() => format!("退避至 {}", format_time(Some(value))),
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

fn format_time(value: Option<DateTime<Utc>>) -> String {
    value
        .map(|value| value.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "—".into())
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
            AppError::Database(_) => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "数据库暂时忙，请稍后重试".into(),
            },
            _ => Self::internal(),
        }
    }
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
    use crate::{auth::create_admin, domain::NewAnime};

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
                    .uri("/static/app.css?v=light-1")
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

        drop((repository, config, auth));
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
            cover_cache: CoverCache::new(&config).unwrap(),
        };
        let app = build_router(state, &config);
        (directory, repository, config, auth, app)
    }
}
