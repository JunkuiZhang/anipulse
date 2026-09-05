use std::sync::LazyLock;

use regex::Regex;

use crate::{detector::title::normalize_title, domain::EpisodeMatch};

static RANGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:ep(?:isode)?|e)?\s*0*(\d{1,4})\s*[-~至到]\s*(?:ep(?:isode)?|e)?\s*0*(\d{1,4})",
    )
    .expect("valid regex")
});
static EXPLICIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:ep(?:isode)?|e)\s*0*(\d{1,3})\b").expect("valid regex"));
static CHINESE_ARABIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"第\s*0*(\d{1,3})\s*[集话話]").expect("valid regex"));
static CHINESE_NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"第\s*([零〇一二两三四五六七八九十百]+)\s*[集话話]").expect("valid regex")
});
static COLLECTION_TOTAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:全\s*0*\d{1,4}\s*[集话話]|全\s*[零〇一二两三四五六七八九十百]+\s*[集话話]|(?:第\s*)?0*\d{1,4}\s*[-~至到]\s*0*\d{1,4}\s*[集话話]|0*\d{1,4}\s*[集话話]\s*全(?:\s|$))",
    )
    .expect("valid regex")
});
static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").expect("valid regex"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpisodeEvidence {
    pub kind: EpisodeMatch,
    pub explicit_other: bool,
}

pub fn match_episode(title: &str, target: i64) -> EpisodeEvidence {
    let normalized = normalize_title(title);

    if has_collection_signal_normalized(&normalized) {
        return evidence(EpisodeMatch::Ambiguous, false);
    }

    for captures in RANGE.captures_iter(&normalized) {
        let first = captures[1].parse::<i64>().unwrap_or_default();
        let second = captures[2].parse::<i64>().unwrap_or_default();
        if first <= 200 && second <= 200 && (first == target || second == target) {
            return evidence(EpisodeMatch::Ambiguous, false);
        }
    }
    if normalized.contains(&format!("{target}.")) || normalized.contains(&format!(".{target}")) {
        return evidence(EpisodeMatch::Ambiguous, false);
    }

    let explicit: Vec<i64> = EXPLICIT
        .captures_iter(&normalized)
        .filter_map(|capture| capture[1].parse().ok())
        .collect();
    if !explicit.is_empty() {
        return if explicit.contains(&target) {
            evidence(EpisodeMatch::Strong, false)
        } else {
            evidence(EpisodeMatch::None, true)
        };
    }

    let chinese_arabic: Vec<i64> = CHINESE_ARABIC
        .captures_iter(&normalized)
        .filter_map(|capture| capture[1].parse().ok())
        .collect();
    if !chinese_arabic.is_empty() {
        return if chinese_arabic.contains(&target) {
            evidence(EpisodeMatch::Strong, false)
        } else {
            evidence(EpisodeMatch::None, true)
        };
    }

    let chinese_numbers: Vec<i64> = CHINESE_NUMBER
        .captures_iter(&normalized)
        .filter_map(|capture| parse_chinese_number(&capture[1]))
        .collect();
    if !chinese_numbers.is_empty() {
        return if chinese_numbers.contains(&target) {
            evidence(EpisodeMatch::Strong, false)
        } else {
            evidence(EpisodeMatch::None, true)
        };
    }

    for found in NUMBER.find_iter(&normalized) {
        let token = found.as_str();
        if token.parse::<i64>().ok() != Some(target) {
            continue;
        }
        let prefix = &normalized[..found.start()];
        let suffix = &normalized[found.end()..];
        let previous = prefix.chars().next_back();
        let next = suffix.chars().next();
        if matches!(next, Some('月' | '年' | 'p')) {
            continue;
        }
        if previous == Some('-')
            && NUMBER
                .find_iter(prefix.trim_end_matches('-'))
                .last()
                .and_then(|number| number.as_str().parse::<i64>().ok())
                .is_some_and(|number| number >= 1_900)
        {
            continue;
        }
        return evidence(
            if token.len() >= 2 && token.starts_with('0') {
                EpisodeMatch::Strong
            } else {
                EpisodeMatch::Weak
            },
            false,
        );
    }
    evidence(EpisodeMatch::None, false)
}

pub fn has_collection_signal(title: &str) -> bool {
    has_collection_signal_normalized(&normalize_title(title))
}

fn has_collection_signal_normalized(normalized: &str) -> bool {
    normalized.contains("合集")
        || normalized.contains("全集")
        || COLLECTION_TOTAL.is_match(normalized)
}

fn evidence(kind: EpisodeMatch, explicit_other: bool) -> EpisodeEvidence {
    EpisodeEvidence {
        kind,
        explicit_other,
    }
}

fn parse_chinese_number(value: &str) -> Option<i64> {
    let digit = |character| match character {
        '零' | '〇' => Some(0),
        '一' => Some(1),
        '二' | '两' => Some(2),
        '三' => Some(3),
        '四' => Some(4),
        '五' => Some(5),
        '六' => Some(6),
        '七' => Some(7),
        '八' => Some(8),
        '九' => Some(9),
        _ => None,
    };
    if value == "百" {
        return Some(100);
    }
    if let Some((left, right)) = value.split_once('十') {
        let tens = if left.is_empty() {
            1
        } else {
            digit(left.chars().next()?)?
        };
        let ones = if right.is_empty() {
            0
        } else {
            digit(right.chars().next()?)?
        };
        return Some(tens * 10 + ones);
    }
    if value.chars().count() == 1 {
        return digit(value.chars().next()?);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_supported_episode_forms() {
        for title in [
            "Silent Witch EP8",
            "Silent Witch EP08",
            "Silent Witch E08",
            "Silent Witch Episode 8",
            "沉默魔女 第8集",
            "沉默魔女 第08集",
            "沉默魔女 第八集",
            "Silent Witch 08",
        ] {
            assert!(
                matches!(
                    match_episode(title, 8).kind,
                    EpisodeMatch::Strong | EpisodeMatch::Weak
                ),
                "{title}"
            );
        }
    }

    #[test]
    fn rejects_numbers_with_other_meanings() {
        for title in [
            "Silent Witch 1080P",
            "Silent Witch EP18",
            "Silent Witch 80",
            "Silent Witch 2026-08",
            "Silent Witch 8月",
        ] {
            assert_eq!(match_episode(title, 8).kind, EpisodeMatch::None, "{title}");
        }
    }

    #[test]
    fn marks_special_episodes_ambiguous() {
        assert_eq!(
            match_episode("Silent Witch EP8-9", 8).kind,
            EpisodeMatch::Ambiguous
        );
        assert_eq!(
            match_episode("Silent Witch EP8.5", 8).kind,
            EpisodeMatch::Ambiguous
        );
    }

    #[test]
    fn collection_totals_are_not_single_episode_evidence() {
        for title in [
            "落第贤者的学院无双 全12话 4k超清无删减完整版",
            "攻壳机动队 全10话 周更",
            "某动画 第1-12集",
            "某动画 12集全",
            "某动画 全十二話",
        ] {
            assert!(has_collection_signal(title), "{title}");
            assert_eq!(
                match_episode(title, 12).kind,
                EpisodeMatch::Ambiguous,
                "{title}"
            );
        }

        assert!(!has_collection_signal("某动画 第12集 全程高能"));
        assert_eq!(
            match_episode("某动画 第12集 全程高能", 12).kind,
            EpisodeMatch::Strong
        );
    }
}
