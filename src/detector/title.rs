use std::sync::LazyLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

use crate::domain::AnimeMatch;

static HTML_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").expect("valid regex"));

pub fn normalize_title(title: &str) -> String {
    let without_tags = HTML_TAG.replace_all(title, " ");
    let decoded = without_tags
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut normalized = String::with_capacity(decoded.len());
    let mut previous_space = true;
    for character in decoded.nfkc().flat_map(char::to_lowercase) {
        let keep = character.is_alphanumeric()
            || matches!(character, '-' | '.' | '~')
            || ('\u{4e00}'..='\u{9fff}').contains(&character)
            || ('\u{3040}'..='\u{30ff}').contains(&character);
        if keep {
            normalized.push(character);
            previous_space = false;
        } else if !previous_space {
            normalized.push(' ');
            previous_space = true;
        }
    }
    normalized.trim().to_string()
}

pub fn match_anime(title: &str, aliases: &[String]) -> AnimeMatch {
    let normalized_title = normalize_title(title);
    let mut best = AnimeMatch::None;
    for alias in aliases {
        let normalized_alias = normalize_title(alias);
        if normalized_alias.chars().count() < 2 {
            continue;
        }
        if normalized_title == normalized_alias {
            return AnimeMatch::Exact;
        }
        if normalized_title.starts_with(&normalized_alias)
            && alias_boundary_after(&normalized_title, normalized_alias.len(), &normalized_alias)
        {
            best = AnimeMatch::Exact;
            continue;
        }
        if contains_alias(&normalized_title, &normalized_alias) {
            if best != AnimeMatch::Exact {
                best = AnimeMatch::Strong;
            }
            continue;
        }
        let tokens: Vec<_> = normalized_alias
            .split_whitespace()
            .filter(|token| token.chars().count() >= 2)
            .collect();
        let title_tokens: Vec<_> = normalized_title.split_whitespace().collect();
        if tokens.len() >= 2
            && tokens
                .iter()
                .all(|token| title_tokens.iter().any(|candidate| candidate == token))
            && !matches!(best, AnimeMatch::Exact | AnimeMatch::Strong)
        {
            best = AnimeMatch::Weak;
        }
    }
    best
}

fn contains_alias(title: &str, alias: &str) -> bool {
    title.match_indices(alias).any(|(start, _)| {
        let before_ok = start == 0
            || !alias_is_latin(alias)
            || title[..start]
                .chars()
                .next_back()
                .is_none_or(|character| !character.is_ascii_alphanumeric());
        before_ok && alias_boundary_after(title, start + alias.len(), alias)
    })
}

fn alias_boundary_after(title: &str, end: usize, alias: &str) -> bool {
    end == title.len()
        || !alias_is_latin(alias)
        || title[end..]
            .chars()
            .next()
            .is_none_or(|character| !character.is_ascii_alphanumeric())
}

fn alias_is_latin(alias: &str) -> bool {
    alias
        .chars()
        .any(|character| character.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_markup_width_case_and_spacing() {
        assert_eq!(
            normalize_title("【１０８０Ｐ】<em>Silent</em>   WITCH！ EP０８"),
            "1080p silent witch ep08"
        );
    }

    #[test]
    fn requires_a_real_alias_match() {
        let aliases = vec!["Silent Witch".to_string(), "沉默魔女".to_string()];
        assert_eq!(
            match_anime("【1080P】Silent Witch EP08", &aliases),
            AnimeMatch::Strong
        );
        assert_eq!(match_anime("别的魔女 EP08", &aliases), AnimeMatch::None);
        assert_eq!(
            match_anime("Silent Witcher EP08", &aliases),
            AnimeMatch::None
        );
    }
}
