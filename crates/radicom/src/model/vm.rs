//! Value Multiplicity (VM-01; docs/01-architecture.md §3).

use crate::error::{DataError, Error};

/// Value multiplicity: how many values an attribute may hold.
///
/// Grammar (dictionary `vm` strings): `"1"`, `"1-n"`, `"2-2n"`, `"1-3"`,
/// `"3"` — `min[-(max|n)[mult]]` where a trailing `n` marks multiplicity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vm {
    pub min: u32,
    /// `None` = unbounded (`n`).
    pub max: Option<u32>,
    pub multiplicity: u32,
}

impl Vm {
    pub const ONE: Vm = Vm {
        min: 1,
        max: Some(1),
        multiplicity: 1,
    };

    /// Parses a dictionary VM string (VM-01), mirroring fo-dicom:
    /// tokenized on `-`, spaces and the `or` keyword (so `"1-n or 1"`
    /// resolves to its first alternative); `"n"` marks unbounded, a trailing
    /// `n` marks the multiplicity step.
    pub fn parse(s: &str) -> Result<Vm, Error> {
        let invalid = || {
            Error::Data(DataError::Other {
                message: format!("invalid VM \"{s}\""),
            })
        };
        let parts: Vec<&str> = s
            .trim()
            .split(['-', ' ', '\t'])
            .map(str::trim)
            .filter(|p| !p.is_empty() && !p.eq_ignore_ascii_case("or"))
            .collect();
        match parts.as_slice() {
            [] => Err(invalid()),
            [single] => {
                let n: u32 = single.parse().map_err(|_| invalid())?;
                Ok(Vm {
                    min: n,
                    max: Some(n),
                    multiplicity: n,
                })
            }
            // extra alternatives after the second token are ignored
            [min_s, max_s, ..] => {
                let min: u32 = min_s.parse().map_err(|_| invalid())?;
                if let Some(head) = max_s.strip_suffix('n').or_else(|| max_s.strip_suffix('N')) {
                    let multiplicity = head.parse::<u32>().unwrap_or(1);
                    Ok(Vm {
                        min,
                        max: None,
                        multiplicity: multiplicity.max(1),
                    })
                } else {
                    let max: u32 = max_s.parse().map_err(|_| invalid())?;
                    Ok(Vm {
                        min,
                        max: Some(max),
                        multiplicity: 1,
                    })
                }
            }
        }
    }

    /// Whether `count` values satisfy the min/max bounds. The multiplicity
    /// step is metadata only (mirrors fo-dicom's ValidateVM which checks
    /// min/max, not the step).
    pub fn matches(&self, count: usize) -> bool {
        let count = count as u32;
        count >= self.min && self.max.is_none_or(|max| count <= max)
    }
}

impl std::fmt::Display for Vm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.max, self.multiplicity) {
            (Some(max), m) if max == self.min && (m == 1 || m == max) => write!(f, "{}", self.min),
            (Some(max), 1) => write!(f, "{}-{}", self.min, max),
            (Some(max), m) if m == max => write!(f, "{}-{}n", self.min, max),
            (None, 1) => write!(f, "{}-n", self.min),
            (None, m) => write!(f, "{}-{}n", self.min, m),
            (Some(max), m) => write!(f, "{}-{} (multiplicity {m})", self.min, max),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display_round_trip() {
        let cases = [
            (
                "1",
                Vm {
                    min: 1,
                    max: Some(1),
                    multiplicity: 1,
                },
                "1",
            ),
            (
                "2",
                Vm {
                    min: 2,
                    max: Some(2),
                    multiplicity: 2,
                },
                "2",
            ),
            (
                "1-n",
                Vm {
                    min: 1,
                    max: None,
                    multiplicity: 1,
                },
                "1-n",
            ),
            (
                "2-n",
                Vm {
                    min: 2,
                    max: None,
                    multiplicity: 1,
                },
                "2-n",
            ),
            (
                "1-3",
                Vm {
                    min: 1,
                    max: Some(3),
                    multiplicity: 1,
                },
                "1-3",
            ),
            // fo-dicom semantics: trailing n = unbounded max + step
            (
                "2-2n",
                Vm {
                    min: 2,
                    max: None,
                    multiplicity: 2,
                },
                "2-2n",
            ),
            (
                "3-n",
                Vm {
                    min: 3,
                    max: None,
                    multiplicity: 1,
                },
                "3-n",
            ),
            // fo-dicom alternative spelling: first alternative wins
            (
                "1-n or 1",
                Vm {
                    min: 1,
                    max: None,
                    multiplicity: 1,
                },
                "1-n",
            ),
        ];
        for (text, expected, display) in cases {
            let vm = Vm::parse(text).unwrap();
            assert_eq!(vm, expected, "{text}");
            assert_eq!(vm.to_string(), display, "{text} display");
        }
        assert!(Vm::parse("x").is_err());
        assert!(Vm::parse("").is_err());
        assert!(Vm::parse("-n").is_err());
        // fo-dicom tolerates a trailing dash (empty segments are dropped):
        assert_eq!(
            Vm::parse("1-").unwrap(),
            Vm {
                min: 1,
                max: Some(1),
                multiplicity: 1
            }
        );
    }

    #[test]
    fn bounds_matching() {
        assert!(Vm::parse("1").unwrap().matches(1));
        assert!(!Vm::parse("1").unwrap().matches(2));
        assert!(Vm::parse("1-n").unwrap().matches(999));
        assert!(Vm::parse("2-2n").unwrap().matches(2));
        assert!(!Vm::parse("2-2n").unwrap().matches(1));
        assert!(Vm::parse("1-3").unwrap().matches(3));
        assert!(!Vm::parse("1-3").unwrap().matches(4));
        assert!(Vm::ONE.matches(1));
    }
}
