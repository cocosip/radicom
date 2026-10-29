//! Generic value range used by date/time elements (DS-08 support).

/// An inclusive `min-max` range (DICOM date/time query ranges).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range<T> {
    pub min: T,
    pub max: T,
}

impl<T> Range<T> {
    pub const fn new(min: T, max: T) -> Self {
        Range { min, max }
    }
}

impl<T: PartialOrd<T> + Clone> Range<T> {
    pub fn contains(&self, value: &T) -> bool {
        value >= &self.min && value <= &self.max
    }

    /// Inclusive union; `None` when the ranges are disjoint.
    pub fn join(&self, other: &Range<T>) -> Option<Range<T>> {
        if self.min > other.max || other.min > self.max {
            return None;
        }
        Some(Range {
            min: if self.min <= other.min {
                self.min.clone()
            } else {
                other.min.clone()
            },
            max: if self.max >= other.max {
                self.max.clone()
            } else {
                other.max.clone()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_and_join() {
        let a = Range::new(1, 5);
        assert!(a.contains(&3));
        assert!(!a.contains(&6));
        assert_eq!(a.join(&Range::new(4, 9)), Some(Range::new(1, 9)));
        assert_eq!(a.join(&Range::new(6, 9)), None);
    }
}
