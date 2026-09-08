use std::{path::PathBuf, str::FromStr, sync::Arc};

use anipulse::{
    application::ApplicationService,
    auth::{create_admin, normalize_username, reset_admin_password},
    config::AppConfig,
    detector::Detector,
    domain::{AutoScheduleMetadata, EpisodeNumberMapping, NewAnime},
    error::{AppError, Result},
    notification::NotificationDispatcher,
    provider::BilibiliProvider,
    repository::Repository,
    schedule::{AutoScheduleRequest, ScheduleProvider, ScheduleSynchronizer},
    scheduler,
};
use chrono::{Datelike, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use clap::{Args, Parser, Subcommand};
use tracing::error;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "anipulse",
    version,
    about = "Conservative anime release watcher"
)]
struct Cli {
    #[arg(
        long,
        env = "ANIPULSE_CONFIG",
        default_value = "config.toml",
        global = true
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Run,
    Web,
    Check {
        anime_id: Option<i64>,
    },
    Anime {
        #[command(subcommand)]
        command: AnimeCommand,
    },
    Candidate {
        #[command(subcommand)]
        command: CandidateCommand,
    },
    Uploader {
        #[command(subcommand)]
        command: UploaderCommand,
    },
    Notification {
        #[command(subcommand)]
        command: NotificationCommand,
    },
    Database {
        #[command(subcommand)]
        command: DatabaseCommand,
    },
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AnimeCommand {
    Add(Box<AddAnimeArgs>),
    List,
    Show {
        anime_id: i64,
    },
    Edit {
        anime_id: i64,
        #[arg(
            long,
            help = "new primary title; the previous title is kept as an alias"
        )]
        title: String,
    },
    Enable {
        anime_id: i64,
    },
    Disable {
        anime_id: i64,
    },
    Remove {
        anime_id: i64,
        #[arg(
            long,
            help = "confirm permanent deletion of the anime and all related records"
        )]
        yes: bool,
    },
    RepairEpisode {
        anime_id: i64,
        #[arg(long, help = "episode number that should be monitored again")]
        episode: i64,
        #[arg(
            long,
            help = "confirm removal of the wrong notification and later episode state"
        )]
        yes: bool,
    },
    Sync {
        anime_id: i64,
    },
}

#[derive(Debug, Args)]
struct AddAnimeArgs {
    #[arg(long)]
    title: String,
    #[arg(long = "alias")]
    aliases: Vec<String>,
    #[arg(long, default_value_t = 1)]
    next_episode: i64,
    #[arg(
        long,
        conflicts_with_all = ["expected_at", "weekday", "time"],
        help = "fill aliases and broadcast time from bangumi-data"
    )]
    auto_schedule: bool,
    #[arg(long, requires = "auto_schedule")]
    bangumi_id: Option<i64>,
    #[arg(
        long,
        requires_all = ["auto_schedule", "bangumi_id"],
        help = "AniList media ID used only as an AnimeSchedule lookup key"
    )]
    anilist_id: Option<i64>,
    #[arg(
        long,
        requires_all = ["auto_schedule", "bangumi_id"],
        help = "explicit AnimeSchedule route (slug) when automatic matching is ambiguous"
    )]
    anime_schedule_route: Option<String>,
    #[arg(
        long,
        requires_all = ["auto_schedule", "bangumi_episode_start"],
        help = "first episode number used for Bilibili search in this Bangumi subject"
    )]
    search_episode_start: Option<i64>,
    #[arg(
        long,
        requires_all = ["auto_schedule", "search_episode_start"],
        help = "Bangumi episode number corresponding to --search-episode-start"
    )]
    bangumi_episode_start: Option<i64>,
    #[arg(long, help = "RFC3339 timestamp; overrides --weekday/--time")]
    expected_at: Option<String>,
    #[arg(long, help = "monday..sunday")]
    weekday: Option<String>,
    #[arg(long, help = "local HH:MM")]
    time: Option<String>,
    #[arg(long, default_value = "Asia/Shanghai")]
    timezone: String,
    #[arg(long, default_value = "20m")]
    duration_min: String,
    #[arg(long, default_value = "28m")]
    duration_max: String,
}

#[derive(Debug, Subcommand)]
enum CandidateCommand {
    List {
        #[arg(long, default_value = "pending")]
        state: String,
        #[arg(long)]
        explain: bool,
    },
    Accept {
        bvid: String,
    },
    AcceptUrl {
        anime_id: i64,
        url: String,
    },
    Reject {
        bvid: String,
    },
    RejectAll {
        anime_id: i64,
        #[arg(
            long,
            help = "confirm rejection of every pending candidate for the active episode"
        )]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum UploaderCommand {
    Trust { anime_id: i64, mid: i64 },
    Block { anime_id: i64, mid: i64 },
}

#[derive(Debug, Subcommand)]
enum NotificationCommand {
    Test,
}

#[derive(Debug, Subcommand)]
enum DatabaseCommand {
    Migrate,
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    Sessions {
        #[command(subcommand)]
        command: SessionCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    Create {
        #[arg(long)]
        username: String,
    },
    ResetPassword {
        #[arg(long)]
        username: String,
    },
    Disable {
        #[arg(long)]
        username: String,
    },
    Enable {
        #[arg(long)]
        username: String,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    RevokeAll {
        #[arg(long)]
        username: String,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    if let Err(error) = execute(Cli::parse()).await {
        error!(%error);
        std::process::exit(1);
    }
}

async fn execute(cli: Cli) -> Result<()> {
    let config = Arc::new(AppConfig::load(&cli.config)?);
    let repository = Repository::connect(&config.database.path).await?;
    let application = ApplicationService::new(repository.clone(), config.clone());
    match cli.command {
        Command::Anime { command } => {
            handle_anime(command, &application, &repository, &config).await
        }
        Command::Candidate { command } => {
            handle_candidate(command, &application, &repository).await
        }
        Command::Uploader { command } => handle_uploader(command, &application).await,
        Command::Notification {
            command: NotificationCommand::Test,
        } => {
            NotificationDispatcher::new(repository, &config.notification, &config.web.public_url)?
                .test()
                .await
        }
        Command::Database {
            command: DatabaseCommand::Migrate,
        } => {
            println!("database migrations are up to date");
            Ok(())
        }
        Command::Auth { command } => handle_auth(command, &repository).await,
        Command::Check { anime_id } => {
            let (detector, dispatcher) = build_runtime(repository.clone(), config.clone())?;
            let ids = match anime_id {
                Some(id) => vec![id],
                None => repository.enabled_anime_ids().await?,
            };
            for id in ids {
                detector.check_anime(id).await?;
            }
            dispatcher.dispatch_pending().await
        }
        Command::Run => {
            let (detector, dispatcher) = build_runtime(repository.clone(), config.clone())?;
            scheduler::run(repository, detector, dispatcher, config).await
        }
        Command::Web => anipulse::web::serve(repository, config).await,
    }
}

async fn handle_auth(command: AuthCommand, repository: &Repository) -> Result<()> {
    match command {
        AuthCommand::Admin {
            command: AdminCommand::Create { username },
        } => {
            let username = normalize_username(&username)?;
            let password = prompt_new_password()?;
            let admin_id = create_admin(repository, &username, password).await?;
            repository
                .record_audit(
                    "cli",
                    Some(admin_id),
                    "auth.admin.create",
                    Some("web_admin"),
                    Some(&admin_id.to_string()),
                    "success",
                    None,
                    None,
                    "{}",
                )
                .await?;
            println!("created owner administrator {username:?}");
        }
        AuthCommand::Admin {
            command: AdminCommand::ResetPassword { username },
        } => {
            let username = normalize_username(&username)?;
            reset_admin_password(repository, &username, prompt_new_password()?).await?;
            let admin = repository
                .web_admin_by_username(&username)
                .await?
                .ok_or_else(|| AppError::NotFound(format!("web admin {username}")))?;
            repository
                .record_audit(
                    "cli",
                    Some(admin.id),
                    "auth.admin.reset_password",
                    Some("web_admin"),
                    Some(&admin.id.to_string()),
                    "success",
                    None,
                    None,
                    "{}",
                )
                .await?;
            println!("reset password and revoked all sessions for {username:?}");
        }
        AuthCommand::Admin {
            command: AdminCommand::Disable { username },
        } => {
            let username = normalize_username(&username)?;
            repository.set_web_admin_disabled(&username, true).await?;
            let admin = repository
                .web_admin_by_username(&username)
                .await?
                .ok_or_else(|| AppError::NotFound(format!("web admin {username}")))?;
            repository
                .record_audit(
                    "cli",
                    Some(admin.id),
                    "auth.admin.disable",
                    Some("web_admin"),
                    Some(&admin.id.to_string()),
                    "success",
                    None,
                    None,
                    "{}",
                )
                .await?;
            println!("disabled {username:?} and revoked all sessions");
        }
        AuthCommand::Admin {
            command: AdminCommand::Enable { username },
        } => {
            let username = normalize_username(&username)?;
            repository.set_web_admin_disabled(&username, false).await?;
            let admin = repository
                .web_admin_by_username(&username)
                .await?
                .ok_or_else(|| AppError::NotFound(format!("web admin {username}")))?;
            repository
                .record_audit(
                    "cli",
                    Some(admin.id),
                    "auth.admin.enable",
                    Some("web_admin"),
                    Some(&admin.id.to_string()),
                    "success",
                    None,
                    None,
                    "{}",
                )
                .await?;
            println!("enabled {username:?}");
        }
        AuthCommand::Sessions {
            command: SessionCommand::RevokeAll { username },
        } => {
            let username = normalize_username(&username)?;
            let count = repository.revoke_web_admin_sessions(&username).await?;
            let admin = repository
                .web_admin_by_username(&username)
                .await?
                .ok_or_else(|| AppError::NotFound(format!("web admin {username}")))?;
            repository
                .record_audit(
                    "cli",
                    Some(admin.id),
                    "auth.sessions.revoke_all",
                    Some("web_admin"),
                    Some(&admin.id.to_string()),
                    "success",
                    None,
                    None,
                    &serde_json::json!({"count": count}).to_string(),
                )
                .await?;
            println!("revoked {count} active session(s) for {username:?}");
        }
    }
    Ok(())
}

fn prompt_new_password() -> Result<String> {
    let password = rpassword::prompt_password("New password: ").map_err(|error| {
        AppError::InvalidInput(format!("cannot read password from TTY: {error}"))
    })?;
    let confirmation = rpassword::prompt_password("Repeat password: ").map_err(|error| {
        AppError::InvalidInput(format!("cannot read password from TTY: {error}"))
    })?;
    if password != confirmation {
        return Err(AppError::InvalidInput("passwords do not match".into()));
    }
    anipulse::auth::PasswordService::validate_password(&password)?;
    Ok(password)
}

fn build_runtime(
    repository: Repository,
    config: Arc<AppConfig>,
) -> Result<(Detector, NotificationDispatcher)> {
    let provider = Arc::new(BilibiliProvider::new(
        config.bilibili.clone(),
        repository.clone(),
    )?);
    let detector = Detector::new(repository.clone(), provider, config.clone());
    let dispatcher =
        NotificationDispatcher::new(repository, &config.notification, &config.web.public_url)?;
    Ok((detector, dispatcher))
}

async fn handle_anime(
    command: AnimeCommand,
    application: &ApplicationService,
    repository: &Repository,
    config: &AppConfig,
) -> Result<()> {
    match command {
        AnimeCommand::Add(args) => {
            Tz::from_str(&args.timezone).map_err(|_| {
                AppError::InvalidInput(format!("invalid timezone: {}", args.timezone))
            })?;
            let episode_mapping = args
                .search_episode_start
                .zip(args.bangumi_episode_start)
                .map(|(local_origin, bangumi_origin)| EpisodeNumberMapping {
                    local_origin,
                    bangumi_origin,
                });
            if let Some(mapping) = episode_mapping {
                mapping.mapped_numbers(args.next_episode)?;
            }
            let mut aliases = args.aliases;
            let (expected_weekday, expected_time, expected_at, auto_schedule) = if args
                .auto_schedule
            {
                let provider = ScheduleProvider::new(config.schedule.clone())?;
                let catalog = provider.load_catalog().await?;
                let resolved = provider
                    .resolve_auto(
                        &catalog,
                        AutoScheduleRequest {
                            title: &args.title,
                            subject_id: args.bangumi_id,
                            next_episode: args.next_episode,
                            episode_mapping,
                            anilist_media_id: args.anilist_id,
                            anime_schedule_route: args.anime_schedule_route.as_deref(),
                            timezone: &args.timezone,
                        },
                    )
                    .await?;
                aliases.extend(resolved.aliases.iter().cloned());
                let mapped_episode = episode_mapping
                    .and_then(|mapping| mapping.mapped_numbers(args.next_episode).ok())
                    .map(|(_, bangumi_episode)| format!(" / Bangumi EP{bangumi_episode}"))
                    .unwrap_or_default();
                println!(
                    "matched Bangumi subject #{}: {}; EP{}{} expected at {} ({}, {})",
                    resolved.bangumi_subject_id,
                    resolved.matched_title,
                    args.next_episode,
                    mapped_episode,
                    resolved
                        .expected_at
                        .map(|value| value.to_rfc3339())
                        .unwrap_or_else(|| "unknown".into()),
                    resolved.schedule_source,
                    resolved.schedule_confidence,
                );
                if let Some(total) = resolved.total_episodes {
                    let final_episode = episode_mapping
                        .map(|mapping| mapping.final_local_episode(total))
                        .transpose()?
                        .unwrap_or(total);
                    if final_episode == total {
                        println!("Bangumi reports {total} regular episodes");
                    } else {
                        println!(
                            "Bangumi reports {total} regular episodes; mapped final local episode is EP{final_episode}"
                        );
                    }
                }
                if let Some(warning) = &resolved.schedule_warning {
                    println!("schedule warning: {warning}");
                }
                (
                    resolved.expected_weekday,
                    resolved.expected_time.clone(),
                    resolved.expected_at,
                    Some(AutoScheduleMetadata {
                        bangumi_subject_id: resolved.bangumi_subject_id,
                        anilist_media_id: resolved.anilist_media_id,
                        anime_schedule_route: resolved.anime_schedule_route,
                        total_episodes: resolved.total_episodes,
                        broadcast_pattern: resolved.broadcast_pattern,
                        schedule_source: resolved.schedule_source,
                        schedule_confidence: resolved.schedule_confidence,
                        schedule_warning: resolved.schedule_warning,
                        next_sync_at: Utc::now()
                            + chrono::Duration::seconds(config.schedule.sync_interval_secs as i64),
                        episode_mapping,
                    }),
                )
            } else {
                let expected_weekday = args.weekday.as_deref().map(parse_weekday).transpose()?;
                let expected_time = args
                    .time
                    .as_deref()
                    .map(parse_time)
                    .transpose()?
                    .map(|time| time.format("%H:%M").to_string());
                let expected_at = if let Some(value) = args.expected_at.as_deref() {
                    Some(
                        chrono::DateTime::parse_from_rfc3339(value)
                            .map_err(|e| {
                                AppError::InvalidInput(format!("invalid --expected-at: {e}"))
                            })?
                            .with_timezone(&Utc),
                    )
                } else if let (Some(weekday), Some(time)) =
                    (expected_weekday, expected_time.as_deref())
                {
                    Some(next_weekly_occurrence(
                        &args.timezone,
                        weekday,
                        parse_time(time)?,
                    )?)
                } else {
                    None
                };
                (
                    expected_weekday.map(|day| i64::from(day.num_days_from_monday())),
                    expected_time,
                    expected_at,
                    None,
                )
            };
            let id = repository
                .add_anime(NewAnime {
                    title: args.title,
                    aliases,
                    next_episode: args.next_episode,
                    expected_at,
                    expected_weekday,
                    expected_time,
                    timezone: args.timezone,
                    duration_min_sec: parse_duration_arg(&args.duration_min)?,
                    duration_max_sec: parse_duration_arg(&args.duration_max)?,
                    auto_schedule,
                })
                .await?;
            println!("added anime id={id}");
            Ok(())
        }
        AnimeCommand::List => {
            println!("ID\tENABLED\tAUTO\tBANGUMI\tTITLE\tDURATION");
            for anime in repository.list_anime().await? {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}-{}m",
                    anime.id,
                    anime.enabled,
                    anime.auto_schedule,
                    anime
                        .bangumi_subject_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "-".into()),
                    anime.title,
                    anime.duration_min_sec / 60,
                    anime.duration_max_sec / 60
                );
            }
            Ok(())
        }
        AnimeCommand::Show { anime_id } => {
            let anime = repository.get_anime(anime_id).await?;
            let episode = repository.active_episode(anime_id).await.ok();
            println!("id: {}", anime.anime.id);
            println!("title: {}", anime.anime.title);
            println!("aliases: {}", anime.aliases.join(", "));
            println!("enabled: {}", anime.anime.enabled);
            println!("timezone: {}", anime.anime.timezone);
            println!("auto schedule: {}", anime.anime.auto_schedule);
            if let Some(subject_id) = anime.anime.bangumi_subject_id {
                println!("Bangumi subject: {subject_id}");
            }
            if let Some(media_id) = anime.anime.anilist_media_id {
                println!("AniList media: {media_id}");
            }
            if let Some(route) = &anime.anime.anime_schedule_route {
                println!("AnimeSchedule route: {route}");
            }
            if let Some((local_origin, bangumi_origin)) = anime
                .anime
                .local_episode_origin
                .zip(anime.anime.bangumi_episode_origin)
            {
                println!("episode mapping: search EP{local_origin} <-> Bangumi EP{bangumi_origin}");
            }
            if let Some(synced_at) = anime.anime.schedule_sync_at {
                println!("schedule synced at: {}", synced_at.to_rfc3339());
            }
            if let Some(error) = &anime.anime.schedule_sync_error {
                println!("schedule sync error: {error}");
            }
            if let Some(source) = &anime.anime.schedule_source {
                println!("schedule source: {source}");
            }
            if let Some(confidence) = &anime.anime.schedule_confidence {
                println!("schedule confidence: {confidence}");
            }
            if let Some(warning) = &anime.anime.schedule_warning {
                println!("schedule warning: {warning}");
            }
            println!(
                "duration: {}-{} seconds",
                anime.anime.duration_min_sec, anime.anime.duration_max_sec
            );
            if let Some(episode) = episode {
                println!("active episode: {} ({})", episode.episode_no, episode.state);
                println!(
                    "expected at: {}",
                    episode
                        .expected_at
                        .map(|value| value.to_rfc3339())
                        .unwrap_or_else(|| "unknown".into())
                );
                println!("next check: {}", episode.next_check_at.to_rfc3339());
            }
            Ok(())
        }
        AnimeCommand::Edit { anime_id, title } => {
            let previous = application.rename_anime(anime_id, &title).await?;
            println!(
                "renamed anime {anime_id} from {:?} to {:?}; the previous title remains an alias and an immediate check was scheduled",
                previous.title,
                title.trim()
            );
            Ok(())
        }
        AnimeCommand::Enable { anime_id } => {
            application.set_anime_enabled(anime_id, true).await?;
            println!("enabled anime {anime_id}");
            Ok(())
        }
        AnimeCommand::Disable { anime_id } => {
            application.set_anime_enabled(anime_id, false).await?;
            println!("disabled anime {anime_id}");
            Ok(())
        }
        AnimeCommand::Remove { anime_id, yes } => {
            if !yes {
                let anime = repository.get_anime(anime_id).await?;
                return Err(AppError::InvalidInput(format!(
                    "refusing to permanently delete anime {anime_id} ({:?}); re-run `anime remove {anime_id} --yes` after verifying the ID",
                    anime.anime.title
                )));
            }
            let anime = application.delete_anime(anime_id).await?;
            println!(
                "removed anime {} ({:?}) and all related records",
                anime.id, anime.title
            );
            Ok(())
        }
        AnimeCommand::RepairEpisode {
            anime_id,
            episode,
            yes,
        } => {
            let current = repository.active_episode(anime_id).await?;
            if !yes {
                return Err(AppError::InvalidInput(format!(
                    "refusing to rewind anime {anime_id} from EP{} to EP{episode}; disable it first, then re-run `anime repair-episode {anime_id} --episode {episode} --yes` after verifying both episode numbers",
                    current.episode_no
                )));
            }
            let repaired = application
                .repair_current_episode(anime_id, current.id, episode)
                .await?;
            println!(
                "repaired anime {anime_id} to EP{}; expired {} candidate(s), removed {} notification row(s), removed {} later episode(s); an immediate check is scheduled",
                repaired.episode_no,
                repaired.expired_candidates,
                repaired.removed_notifications,
                repaired.removed_future_episodes
            );
            Ok(())
        }
        AnimeCommand::Sync { anime_id } => {
            ScheduleSynchronizer::new(repository.clone(), config.schedule.clone())?
                .sync_now(anime_id)
                .await?;
            println!("synchronized schedule for anime {anime_id}");
            Ok(())
        }
    }
}

async fn handle_candidate(
    command: CandidateCommand,
    application: &ApplicationService,
    repository: &Repository,
) -> Result<()> {
    match command {
        CandidateCommand::List { state, explain } => {
            let state = if state.eq_ignore_ascii_case("all") {
                None
            } else {
                Some(state.as_str())
            };
            println!("BVID\tANIME\tEP\tMID\tUPLOADER\tDURATION\tSCORE\tSTATE\tSEEN\tTITLE");
            for candidate in repository.list_candidates(state).await? {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    candidate.bvid,
                    candidate.anime_title,
                    candidate.episode_no,
                    candidate.uploader_mid,
                    candidate.uploader_name,
                    candidate.duration_sec,
                    candidate.score,
                    candidate.state,
                    candidate.seen_count,
                    candidate.title
                );
                if explain {
                    println!("  {}", candidate.evaluation_json);
                }
            }
            Ok(())
        }
        CandidateCommand::Accept { bvid } => {
            application.accept_candidate(&bvid).await?;
            println!("accepted {bvid}; notification is pending");
            Ok(())
        }
        CandidateCommand::AcceptUrl { anime_id, url } => {
            let bvid = application.accept_bilibili_url(anime_id, &url).await?;
            println!(
                "accepted {bvid} for anime {anime_id} from Bilibili URL; notification is pending"
            );
            Ok(())
        }
        CandidateCommand::Reject { bvid } => {
            application.reject_candidate(&bvid).await?;
            println!("rejected {bvid}");
            Ok(())
        }
        CandidateCommand::RejectAll { anime_id, yes } => {
            let episode = repository.active_episode(anime_id).await?;
            let candidates = repository.active_candidates(episode.id).await?;
            if !yes {
                return Err(AppError::InvalidInput(format!(
                    "refusing to reject {} pending candidate(s) for anime {anime_id} EP{}; re-run with --yes",
                    candidates.len(),
                    episode.episode_no
                )));
            }
            let (_, rejected) = application.reject_all_candidates(anime_id).await?;
            println!(
                "rejected {rejected} pending candidate(s) for anime {anime_id} EP{}",
                episode.episode_no
            );
            Ok(())
        }
    }
}

async fn handle_uploader(command: UploaderCommand, application: &ApplicationService) -> Result<()> {
    match command {
        UploaderCommand::Trust { anime_id, mid } => {
            application
                .set_uploader_flag(anime_id, mid, true, false)
                .await?;
            println!("trusted uploader mid={mid} for anime={anime_id}");
        }
        UploaderCommand::Block { anime_id, mid } => {
            application
                .set_uploader_flag(anime_id, mid, false, true)
                .await?;
            println!("blocked uploader mid={mid} for anime={anime_id}");
        }
    }
    Ok(())
}

fn parse_duration_arg(value: &str) -> Result<i64> {
    let value = value.trim();
    if let Some(minutes) = value.strip_suffix('m') {
        return minutes
            .parse::<i64>()
            .map(|minutes| minutes * 60)
            .map_err(|_| AppError::InvalidInput(format!("invalid duration: {value}")));
    }
    if let Some(seconds) = value.strip_suffix('s') {
        return seconds
            .parse::<i64>()
            .map_err(|_| AppError::InvalidInput(format!("invalid duration: {value}")));
    }
    if let Some((minutes, seconds)) = value.split_once(':') {
        return Ok(minutes
            .parse::<i64>()
            .map_err(|_| AppError::InvalidInput(format!("invalid duration: {value}")))?
            * 60
            + seconds
                .parse::<i64>()
                .map_err(|_| AppError::InvalidInput(format!("invalid duration: {value}")))?);
    }
    value
        .parse()
        .map_err(|_| AppError::InvalidInput(format!("invalid duration: {value}")))
}

fn parse_weekday(value: &str) -> Result<Weekday> {
    match value.to_ascii_lowercase().as_str() {
        "monday" | "mon" => Ok(Weekday::Mon),
        "tuesday" | "tue" => Ok(Weekday::Tue),
        "wednesday" | "wed" => Ok(Weekday::Wed),
        "thursday" | "thu" => Ok(Weekday::Thu),
        "friday" | "fri" => Ok(Weekday::Fri),
        "saturday" | "sat" => Ok(Weekday::Sat),
        "sunday" | "sun" => Ok(Weekday::Sun),
        _ => Err(AppError::InvalidInput(format!("invalid weekday: {value}"))),
    }
}

fn parse_time(value: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(value, "%H:%M")
        .map_err(|e| AppError::InvalidInput(format!("invalid time {value}: {e}")))
}

fn next_weekly_occurrence(
    timezone: &str,
    weekday: Weekday,
    time: NaiveTime,
) -> Result<chrono::DateTime<Utc>> {
    let timezone = Tz::from_str(timezone)
        .map_err(|_| AppError::InvalidInput(format!("invalid timezone: {timezone}")))?;
    let now = Utc::now().with_timezone(&timezone);
    for offset in 0..=7 {
        let date = now.date_naive() + chrono::Duration::days(offset);
        if date.weekday() != weekday {
            continue;
        }
        let local = date.and_time(time);
        if let Some(candidate) = timezone.from_local_datetime(&local).earliest()
            && candidate > now
        {
            return Ok(candidate.with_timezone(&Utc));
        }
    }
    Err(AppError::InvalidInput(
        "cannot calculate the next expected release time".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cli_duration_formats() {
        assert_eq!(parse_duration_arg("20m").unwrap(), 1_200);
        assert_eq!(parse_duration_arg("23:40").unwrap(), 1_420);
        assert_eq!(parse_duration_arg("90s").unwrap(), 90);
    }

    #[test]
    fn anime_remove_requires_explicit_confirmation_flag() {
        let cli = Cli::try_parse_from(["anipulse", "anime", "remove", "4"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Anime {
                command: AnimeCommand::Remove {
                    anime_id: 4,
                    yes: false
                }
            }
        ));

        let cli = Cli::try_parse_from(["anipulse", "anime", "remove", "4", "--yes"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Anime {
                command: AnimeCommand::Remove {
                    anime_id: 4,
                    yes: true
                }
            }
        ));
    }

    #[test]
    fn parses_anime_edit_title() {
        let cli = Cli::try_parse_from([
            "anipulse",
            "anime",
            "edit",
            "4",
            "--title",
            "无职转生 第三季",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Anime {
                command: AnimeCommand::Edit {
                    anime_id: 4,
                    ref title
                }
            } if title == "无职转生 第三季"
        ));
    }

    #[test]
    fn parses_optional_episode_number_mapping_as_a_pair() {
        let cli = Cli::try_parse_from([
            "anipulse",
            "anime",
            "add",
            "--title",
            "Re：从零开始的异世界生活 第四季",
            "--next-episode",
            "14",
            "--auto-schedule",
            "--bangumi-id",
            "633836",
            "--search-episode-start",
            "12",
            "--bangumi-episode-start",
            "78",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Anime {
                command: AnimeCommand::Add(ref args)
            } if args.search_episode_start == Some(12)
                && args.bangumi_episode_start == Some(78)
        ));

        assert!(
            Cli::try_parse_from([
                "anipulse",
                "anime",
                "add",
                "--title",
                "Re：从零开始的异世界生活 第四季",
                "--auto-schedule",
                "--search-episode-start",
                "12",
            ])
            .is_err()
        );
    }

    #[test]
    fn episode_repair_requires_explicit_confirmation_flag() {
        let cli =
            Cli::try_parse_from(["anipulse", "anime", "repair-episode", "1", "--episode", "9"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Command::Anime {
                command: AnimeCommand::RepairEpisode {
                    anime_id: 1,
                    episode: 9,
                    yes: false
                }
            }
        ));

        let cli = Cli::try_parse_from([
            "anipulse",
            "anime",
            "repair-episode",
            "1",
            "--episode",
            "9",
            "--yes",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Anime {
                command: AnimeCommand::RepairEpisode {
                    anime_id: 1,
                    episode: 9,
                    yes: true
                }
            }
        ));
    }

    #[test]
    fn candidate_reject_all_requires_confirmation_flag() {
        let cli = Cli::try_parse_from(["anipulse", "candidate", "reject-all", "4"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Candidate {
                command: CandidateCommand::RejectAll {
                    anime_id: 4,
                    yes: false
                }
            }
        ));
    }
}
