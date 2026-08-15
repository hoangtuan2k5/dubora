const LANGUAGE_TIERS: &[&[&str]] = &[
    &["en"],
    &[
        "es", "fr", "de", "it", "nl", "pl", "uk", "ru", "sv", "da", "no", "fi", "ro", "cs", "el",
        "tr",
    ],
    &["zh"],
    &["pt"],
    &["ja"],
    &["ko"],
    &["id"],
    &["hi"],
    &["ar"],
];

fn matches_language(language: &str, prefix: &str) -> bool {
    language == prefix
        || language
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('-'))
}

/// Returns one available subtitle language using Dubora's documented fallback order.
pub fn select_subtitle_language(languages: impl IntoIterator<Item = String>) -> Option<String> {
    let mut available: Vec<String> = languages.into_iter().collect();
    available.sort();
    available.dedup();

    for tier in LANGUAGE_TIERS {
        for prefix in *tier {
            if let Some(language) = available
                .iter()
                .find(|language| matches_language(language, prefix))
            {
                return Some(language.clone());
            }
        }
    }
    available.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::select_subtitle_language;

    #[test]
    fn selects_english_before_every_other_available_language() {
        let selected =
            select_subtitle_language(["ja", "fr", "en-US", "zh-Hans"].map(str::to_owned));
        assert_eq!(selected.as_deref(), Some("en-US"));
    }

    #[test]
    fn falls_back_through_the_documented_language_order() {
        let selected = select_subtitle_language(["pt-BR", "zh-Hant", "ko"].map(str::to_owned));
        assert_eq!(selected.as_deref(), Some("zh-Hant"));
    }
}
