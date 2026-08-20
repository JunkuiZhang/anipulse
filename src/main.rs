use std::{path::PathBuf, str::FromStr, sync::Arc};

use anipulse::{
    config::AppConfig,
    detector::Detector,
    domain::NewAnime,
    error::{AppError, Result},
    notification::NotificationDispatcher,
    provider::BilibiliProvider,
    repository::Repository,
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
    Add(AddAnimeArgs),
    List,
    Show { anime_id: i64 },
    Enable { anime_id: i64 },
    Disable { anime_id: i64 },
}

#[derive(Debug, Args)]
struct AddAnimeArgs {
    #[arg(long)]
    title: String,
    #[arg(long = "alias")]
    aliases: Vec<String>,
    #[arg(long, default_value_t = 1)]
    next_episode: i64,
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
    Reject {
        bvid: String,
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
        Command::Anime { command } => handle_anime(command, &repository).await,
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

async fn handle_anime(command: AnimeCommand, repository: &Repository) -> Result<()> {
    match command {
        AnimeCommand::Add(args) => {
            Tz::from_str(&args.timezone).map_err(|_| {
                AppError::InvalidInput(format!("invalid timezone: {}", args.timezone))
            })?;
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
                        .map_err(|e| AppError::InvalidInput(format!("invalid --expected-at: {e}")))?
                        .with_timezone(&Utc),
                )
            } else if let (Some(weekday), Some(time)) = (expected_weekday, expected_time.as_deref())
            {
                Some(next_weekly_occurrence(
                    &args.timezone,
                    weekday,
                    parse_time(time)?,
                )?)
            } else {
                None
            };
            let id = repository
                .add_anime(NewAnime {
                    title: args.title,
                    aliases: args.aliases,
                    next_episode: args.next_episode,
                    expected_at,
                    expected_weekday: expected_weekday
                        .map(|day| i64::from(day.num_days_from_monday())),
                    expected_time,
                    timezone: args.timezone,
                    duration_min_sec: parse_duration_arg(&args.duration_min)?,
                    duration_max_sec: parse_duration_arg(&args.duration_max)?,
                })
                .await?;
            println!("added anime id={id}");
            Ok(())
        }
        AnimeCommand::List => {
            println!("ID\tENABLED\tTITLE\tDURATION");
            for anime in repository.list_anime().await? {
                println!(
                    "{}\t{}\t{}\t{}-{}m",
                    anime.id,
                    anime.enabled,
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
        CandidateCommand::Reject { bvid } => {
            repository.reject_candidate(&bvid, true).await?;
            println!("rejected {bvid}");
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
}
