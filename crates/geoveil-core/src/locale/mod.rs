//! Country -> locale helpers: default BCP-47 locale per country, Windows
//! GeoIDs, and BCP-47 -> POSIX locale name conversion.

mod countries;
mod geo_ids;

use countries::COUNTRY_LOCALES;
use geo_ids::GEO_IDS;

/// Locale used for unknown or empty country codes.
const FALLBACK_LOCALE: &str = "en-US";
/// POSIX locale used when a BCP-47 tag cannot be parsed.
const FALLBACK_POSIX: &str = "en_US.UTF-8";

/// Preferred region for a bare language tag whose code is not also a country
/// code with that language (e.g. `"ja"` -> `JP`, `"en"` -> `US`).
const LANGUAGE_DEFAULT_REGION: &[(&str, &str)] = &[
    ("ar", "SA"),
    ("cs", "CZ"),
    ("da", "DK"),
    ("el", "GR"),
    ("en", "US"),
    ("et", "EE"),
    ("fa", "IR"),
    ("he", "IL"),
    ("hi", "IN"),
    ("hy", "AM"),
    ("ja", "JP"),
    ("ka", "GE"),
    ("ko", "KR"),
    ("ms", "MY"),
    ("nb", "NO"),
    ("sl", "SI"),
    ("sq", "AL"),
    ("sr", "RS"),
    ("sv", "SE"),
    ("sw", "TZ"),
    ("uk", "UA"),
    ("vi", "VN"),
    ("zh", "CN"),
];

/// Normalises a country code: trimmed, ASCII upper case; `None` unless it is
/// exactly two ASCII letters.
fn normalize_iso2(iso2: &str) -> Option<String> {
    let code = iso2.trim();
    (code.len() == 2 && code.bytes().all(|b| b.is_ascii_alphabetic()))
        .then(|| code.to_ascii_uppercase())
}

/// Binary-searches a table sorted by ISO2 code.
fn lookup<T: Copy>(table: &[(&str, T)], iso2: &str) -> Option<T> {
    let code = normalize_iso2(iso2)?;
    table
        .binary_search_by(|(c, _)| (*c).cmp(code.as_str()))
        .ok()
        .map(|i| table[i].1)
}

/// Returns the default BCP-47 locale for an ISO-3166-1 alpha-2 country code,
/// e.g. `"DE"` -> `"de-DE"`, `"CH"` -> `"de-CH"`, `"BE"` -> `"nl-BE"`.
///
/// Input is case-insensitive and trimmed; unknown or malformed codes yield
/// `"en-US"`.
pub fn locale_for_country(iso2: &str) -> String {
    lookup(COUNTRY_LOCALES, iso2)
        .unwrap_or(FALLBACK_LOCALE)
        .to_owned()
}

/// Returns the Windows GeoID (for `Set-WinHomeLocation -GeoId`) of an
/// ISO-3166-1 alpha-2 country code, e.g. `"US"` -> `244`, `"DE"` -> `94`.
///
/// Input is case-insensitive. Returns `None` for unknown codes and for
/// territories whose GeoID is not in the embedded (deliberately conservative)
/// table.
pub fn windows_geo_id(iso2: &str) -> Option<u32> {
    lookup(GEO_IDS, iso2)
}

/// Parsed pieces of a BCP-47 tag that matter for POSIX locale names.
#[derive(Debug, PartialEq)]
struct Tag {
    language: String,
    script: Option<String>,
    region: Option<String>,
}

/// Parses `language[-Script][-REGION][-...]`, accepting `_` as separator and
/// ignoring any POSIX `.codeset` / `@modifier` suffix.
fn parse_tag(tag: &str) -> Option<Tag> {
    let core = tag.trim().split(['.', '@']).next().unwrap_or_default();
    let mut parts = core.split(['-', '_']).filter(|p| !p.is_empty());

    let language = parts.next()?;
    let language_ok =
        (2..=3).contains(&language.len()) && language.bytes().all(|b| b.is_ascii_alphabetic());
    if !language_ok {
        return None;
    }
    let mut tag = Tag {
        language: language.to_ascii_lowercase(),
        script: None,
        region: None,
    };

    let mut next = parts.next();
    if let Some(p) = next.filter(|p| p.len() == 4 && p.bytes().all(|b| b.is_ascii_alphabetic())) {
        let mut script = p.to_ascii_lowercase();
        script[..1].make_ascii_uppercase();
        tag.script = Some(script);
        next = parts.next();
    }
    if let Some(p) = next {
        let alpha2 = p.len() == 2 && p.bytes().all(|b| b.is_ascii_alphabetic());
        let numeric3 = p.len() == 3 && p.bytes().all(|b| b.is_ascii_digit());
        if alpha2 || numeric3 {
            tag.region = Some(p.to_ascii_uppercase());
        }
    }
    Some(tag)
}

/// Picks a region for a bare language tag, or `None` if nothing sensible.
fn default_region(language: &str) -> Option<String> {
    let lang_prefix = format!("{language}-");
    let same_code = language.to_ascii_uppercase();
    if lookup(COUNTRY_LOCALES, &same_code).is_some_and(|l| l.starts_with(&lang_prefix)) {
        return Some(same_code);
    }
    if let Some((_, region)) = LANGUAGE_DEFAULT_REGION.iter().find(|(l, _)| *l == language) {
        return Some((*region).to_owned());
    }
    COUNTRY_LOCALES
        .iter()
        .find(|(_, l)| l.starts_with(&lang_prefix))
        .map(|(c, _)| (*c).to_owned())
}

/// glibc `@modifier` needed to select a non-default script, if any.
fn posix_modifier(tag: &Tag) -> Option<&'static str> {
    match (tag.language.as_str(), tag.script.as_deref(), tag.region.as_deref()) {
        // sr_ME is Latin by default; sr_RS needs @latin.
        ("sr", Some("Latn"), Some("ME")) => None,
        ("sr", Some("Latn"), _) | ("be", Some("Latn"), _) => Some("@latin"),
        ("uz", Some("Cyrl"), _) => Some("@cyrillic"),
        _ => None,
    }
}

/// Converts a BCP-47 tag to a POSIX/glibc locale name with UTF-8 codeset,
/// e.g. `"de-DE"` -> `"de_DE.UTF-8"`, `"sr-Latn-RS"` -> `"sr_RS.UTF-8@latin"`.
///
/// A bare language gets a sensible default region (`"de"` -> `"de_DE.UTF-8"`,
/// `"en"` -> `"en_US.UTF-8"`). Unparseable input yields `"en_US.UTF-8"`.
pub fn posix_locale(bcp47: &str) -> String {
    let Some(mut tag) = parse_tag(bcp47) else {
        return FALLBACK_POSIX.to_owned();
    };
    if tag.region.is_none() {
        tag.region = default_region(&tag.language);
    }
    let modifier = posix_modifier(&tag).unwrap_or_default();
    match &tag.region {
        Some(region) => format!("{}_{}.UTF-8{}", tag.language, region, modifier),
        None => format!("{}.UTF-8{}", tag.language, modifier),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// All officially assigned ISO-3166-1 alpha-2 codes (249).
    const ISO_3166_1: &str = "AD AE AF AG AI AL AM AO AQ AR AS AT AU AW AX AZ \
        BA BB BD BE BF BG BH BI BJ BL BM BN BO BQ BR BS BT BV BW BY BZ \
        CA CC CD CF CG CH CI CK CL CM CN CO CR CU CV CW CX CY CZ \
        DE DJ DK DM DO DZ EC EE EG EH ER ES ET FI FJ FK FM FO FR \
        GA GB GD GE GF GG GH GI GL GM GN GP GQ GR GS GT GU GW GY \
        HK HM HN HR HT HU ID IE IL IM IN IO IQ IR IS IT JE JM JO JP \
        KE KG KH KI KM KN KP KR KW KY KZ LA LB LC LI LK LR LS LT LU LV LY \
        MA MC MD ME MF MG MH MK ML MM MN MO MP MQ MR MS MT MU MV MW MX MY MZ \
        NA NC NE NF NG NI NL NO NP NR NU NZ OM \
        PA PE PF PG PH PK PL PM PN PR PS PT PW PY QA RE RO RS RU RW \
        SA SB SC SD SE SG SH SI SJ SK SL SM SN SO SR SS ST SV SX SY SZ \
        TC TD TF TG TH TJ TK TL TM TN TO TR TT TV TW TZ \
        UA UG UM US UY UZ VA VC VE VG VI VN VU WF WS YE YT ZA ZM ZW";

    fn assert_sorted_unique<T>(table: &[(&str, T)]) {
        for pair in table.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{} !< {}", pair[0].0, pair[1].0);
        }
    }

    #[test]
    fn tables_sorted_and_unique() {
        assert_sorted_unique(COUNTRY_LOCALES);
        assert_sorted_unique(GEO_IDS);
    }

    #[test]
    fn every_iso_country_has_a_locale() {
        let codes: Vec<&str> = ISO_3166_1.split_whitespace().collect();
        assert_eq!(codes.len(), 249);
        assert_eq!(codes.iter().collect::<HashSet<_>>().len(), 249);
        for code in codes {
            assert!(lookup(COUNTRY_LOCALES, code).is_some(), "missing {code}");
        }
    }

    #[test]
    fn locale_entries_are_well_formed() {
        for (code, locale) in COUNTRY_LOCALES {
            let tag = parse_tag(locale).unwrap_or_else(|| panic!("bad tag {locale}"));
            assert!(tag.region.is_some(), "{code}: {locale} has no region");
        }
    }

    #[test]
    fn locale_samples() {
        let cases = [
            ("DE", "de-DE"),
            ("CH", "de-CH"),
            ("BE", "nl-BE"),
            ("CA", "en-CA"),
            ("US", "en-US"),
            ("GB", "en-GB"),
            ("FR", "fr-FR"),
            ("NL", "nl-NL"),
            ("JP", "ja-JP"),
            ("IR", "fa-IR"),
            ("BR", "pt-BR"),
            ("CN", "zh-CN"),
            ("NO", "nb-NO"),
            ("AT", "de-AT"),
            ("SE", "sv-SE"),
        ];
        for (code, want) in cases {
            assert_eq!(locale_for_country(code), want, "{code}");
        }
    }

    #[test]
    fn locale_case_insensitive_and_fallback() {
        assert_eq!(locale_for_country("de"), "de-DE");
        assert_eq!(locale_for_country(" Ch "), "de-CH");
        assert_eq!(locale_for_country(""), "en-US");
        assert_eq!(locale_for_country("ZZ"), "en-US");
        assert_eq!(locale_for_country("DEU"), "en-US");
        assert_eq!(locale_for_country("1A"), "en-US");
    }

    #[test]
    fn geo_id_samples() {
        let cases = [
            ("US", 244),
            ("DE", 94),
            ("GB", 242),
            ("FR", 84),
            ("NL", 176),
            ("JP", 122),
            ("CA", 39),
            ("CH", 223),
            ("AU", 12),
            // .NET RegionInfo("ME").GeoId; 20336 is not a valid GeoID.
            ("ME", 270),
            ("RS", 271),
        ];
        for (code, want) in cases {
            assert_eq!(windows_geo_id(code), Some(want), "{code}");
        }
    }

    #[test]
    fn geo_id_case_insensitive_and_unknown() {
        assert_eq!(windows_geo_id("us"), Some(244));
        assert_eq!(windows_geo_id("gB"), Some(242));
        assert_eq!(windows_geo_id("ZZ"), None);
        assert_eq!(windows_geo_id(""), None);
    }

    #[test]
    fn geo_ids_are_unique_and_countries_known() {
        let mut seen = HashSet::new();
        for (code, id) in GEO_IDS {
            assert!(seen.insert(*id), "duplicate GeoID {id} ({code})");
            assert!(lookup(COUNTRY_LOCALES, code).is_some(), "unknown country {code}");
        }
    }

    #[test]
    fn posix_samples() {
        let cases = [
            ("de-DE", "de_DE.UTF-8"),
            ("en-US", "en_US.UTF-8"),
            ("nl_BE", "nl_BE.UTF-8"),
            ("PT-br", "pt_BR.UTF-8"),
            ("zh-Hans-CN", "zh_CN.UTF-8"),
            ("sr-Latn-RS", "sr_RS.UTF-8@latin"),
            ("sr-Latn-ME", "sr_ME.UTF-8"),
            ("uz-Cyrl-UZ", "uz_UZ.UTF-8@cyrillic"),
            ("es-419", "es_419.UTF-8"),
            ("de_DE.UTF-8", "de_DE.UTF-8"),
            ("de", "de_DE.UTF-8"),
            ("en", "en_US.UTF-8"),
            ("ja", "ja_JP.UTF-8"),
            ("", "en_US.UTF-8"),
            ("1234", "en_US.UTF-8"),
        ];
        for (input, want) in cases {
            assert_eq!(posix_locale(input), want, "{input}");
        }
    }

    #[test]
    fn posix_for_every_country_locale() {
        for (_, locale) in COUNTRY_LOCALES {
            let posix = posix_locale(locale);
            assert!(posix.contains(".UTF-8"), "{locale} -> {posix}");
            assert!(posix.contains('_'), "{locale} -> {posix}");
        }
    }
}
