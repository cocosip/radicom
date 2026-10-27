//! GENERATED module wiring (handwritten; `tags.rs` is refreshed by
//! `cargo xtask gen-dict`).
//!
//! The module is public so the generated inherent associated constants on
//! [`crate::Tag`] are reachable (`Tag::ACCESSION_NUMBER`, ...).

pub mod tags;

#[cfg(test)]
mod tests {
    #[test]
    fn generated_tag_constants() {
        use crate::model::tag::Tag;
        assert_eq!(Tag::ACCESSION_NUMBER, Tag::new(0x0008, 0x0050));
        assert_eq!(Tag::PIXEL_DATA, Tag::new(0x7FE0, 0x0010));
        assert_eq!(Tag::ROWS, Tag::new(0x0028, 0x0010));
        assert_eq!(Tag::PATIENT_NAME, Tag::new(0x0010, 0x0010));
        assert_eq!(Tag::SOP_INSTANCE_UID, Tag::new(0x0008, 0x0018));
    }
}
