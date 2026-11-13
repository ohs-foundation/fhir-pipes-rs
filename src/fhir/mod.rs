pub(crate) mod chunks;
pub mod content;
pub(crate) mod edit;
pub mod group;
pub(crate) mod head;
pub(crate) mod input;
pub(crate) mod location;
pub mod ndjson;
pub(crate) mod organization;
pub(crate) mod package;
pub(crate) mod spatial;

pub use location::{Boundary, Location};
pub use organization::Organization;

/// A FHIR resource type name: an upper-case letter, then letters. It becomes
/// a file name, so nothing else is accepted.
pub fn valid_type(t: &str) -> bool {
    let mut chars = t.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase()) && chars.all(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_names_are_plain_words() {
        assert!(valid_type("MeasureReport"));
        assert!(valid_type("Location"));
        assert!(!valid_type(""));
        assert!(!valid_type("measureReport"));
        assert!(!valid_type("../Location"));
        assert!(!valid_type("Location?_count=1"));
    }
}
