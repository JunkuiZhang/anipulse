use crate::{
    detector::{episode::match_episode, title::match_anime},
    domain::{
        AnimeMatch, AnimeWithAliases, DurationMatch, Episode, EpisodeMatch, Evaluation,
        UploaderTrust, VideoCandidate,
    },
};

pub fn evaluate(
    anime: &AnimeWithAliases,
    episode: &Episode,
    candidate: &VideoCandidate,
    trust: &UploaderTrust,
    trusted_confirmed_count: i64,
    blocked_keywords: &[String],
) -> Evaluation {
    let mut score = 0;
    let mut hard_reject = false;
    let mut manual_review = false;
    let mut reasons = Vec::new();

    let anime_match = match_anime(&candidate.title, &anime.aliases);
    match anime_match {
        AnimeMatch::Exact => score += 25,
        AnimeMatch::Strong => score += 20,
        AnimeMatch::Weak => score += 10,
        AnimeMatch::None => {
            score -= 60;
            hard_reject = true;
            reasons.push("no configured anime alias matched".into());
        }
    }

    let episode_evidence = match_episode(&candidate.title, episode.episode_no);
    match episode_evidence.kind {
        EpisodeMatch::Strong => score += 30,
        EpisodeMatch::Weak => score += 15,
        EpisodeMatch::Ambiguous => {
            score -= 20;
            manual_review = true;
            reasons.push("multi-part or special episode notation requires review".into());
        }
        EpisodeMatch::None => {
            score -= 40;
            hard_reject = true;
            reasons.push(if episode_evidence.explicit_other {
                "title explicitly names another episode".into()
            } else {
                "target episode is not identifiable in title".into()
            });
        }
    }

    let minimum = anime.anime.duration_min_sec;
    let maximum = anime.anime.duration_max_sec;
    let hard_minimum = minimum * 60 / 100;
    let duration_match = if candidate.duration_sec == 0 && !candidate.enriched {
        score -= 10;
        reasons.push("search result has no duration; detail metadata is required".into());
        DurationMatch::Suspicious
    } else if candidate.duration_sec < hard_minimum {
        score -= 60;
        hard_reject = true;
        reasons.push("video is below the hard minimum duration".into());
        DurationMatch::TooShort
    } else if candidate.duration_sec >= minimum && candidate.duration_sec <= maximum {
        score += 15;
        DurationMatch::Normal
    } else if candidate.duration_sec >= minimum * 80 / 100
        && candidate.duration_sec <= maximum * 120 / 100
    {
        score += 5;
        DurationMatch::Acceptable
    } else {
        score -= 30;
        reasons.push("video duration is suspicious".into());
        DurationMatch::Suspicious
    };

    let normalized = crate::detector::title::normalize_title(&candidate.title);
    let mut negative_keywords = find_negative_keywords(&normalized);
    for keyword in &negative_keywords {
        let strong = matches!(
            keyword.as_str(),
            "预告"
                | "pv"
                | "reaction"
                | "mad"
                | "amv"
                | "名场面"
                | "剪辑"
                | "速看"
                | "一口气"
                | "预测"
        );
        score -= if strong { 50 } else { 40 };
        if strong {
            hard_reject = true;
        }
    }
    if !negative_keywords.is_empty() {
        reasons.push(format!(
            "negative title signals: {}",
            negative_keywords.join(", ")
        ));
    }
    let configured_haystack = crate::detector::title::normalize_title(&format!(
        "{} {} {}",
        candidate.title,
        candidate.description.as_deref().unwrap_or_default(),
        candidate.tags.join(" ")
    ));
    let mut configured_hits = blocked_keywords
        .iter()
        .filter(|keyword| !keyword.is_empty() && configured_haystack.contains(keyword.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    configured_hits.sort();
    configured_hits.dedup();
    if !configured_hits.is_empty() {
        score -= 100;
        hard_reject = true;
        for keyword in &configured_hits {
            if !negative_keywords.contains(keyword) {
                negative_keywords.push(keyword.clone());
            }
        }
        reasons.push(format!(
            "matched configured blocked keywords: {}",
            configured_hits.join(", ")
        ));
    }

    let expected_time_delta_sec = episode
        .expected_at
        .map(|expected| (candidate.published_at - expected).num_seconds());
    if let Some(delta) = expected_time_delta_sec {
        if (-43_200..=172_800).contains(&delta) {
            score += 10;
        } else if delta < -259_200 {
            score -= 10;
            reasons.push("publication is much earlier than expected".into());
        }
    }

    let trusted_uploader = trust.is_trusted(trusted_confirmed_count);
    if trusted_uploader {
        score += 25;
    }
    if trust.manually_blocked {
        score -= 100;
        hard_reject = true;
        reasons.push("uploader is blocked for this anime".into());
    }
    if candidate.enriched {
        score += 10;
    }

    Evaluation {
        anime_match,
        episode_match: episode_evidence.kind,
        duration_match,
        expected_time_delta_sec,
        trusted_uploader,
        blocked_uploader: trust.manually_blocked,
        negative_keywords,
        metadata_enriched: candidate.enriched,
        score,
        hard_reject,
        manual_review,
        reasons,
    }
}

fn find_negative_keywords(normalized: &str) -> Vec<String> {
    let mut found = Vec::new();
    for keyword in [
        "预告",
        "预热视频",
        "reaction",
        "解说",
        "解析",
        "吐槽",
        "名场面",
        "mad",
        "amv",
        "剪辑",
        "速看",
        "一口气",
        "预测",
    ] {
        if keyword == "解说" && normalized.contains("无解说") {
            continue;
        }
        if normalized.contains(keyword) {
            found.push(keyword.to_string());
        }
    }
    if normalized
        .split_whitespace()
        .any(|token| token.eq_ignore_ascii_case("pv"))
    {
        found.push("pv".into());
    }
    found.sort();
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::domain::{Anime, AnimeWithAliases, Episode};

    fn fixtures(duration: i64, title: &str) -> (AnimeWithAliases, Episode, VideoCandidate) {
        let now = Utc::now();
        (
            AnimeWithAliases {
                anime: Anime {
                    id: 1,
                    title: "Silent Witch".into(),
                    bangumi_subject_id: None,
                    expected_weekday: None,
                    expected_time: None,
                    timezone: "Asia/Shanghai".into(),
                    duration_min_sec: 1_200,
                    duration_max_sec: 1_680,
                    enabled: true,
                    created_at: now,
                    updated_at: now,
                    auto_schedule: false,
                    broadcast_pattern: None,
                    schedule_sync_at: None,
                    schedule_next_sync_at: None,
                    schedule_sync_error: None,
                    local_episode_origin: None,
                    bangumi_episode_origin: None,
                    lifecycle: "tracking".into(),
                    summary: String::new(),
                    total_episodes: None,
                    released_completed_at: None,
                    archived_at: None,
                },
                aliases: vec!["Silent Witch".into(), "沉默魔女".into()],
            },
            Episode {
                id: 1,
                anime_id: 1,
                episode_no: 8,
                expected_at: Some(Utc.timestamp_opt(1_800_000_000, 0).unwrap()),
                state: "watching".into(),
                next_check_at: now,
                first_candidate_at: None,
                confirmed_at: None,
                notified_at: None,
            },
            VideoCandidate {
                bvid: "BVtest".into(),
                title: title.into(),
                description: None,
                uploader_mid: 1,
                uploader_name: "up".into(),
                duration_sec: duration,
                published_at: Utc.timestamp_opt(1_800_000_000, 0).unwrap(),
                url: "https://example.com".into(),
                tags: vec![],
                page_count: Some(1),
                discovered_at: now,
                enriched: true,
            },
        )
    }

    #[test]
    fn duration_rules_are_conservative() {
        for duration in [90, 300] {
            let (anime, episode, candidate) = fixtures(duration, "Silent Witch EP08");
            assert!(
                evaluate(
                    &anime,
                    &episode,
                    &candidate,
                    &UploaderTrust::default(),
                    3,
                    &[],
                )
                .hard_reject
            );
        }
        let (anime, episode, candidate) = fixtures(1_420, "Silent Witch EP08");
        assert_eq!(
            evaluate(
                &anime,
                &episode,
                &candidate,
                &UploaderTrust::default(),
                3,
                &[],
            )
            .duration_match,
            DurationMatch::Normal
        );
        let (anime, episode, candidate) = fixtures(2_700, "Silent Witch EP08");
        assert_eq!(
            evaluate(
                &anime,
                &episode,
                &candidate,
                &UploaderTrust::default(),
                3,
                &[],
            )
            .duration_match,
            DurationMatch::Suspicious
        );
    }

    #[test]
    fn negative_titles_do_not_confirm() {
        for suffix in ["预告", "解说", "Reaction", "名场面"] {
            let title = format!("Silent Witch EP08 {suffix}");
            let (anime, episode, candidate) = fixtures(1_420, &title);
            let evaluation = evaluate(
                &anime,
                &episode,
                &candidate,
                &UploaderTrust::default(),
                3,
                &[],
            );
            assert!(evaluation.score < 60, "{suffix}: {}", evaluation.score);
        }
    }

    #[test]
    fn accepts_real_nyanko_episode_eight_samples() {
        for (title, duration) in [
            ("『尼古喵喵』08（无删减版）【中文字幕】", 1_688),
            ("【尼古喵喵】第8集（未删减版）", 1_948),
            ("尼古喵喵 第8集（未删减版）", 2_028),
            ("『尼古喵喵』第8话", 2_058),
        ] {
            let (mut anime, episode, candidate) = fixtures(duration, title);
            anime.anime.title = "尼古喵喵".into();
            anime.aliases = vec!["尼古喵喵".into(), "ヤニねこ".into()];
            anime.anime.duration_min_sec = 1_500;
            anime.anime.duration_max_sec = 2_100;

            let evaluation = evaluate(
                &anime,
                &episode,
                &candidate,
                &UploaderTrust::default(),
                3,
                &[],
            );
            assert!(!evaluation.hard_reject, "{title}: {:?}", evaluation.reasons);
            assert_eq!(evaluation.episode_match, EpisodeMatch::Strong, "{title}");
            assert_eq!(evaluation.duration_match, DurationMatch::Normal, "{title}");
            assert!(evaluation.score >= 60, "{title}: {}", evaluation.score);
        }
    }

    #[test]
    fn configured_blocked_keyword_is_a_hard_reject() {
        let (anime, episode, mut candidate) = fixtures(1_420, "Silent Witch EP08");
        candidate.description = Some("有声漫画完整版".into());
        let evaluation = evaluate(
            &anime,
            &episode,
            &candidate,
            &UploaderTrust::default(),
            3,
            &["有声漫画".into()],
        );

        assert!(evaluation.hard_reject);
        assert!(
            evaluation
                .negative_keywords
                .iter()
                .any(|keyword| keyword == "有声漫画")
        );
    }
}
