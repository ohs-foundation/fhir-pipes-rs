pub mod chunks;
pub mod group;
pub mod head;
pub mod input;
pub mod location;
pub mod ndjson;
pub mod organization;
pub mod package;
pub mod spatial;

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
