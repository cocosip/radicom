//! Tag interning index (TAG-04; docs/01-architecture.md §3).
//!
//! Reuses interned instances for known tags so the parser and AT elements
//! avoid per-element allocations. Unknown tags are constructed fresh and
//! are not inserted (mirrors fo-dicom's read-only intern table).

use crate::model::tag::Tag;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Arc;

// The table is built once and never mutated (unknown tags are not inserted), so
// it needs no lock: a read is a plain load, not a shared cache-line write
// (docs/fo-dicom-alignment/05 §3.3).
static INDEX: Lazy<HashMap<u32, Arc<Tag>>> = Lazy::new(|| {
    // Seed from the generated constant table: ~5259 entries, built once.
    let mut m = HashMap::with_capacity(8192);
    for raw in radicom_dict::generated::dict::DICTIONARY {
        m.entry((raw.group as u32) << 16 | raw.element as u32).or_insert_with(|| {
            Arc::new(Tag::new(raw.group, raw.element))
        });
    }
    m
});

/// Returns the interned tag for a known tag; `None` for unknown values
/// (callers construct their own). The reference lives as long as the process,
/// so the parser can hand it out without an `Arc` clone per element.
pub fn lookup_interned(group: u16, element: u16) -> Option<&'static Tag> {
    let key = (group as u32) << 16 | element as u32;
    INDEX.get(&key).map(|arc| arc.as_ref())
}

/// Returns the interned instance for a known tag, or a fresh (uninterned)
/// tag for unknown values.
pub fn lookup_or_create(group: u16, element: u16) -> Tag {
    lookup_interned(group, element).cloned().unwrap_or_else(|| Tag::new(group, element))
}

/// Number of interned tags (the generated public exact table size).
pub fn len() -> usize {
    INDEX.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tags_are_interned_stably() {
        assert!(len() > 5000, "seeded from the generated table");
        let a = lookup_interned(0x0010, 0x0020).expect("PatientID is known");
        let b = lookup_interned(0x0010, 0x0020).expect("PatientID is known");
        assert!(std::ptr::eq(a, b), "same instance returned across calls");
        assert_eq!(a, &Tag::PATIENT_ID);

        let value = lookup_or_create(0x7FE0, 0x0010);
        assert_eq!(value, Tag::PIXEL_DATA);
    }

    #[test]
    fn unknown_tags_are_fresh() {
        assert!(lookup_interned(0x0045, 0xABCD).is_none());
        let t = lookup_or_create(0x0045, 0xABCD);
        assert_eq!(t, Tag::new(0x0045, 0xABCD));
        assert_eq!(len(), lookup_interned(0x0010, 0x0020).map(|_| len()).unwrap_or(len()));
    }
}
