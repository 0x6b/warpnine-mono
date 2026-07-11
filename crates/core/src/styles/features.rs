//! OpenType feature definitions.

#[derive(Debug, Clone, Copy)]
pub struct FeatureTag(pub &'static str);

impl AsRef<str> for FeatureTag {
    fn as_ref(&self) -> &str {
        self.0
    }
}

/// Features to freeze for WarpnineMono (static and variable).
pub const MONO_FEATURES: &[FeatureTag] = &[
    FeatureTag("ss01"), // Single-story a
    FeatureTag("ss02"), // Single-story g
    FeatureTag("ss03"), // Simplified f
    FeatureTag("ss04"), // Simplified i
    FeatureTag("ss05"), // Simplified l
    FeatureTag("ss06"), // Simplified r
    FeatureTag("ss07"), // Simplified italic diagonals / Serifless I
    FeatureTag("ss08"), // No-serif L and Z
    FeatureTag("ss10"), // Dotted zero
    FeatureTag("ss11"), // Simplified 1
    FeatureTag("ss12"), // Simplified @
    FeatureTag("pnum"), // Proportional numerals
];

/// Features to freeze for WarpnineSans and WarpnineSansCondensed.
pub const SANS_FEATURES: &[FeatureTag] = &[
    FeatureTag("ss01"), // Single-story a
    FeatureTag("ss02"), // Single-story g
    FeatureTag("ss03"), // Simplified f
    FeatureTag("ss04"), // Simplified i
    FeatureTag("ss05"), // Simplified l
    FeatureTag("ss06"), // Simplified r
    FeatureTag("ss07"), // Simplified italic diagonals / Serifless I
    FeatureTag("ss08"), // No-serif L and Z
    FeatureTag("ss12"), // Simplified @
    FeatureTag("case"), // Case-sensitive forms
    FeatureTag("pnum"), // Proportional numerals
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_ligatures_are_not_in_freeze_lists() {
        // These multi-glyph features remain in GSUB for application opt-in;
        // they cannot be represented by a single cmap mapping.
        for feature in ["dlig", "liga"] {
            assert!(!MONO_FEATURES.iter().any(|frozen| frozen.0 == feature));
            assert!(!SANS_FEATURES.iter().any(|frozen| frozen.0 == feature));
        }
    }
}
