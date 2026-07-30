// Copyright 2026 The hashline-rs Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
//! Stateless range-size policy for hashline edit safety.
//!
//! Classifies edit ranges by size and produces tiered warnings:
//! - Small (≤5 lines): no warning
//! - Medium (6–20 lines): caution
//! - Large (>20 lines): stronger caution
//!
//! No session state — purely a function of the requested range.

const SMALL_MAX: usize = 5;
const MEDIUM_MAX: usize = 20;

/// Size classification of an edit range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSize {
    /// At most 5 lines.
    Small,
    /// 6–20 lines.
    Medium,
    /// More than 20 lines.
    Large,
}

impl RangeSize {
    /// Classify a line count into a size tier.
    pub fn classify(line_count: usize) -> Self {
        if line_count <= SMALL_MAX {
            Self::Small
        } else if line_count <= MEDIUM_MAX {
            Self::Medium
        } else {
            Self::Large
        }
    }
}

/// Evaluate a range edit and return a warning for medium or large ranges.
///
/// `start` and `end` are 0-based with `end` exclusive.
pub fn range_warning(start: usize, end: usize) -> Option<String> {
    let count = end.saturating_sub(start);
    match RangeSize::classify(count) {
        RangeSize::Large => Some(format!(
            "Caution: large range edit ({count} lines, lines {}-{}). \
             Verify the target range is correct.",
            start + 1,
            end,
        )),
        RangeSize::Medium => Some(format!(
            "Note: medium range edit ({count} lines, lines {}-{}).",
            start + 1,
            end,
        )),
        RangeSize::Small => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert_eq!(RangeSize::classify(1), RangeSize::Small);
        assert_eq!(RangeSize::classify(5), RangeSize::Small);
        assert_eq!(RangeSize::classify(6), RangeSize::Medium);
        assert_eq!(RangeSize::classify(20), RangeSize::Medium);
        assert_eq!(RangeSize::classify(21), RangeSize::Large);
        assert_eq!(RangeSize::classify(100), RangeSize::Large);
    }

    #[test]
    fn small_range_no_warning() {
        assert!(range_warning(0, 3).is_none());
    }

    #[test]
    fn medium_range_warns() {
        let w = range_warning(0, 10).unwrap();
        assert!(w.contains("10 lines"));
        assert!(w.contains("medium range"));
    }

    #[test]
    fn large_range_warns() {
        let w = range_warning(0, 30).unwrap();
        assert!(w.contains("30 lines"));
        assert!(w.contains("large range"));
    }

    #[test]
    fn empty_range_no_warning() {
        assert!(range_warning(5, 5).is_none());
    }
}
