//! The system locale, in the form AdGuard's filter databases use.
//!
//! `filter_localisation.lang` and `filter_group_localisation.lang` hold
//! POSIX-style tags with an **underscore** and an uppercase region — `en`,
//! `pl`, `pt_BR`, `pt_PT`, `es_ES`, `zh_TW` (44 languages in
//! `agflm_standard.db`, 34 in `agflm_dns.db`). A BCP-47 hyphen never appears,
//! so `pt-BR` must be normalised or it silently matches nothing and the UI
//! quietly falls back to English.
//!
//! Region-specific rows are the exception rather than the rule, so every
//! lookup tries the full tag first and then the bare language:
//! `pt_BR` -> `pt` -> the English `filter.title` column.

use std::env;

/// A resolved pair of language candidates for the localisation joins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locale {
    primary: String,
    base: Option<String>,
}

impl Locale {
    /// Read the locale from the environment, in the order the C library
    /// itself resolves it. Falls back to English rather than failing, since a
    /// missing locale is not an error condition for us.
    pub fn from_env() -> Self {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(env::var_os)
            .map(|value| value.to_string_lossy().into_owned())
            .find(|value| !value.trim().is_empty())
            .map_or_else(Self::english, |value| Self::parse(&value))
    }

    pub fn english() -> Self {
        Self {
            primary: "en".to_owned(),
            base: None,
        }
    }

    /// Normalise one locale tag: drop the codeset and modifier, convert a
    /// BCP-47 hyphen to the databases' underscore, and split off the bare
    /// language as the fallback candidate.
    pub fn parse(tag: &str) -> Self {
        // `pt_BR.UTF-8@euro` -> `pt_BR`
        let tag = tag.split(['.', '@']).next().unwrap_or_default().trim();
        let tag = tag.replace('-', "_");
        let mut parts = tag.split('_');
        let language = parts.next().unwrap_or_default().to_ascii_lowercase();

        // `C` and `POSIX` mean "no localisation at all". The databases carry
        // only real languages, so ask for English instead of looking up a tag
        // that cannot exist.
        if language.is_empty() || language == "c" || language == "posix" {
            return Self::english();
        }

        match parts.next().filter(|region| !region.is_empty()) {
            Some(region) => Self {
                primary: format!("{language}_{}", region.to_ascii_uppercase()),
                base: Some(language),
            },
            None => Self {
                primary: language,
                base: None,
            },
        }
    }

    /// First lookup candidate — the full tag, e.g. `pt_BR`.
    pub fn primary(&self) -> &str {
        &self.primary
    }

    /// Second lookup candidate — the bare language, e.g. `pt`.
    ///
    /// Equal to [`Self::primary`] when the tag carries no region, so callers
    /// can always bind two parameters and keep one statement shape.
    pub fn fallback(&self) -> &str {
        self.base.as_deref().unwrap_or(&self.primary)
    }
}

impl Default for Locale {
    fn default() -> Self {
        Self::english()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_region_from_language() {
        let locale = Locale::parse("pt_BR.UTF-8");
        assert_eq!(locale.primary(), "pt_BR");
        assert_eq!(locale.fallback(), "pt");
    }

    /// The databases use underscores; a hyphenated tag must not be looked up
    /// verbatim, or every localisation silently misses.
    #[test]
    fn normalises_bcp47_hyphen() {
        assert_eq!(Locale::parse("pt-BR").primary(), "pt_BR");
        assert_eq!(Locale::parse("zh-tw").primary(), "zh_TW");
    }

    #[test]
    fn language_without_region_repeats_as_fallback() {
        let locale = Locale::parse("pl");
        assert_eq!(locale.primary(), "pl");
        assert_eq!(locale.fallback(), "pl");
    }

    #[test]
    fn drops_codeset_and_modifier() {
        assert_eq!(Locale::parse("sr_RS@latin").primary(), "sr_RS");
        assert_eq!(Locale::parse("de_DE.ISO-8859-1").primary(), "de_DE");
    }

    /// `C`/`POSIX` are not languages, and neither is an empty `LANG`.
    #[test]
    fn c_locale_becomes_english() {
        for tag in ["C", "POSIX", "c.UTF-8", "", "   "] {
            assert_eq!(Locale::parse(tag), Locale::english(), "tag {tag:?}");
        }
    }

    #[test]
    fn reference_machine_locale() {
        let locale = Locale::parse("en_US.UTF-8");
        assert_eq!(locale.primary(), "en_US");
        assert_eq!(locale.fallback(), "en");
    }
}
