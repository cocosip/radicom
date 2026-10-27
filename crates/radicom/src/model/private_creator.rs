//! Private creator identity (TAG-02 supporting type).
//!
//! Creators are cheaply cloneable and compared/hashed by string content.
//! Interning through the dictionary creator cache arrives with DICT-02;
//! the type is designed so interning never changes equality semantics.

use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrivateCreator(Arc<str>);

impl PrivateCreator {
    pub fn new(name: impl Into<Arc<str>>) -> Self {
        PrivateCreator(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PrivateCreator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equality_and_order_by_content() {
        let a = PrivateCreator::new("ACME 1.2");
        let b = PrivateCreator::new(format!("ACME {}", "1.2"));
        assert_eq!(a, b, "different Arc instances, same content");
        let c = PrivateCreator::new("ACME 1.3");
        assert!(a < c);
        assert_eq!(a.as_str(), "ACME 1.2");
        assert_eq!(a.to_string(), "ACME 1.2");
    }
}
