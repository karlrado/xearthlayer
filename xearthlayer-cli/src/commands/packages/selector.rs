//! Region selectors: a partial region code that selects a group of packages.
//!
//! Region codes are hierarchical by convention (`NA-CANADA-WEST`), so a
//! leading part of a code selects every package beneath it: `NA` selects all
//! of North America and `NA-CANADA` both halves of Canada. Matching is a
//! case-insensitive prefix match on the raw string and always expands, so an
//! exact title is one match among the rest rather than a short circuit.
//!
//! A selector is user input that never becomes a path. Only the titles it
//! resolves to reach the filesystem, and those were already validated as
//! region codes when their library index or metadata was parsed.

use crate::error::CliError;

/// Resolve `selector` against `candidates`, keeping every candidate whose
/// region code starts with it, in candidate order.
///
/// Each command passes the candidates it reads: library entries for
/// `install`, installed packages for `update`, `remove` and `info`. An empty
/// result is an error that lists the codes those candidates offer.
pub fn resolve<T>(
    selector: &str,
    candidates: Vec<T>,
    code: impl Fn(&T) -> &str,
) -> Result<Vec<T>, CliError> {
    let selector = selector.trim();
    if selector.is_empty() {
        return Err(CliError::Packages(
            "Region code must not be empty".to_string(),
        ));
    }

    let selects = |c: &T| starts_with_ignore_case(code(c), selector);
    if !candidates.iter().any(selects) {
        return Err(no_match(selector, candidates.iter().map(&code)));
    }
    Ok(candidates.into_iter().filter(selects).collect())
}

/// `str::starts_with`, ignoring ASCII case. `get` rather than slicing, so a
/// selector longer than the code, or one that ends inside a multi-byte
/// character, is simply not a match.
fn starts_with_ignore_case(code: &str, prefix: &str) -> bool {
    code.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn no_match<'a>(selector: &str, codes: impl Iterator<Item = &'a str>) -> CliError {
    let mut codes: Vec<&str> = codes.collect();
    codes.sort_unstable();
    codes.dedup();
    let available = if codes.is_empty() {
        "none".to_string()
    } else {
        codes.join(", ")
    };
    CliError::Packages(format!(
        "No package matches '{selector}'. Available: {available}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The world rebuild's region codes, plus a legacy `EU` that coexists
    /// with its successors while the rebuild is promoted.
    const CODES: &[&str] = &[
        "NA-USA-BAHAMAS-BERMUDA",
        "NA-CANADA-WEST",
        "NA-CANADA-EAST",
        "NA-GREENLAND",
        "NA-MEXICO-CENTRAL-CARIBBEAN",
        "EU",
        "EU-WEST",
        "EU-EAST",
        "AF-NORTH",
        "AF-SOUTH",
        "OC-AUSTRALIA-NZ-PACIFIC",
        "AS-CHINA-MONGOLIA",
        "AS-INDIA-MIDDLE-EAST-HIMALAYIAN",
        "AS-ASIA-PACIFIC",
        "AS-RUSSIA-WEST",
        "AS-RUSSIA-EAST",
    ];

    fn select(selector: &str) -> Result<Vec<&'static str>, CliError> {
        resolve(selector, CODES.to_vec(), |c| c)
    }

    fn error_message(result: Result<Vec<&'static str>, CliError>) -> String {
        match result {
            Err(CliError::Packages(message)) => message,
            other => panic!("expected a packages error, got {:?}", other.ok()),
        }
    }

    #[test]
    fn a_continent_code_selects_every_package_on_it() {
        assert_eq!(
            select("NA").unwrap(),
            vec![
                "NA-USA-BAHAMAS-BERMUDA",
                "NA-CANADA-WEST",
                "NA-CANADA-EAST",
                "NA-GREENLAND",
                "NA-MEXICO-CENTRAL-CARIBBEAN",
            ]
        );
    }

    #[test]
    fn a_country_code_selects_every_part_of_it() {
        assert_eq!(
            select("NA-CANADA").unwrap(),
            vec!["NA-CANADA-WEST", "NA-CANADA-EAST"]
        );
        assert_eq!(
            select("AS-RUSSIA").unwrap(),
            vec!["AS-RUSSIA-WEST", "AS-RUSSIA-EAST"]
        );
        assert_eq!(
            select("NA-MEXICO").unwrap(),
            vec!["NA-MEXICO-CENTRAL-CARIBBEAN"]
        );
    }

    #[test]
    fn a_full_code_selects_exactly_that_package() {
        assert_eq!(select("AF-SOUTH").unwrap(), vec!["AF-SOUTH"]);
    }

    #[test]
    fn matching_ignores_case() {
        assert_eq!(
            select("na-canada").unwrap(),
            vec!["NA-CANADA-WEST", "NA-CANADA-EAST"]
        );
    }

    #[test]
    fn an_exact_title_does_not_stop_the_expansion() {
        assert_eq!(select("EU").unwrap(), vec!["EU", "EU-WEST", "EU-EAST"]);
    }

    #[test]
    fn a_selector_need_not_end_on_a_segment_boundary() {
        assert_eq!(
            select("NA-CAN").unwrap(),
            vec!["NA-CANADA-WEST", "NA-CANADA-EAST"]
        );
        assert_eq!(select("AF-S").unwrap(), vec!["AF-SOUTH"]);
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(select("  AF-NORTH ").unwrap(), vec!["AF-NORTH"]);
    }

    #[test]
    fn no_match_is_an_error_naming_the_selector_and_the_available_codes() {
        let message = error_message(resolve("XX", vec!["NA", "EU-WEST"], |c| c));

        assert!(message.contains("'XX'"), "{message}");
        assert!(message.contains("Available: EU-WEST, NA"), "{message}");
    }

    #[test]
    fn available_codes_are_listed_once_each() {
        // A region with both an ortho and an overlay package is two candidates.
        let message = error_message(resolve("XX", vec!["NA", "NA", "EU"], |c| c));

        assert!(message.contains("Available: EU, NA"), "{message}");
    }

    #[test]
    fn no_candidates_at_all_says_so() {
        let message = error_message(resolve("NA", Vec::<&str>::new(), |c| c));

        assert!(message.contains("Available: none"), "{message}");
    }

    #[test]
    fn an_empty_selector_selects_nothing() {
        // Every code starts with the empty string, so without this guard
        // `install ""` would install every package in the library.
        let message = error_message(select("   "));

        assert!(message.contains("must not be empty"), "{message}");
    }
}
