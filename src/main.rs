use std::{path::PathBuf, str::FromStr, sync::Arc};

use anipulse::{
    config::AppConfig,
    detector::Detector,
    domain::{AutoScheduleMetadata, CandidateState, NewAnime, VideoCandidate},
    error::{AppError, Result},
    notification::NotificationDispatcher,
    provider::{BilibiliProvider, VideoSearchProvider},
    repository::Repository,
    schedule::{ScheduleProvider, ScheduleSynchronizer},
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
    match cli.command {
        Command::Anime { command } => handle_anime(command, &repository, &config).await,
        Command::Candidate { command } => handle_candidate(command, &repository, &config).await,
        Command::Uploader { command } => handle_uploader(command, &repository).await,
        Command::Notification {
            command: NotificationCommand::Test,
        } => {
            NotificationDispatcher::new(repository, &config.notification)?
                .test()
                .await
        }
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
    }
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
    let dispatcher = NotificationDispatcher::new(repository, &config.notification)?;
    Ok((detector, dispatcher))
}

async fn handle_anime(
    command: AnimeCommand,
    repository: &Repository,
    config: &AppConfig,
) -> Result<()> {
    match command {
        AnimeCommand::Add(args) => {
            Tz::from_str(&args.timezone).map_err(|_| {
                AppError::InvalidInput(format!("invalid timezone: {}", args.timezone))
            })?;
            let mut aliases = args.aliases;
            let (expected_weekday, expected_time, expected_at, auto_schedule) = if args
                .auto_schedule
            {
                let provider = ScheduleProvider::new(config.schedule.clone())?;
                let catalog = provider.load_catalog().await?;
                let resolved = provider
                    .resolve(
                        &catalog,
                        &args.title,
                        args.bangumi_id,
                        args.next_episode,
                        &args.timezone,
                    )
                    .await?;
                aliases.extend(resolved.aliases.iter().cloned());
                println!(
                    "matched Bangumi subject #{}: {}; EP{} expected at {}",
                    resolved.bangumi_subject_id,
                    resolved.matched_title,
                    args.next_episode,
                    resolved.expected_at.to_rfc3339()
                );
                (
                    Some(resolved.expected_weekday),
                    Some(resolved.expected_time.clone()),
                    Some(resolved.expected_at),
                    Some(AutoScheduleMetadata {
                        bangumi_subject_id: resolved.bangumi_subject_id,
                        broadcast_pattern: resolved.broadcast_pattern,
                        next_sync_at: Utc::now()
                            + chrono::Duration::seconds(config.schedule.sync_interval_secs as i64),
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
            if let Some(synced_at) = anime.anime.schedule_sync_at {
                println!("schedule synced at: {}", synced_at.to_rfc3339());
            }
            if let Some(error) = &anime.anime.schedule_sync_error {
                println!("schedule sync error: {error}");
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
            let previous = repository.rename_anime(anime_id, &title).await?;
            println!(
                "renamed anime {anime_id} from {:?} to {:?}; the previous title remains an alias and an immediate check was scheduled",
                previous.title,
                title.trim()
            );
            Ok(())
        }
        AnimeCommand::Enable { anime_id } => {
            repository.set_anime_enabled(anime_id, true).await?;
            println!("enabled anime {anime_id}");
            Ok(())
        }
        AnimeCommand::Disable { anime_id } => {
            repository.set_anime_enabled(anime_id, false).await?;
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
            let anime = repository.delete_anime(anime_id).await?;
            println!(
                "removed anime {} ({:?}) and all related records",
                anime.id, anime.title
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
    repository: &Repository,
    config: &AppConfig,
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
            let (candidate, _) = repository.candidate_context(&bvid).await?;
            repository
                .confirm_candidate(
                    candidate.episode_id,
                    &bvid,
                    "manual_confirmation",
                    &config.notification.channel,
                    true,
                )
                .await?;
            println!("accepted {bvid}; notification is pending");
            Ok(())
        }
        CandidateCommand::AcceptUrl { anime_id, url } => {
            let bvid = parse_bilibili_bvid(&url)?;
            let anime = repository.get_anime(anime_id).await?;
            let episode = repository.active_episode(anime_id).await?;
            let now = Utc::now();
            let seed = VideoCandidate {
                bvid: bvid.clone(),
                title: bvid.clone(),
                description: None,
                uploader_mid: 0,
                uploader_name: "unknown".into(),
                duration_sec: 0,
                published_at: now,
                url: format!("https://www.bilibili.com/video/{bvid}"),
                tags: Vec::new(),
                page_count: None,
                discovered_at: now,
                enriched: false,
            };
            let provider = BilibiliProvider::new(config.bilibili.clone(), repository.clone())?;
            let candidate = provider.enrich(&seed).await?;
            let trust = repository
                .uploader_trust(anime_id, candidate.uploader_mid)
                .await?;
            let evaluation = anipulse::detector::evaluator::evaluate(
                &anime,
                &episode,
                &candidate,
                &trust,
                config.confirmation.trusted_confirmed_count,
            );
            repository
                .upsert_candidate(episode.id, &candidate, &evaluation, CandidateState::Pending)
                .await?;
            repository
                .confirm_candidate(
                    episode.id,
                    &bvid,
                    "manual_url_confirmation",
                    &config.notification.channel,
                    true,
                )
                .await?;
            println!(
                "accepted {bvid} for anime {anime_id} EP{} from Bilibili URL; notification is pending",
                episode.episode_no
            );
            Ok(())
        }
        CandidateCommand::Reject { bvid } => {
            repository.reject_candidate(&bvid, true).await?;
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
            let rejected = repository.reject_all_candidates(episode.id, true).await?;
            println!(
                "rejected {rejected} pending candidate(s) for anime {anime_id} EP{}",
                episode.episode_no
            );
            Ok(())
        }
    }
}

async fn handle_uploader(command: UploaderCommand, repository: &Repository) -> Result<()> {
    match command {
        UploaderCommand::Trust { anime_id, mid } => {
            repository
                .set_uploader_flag(anime_id, mid, true, false)
                .await?;
            println!("trusted uploader mid={mid} for anime={anime_id}");
        }
        UploaderCommand::Block { anime_id, mid } => {
            repository
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

fn parse_bilibili_bvid(value: &str) -> Result<String> {
    let value = value.trim();
    if valid_bvid(value) {
        return Ok(value.to_string());
    }

    let url = url::Url::parse(value)
        .map_err(|_| AppError::InvalidInput("expected a BV ID or Bilibili video URL".into()))?;
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("www.bilibili.com" | "m.bilibili.com"))
    {
        return Err(AppError::InvalidInput(
            "only canonical HTTPS Bilibili video URLs are accepted".into(),
        ));
    }
    let mut segments = url.path_segments().into_iter().flatten();
    if segments.next() != Some("video") {
        return Err(AppError::InvalidInput(
            "Bilibili URL must use /video/BV...".into(),
        ));
    }
    let bvid = segments.next().unwrap_or_default();
    if !valid_bvid(bvid) {
        return Err(AppError::InvalidInput(
            "Bilibili URL contains an invalid BV ID".into(),
        ));
    }
    Ok(bvid.to_string())
}

fn valid_bvid(value: &str) -> bool {
    value.len() == 12
        && value.starts_with("BV")
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
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
    fn parses_canonical_bilibili_video_input() {
        assert_eq!(
            parse_bilibili_bvid("https://www.bilibili.com/video/BV1Es8A6UEnr?p=1").unwrap(),
            "BV1Es8A6UEnr"
        );
        assert_eq!(parse_bilibili_bvid("BV1Es8A6UEnr").unwrap(), "BV1Es8A6UEnr");
        assert!(parse_bilibili_bvid("https://example.com/video/BV1Es8A6UEnr").is_err());
        assert!(parse_bilibili_bvid("https://www.bilibili.com/bangumi/BV1Es8A6UEnr").is_err());
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
