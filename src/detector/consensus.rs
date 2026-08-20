use std::collections::HashMap;

use crate::domain::{AnimeMatch, DurationMatch, EpisodeMatch, Evaluation, StoredCandidate};

pub fn find_consensus(
    candidates: &[StoredCandidate],
    minimum_score: i32,
    required_uploaders: usize,
    max_duration_delta: i64,
    max_publish_delta: i64,
) -> Option<(String, usize)> {
    let mut per_uploader: HashMap<i64, (&StoredCandidate, Evaluation)> = HashMap::new();
    for candidate in candidates {
        let Ok(evaluation) = serde_json::from_str::<Evaluation>(&candidate.evaluation_json) else {
            continue;
        };
        if candidate.score < i64::from(minimum_score)
            || evaluation.hard_reject
            || evaluation.episode_match != EpisodeMatch::Strong
            || !matches!(
                evaluation.anime_match,
                AnimeMatch::Exact | AnimeMatch::Strong
            )
            || !evaluation.negative_keywords.is_empty()
            || !matches!(
                evaluation.duration_match,
                DurationMatch::Normal | DurationMatch::Acceptable
            )
        {
            continue;
        }
        let replace = per_uploader
            .get(&candidate.uploader_mid)
            .is_none_or(|(existing, _)| existing.score < candidate.score);
        if replace {
            per_uploader.insert(candidate.uploader_mid, (candidate, evaluation));
        }
    }
    let unique: Vec<_> = per_uploader.into_values().collect();
    for (anchor, _) in &unique {
        let compatible: Vec<_> = unique
            .iter()
            .filter(|(candidate, _)| {
                (candidate.duration_sec - anchor.duration_sec).abs() <= max_duration_delta
                    && (candidate.published_at - anchor.published_at)
                        .num_seconds()
                        .abs()
                        <= max_publish_delta
            })
            .collect();
        if compatible.len() >= required_uploaders {
            let best = compatible
                .into_iter()
                .map(|(candidate, _)| *candidate)
                .max_by_key(|candidate| candidate.score)?;
            return Some((best.bvid.clone(), unique.len().min(required_uploaders)));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::domain::{AnimeMatch, DurationMatch, EpisodeMatch};

    fn candidate(bvid: &str, mid: i64) -> StoredCandidate {
        let now = Utc::now();
        let evaluation = Evaluation {
            anime_match: AnimeMatch::Strong,
            episode_match: EpisodeMatch::Strong,
            duration_match: DurationMatch::Normal,
            expected_time_delta_sec: Some(0),
            trusted_uploader: false,
            blocked_uploader: false,
            negative_keywords: vec![],
            metadata_enriched: true,
            score: 75,
            hard_reject: false,
            manual_review: false,
            reasons: vec![],
        };
        StoredCandidate {
            id: mid,
            episode_id: 1,
            bvid: bvid.into(),
            uploader_mid: mid,
            uploader_name: format!("up{mid}"),
            title: "Silent Witch EP08".into(),
            description: None,
            duration_sec: 1_420,
            published_at: now,
            url: "https://example.com".into(),
            tags_json: "[]".into(),
            page_count: Some(1),
            score: 75,
            state: "pending".into(),
            first_seen_at: now,
            last_seen_at: now,
            seen_count: 1,
            evaluation_json: serde_json::to_string(&evaluation).unwrap(),
        }
    }

    #[test]
    fn repeated_videos_from_same_uploader_are_one_vote() {
        let candidates = vec![candidate("BV1", 100), candidate("BV2", 100)];
        assert!(find_consensus(&candidates, 60, 2, 180, 7_200).is_none());
    }

    #[test]
    fn distinct_uploaders_form_consensus() {
        let candidates = vec![candidate("BV1", 100), candidate("BV2", 200)];
        assert!(find_consensus(&candidates, 60, 2, 180, 7_200).is_some());
    }
}
