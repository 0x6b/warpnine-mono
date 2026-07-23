//! Integration tests ported from fonttools-opentype-feature-freezer
//!
//! Test fixtures from: https://github.com/twardoch/fonttools-opentype-feature-freezer/tree/master/tests
//! OpenSans-Bold.subset.ttf is licensed under Apache License 2.0

use std::{collections::HashMap, string::ToString};

use font_feature_freezer::{
    FreezeOptions, freeze, freeze_features, freeze_features_with_stats, report,
};
use read_fonts::{FontRef, TableProvider, types::GlyphId16};

fn get_cmap(data: &[u8]) -> HashMap<u32, u16> {
    let font = FontRef::new(data).unwrap();
    let cmap = font.cmap().unwrap();
    let mut result = HashMap::new();

    for record in cmap.encoding_records() {
        if let Ok(subtable) = record.subtable(cmap.offset_data()) {
            for (codepoint, gid) in subtable.iter() {
                result.insert(codepoint, gid.to_u32() as u16);
            }
            break;
        }
    }
    result
}

fn get_glyph_name(data: &[u8], gid: u16) -> Option<String> {
    let font = FontRef::new(data).unwrap();
    if let Ok(post) = font.post() {
        post.glyph_name(GlyphId16::new(gid)).map(ToString::to_string)
    } else {
        None
    }
}

fn cmap_to_names(data: &[u8]) -> HashMap<u32, String> {
    let cmap = get_cmap(data);
    cmap.into_iter()
        .filter_map(|(cp, gid)| get_glyph_name(data, gid).map(|name| (cp, name)))
        .collect()
}

fn get_name_record(data: &[u8], name_id: u16) -> Option<String> {
    let font = FontRef::new(data).unwrap();
    if let Ok(name) = font.name() {
        for record in name.name_record() {
            if record.name_id().to_u16() == name_id
                && let Ok(s) = record.string(name.string_data())
            {
                return Some(s.to_string());
            }
        }
    }
    None
}

// ============================================================================
// Tests ported from pyftfeatfreeze test_freeze.py
// ============================================================================

#[test]
fn test_freeze_onum_opensans() {
    // Equivalent to pyftfeatfreeze test_freeze with onum feature
    let font_data = include_bytes!("fixtures/OpenSans-Bold.subset.ttf");

    let frozen = freeze_features(font_data, ["onum"]).unwrap();

    let names = cmap_to_names(&frozen);

    assert_eq!(names.get(&0x30), Some(&"zero.os".to_string()));
    assert_eq!(names.get(&0x31), Some(&"one.os".to_string()));
    assert_eq!(names.get(&0x32), Some(&"two.os".to_string()));
    assert_eq!(names.get(&0x33), Some(&"three.os".to_string()));
    assert_eq!(names.get(&0x34), Some(&"four.os".to_string()));
    assert_eq!(names.get(&0x35), Some(&"five.os".to_string()));
    assert_eq!(names.get(&0x36), Some(&"six.os".to_string()));
    assert_eq!(names.get(&0x37), Some(&"seven.os".to_string()));
    assert_eq!(names.get(&0x38), Some(&"eight.os".to_string()));
    assert_eq!(names.get(&0x39), Some(&"nine.os".to_string()));

    // Letters unchanged
    assert_eq!(names.get(&0x61), Some(&"a".to_string()));
    assert_eq!(names.get(&0x62), Some(&"b".to_string()));
    assert_eq!(names.get(&0x63), Some(&"c".to_string()));
}

#[test]
fn test_freeze_pnum_opensans() {
    let font_data = include_bytes!("fixtures/OpenSans-Bold.subset.ttf");

    let frozen = freeze_features(font_data, ["pnum"]).unwrap();

    let names = cmap_to_names(&frozen);

    assert_eq!(names.get(&0x31), Some(&"one.pnum".to_string()));
    assert_eq!(names.get(&0x30), Some(&"zero".to_string()));
}

#[test]
fn test_freeze_preserves_format4_platform_encoding_ids() {
    // The source font's only 'cmap' subtable is (platform 3, encoding 1), format 4 -- the
    // Windows-required Unicode BMP subtable. Freezing must not rewrite it into format 12
    // under (3, 10): Windows refuses to load a font whose 'cmap' has no subtable matching
    // the 'name' table's (3, 1) platform/encoding, so silently converting a valid (3, 1)
    // format 4 subtable away breaks the font on Windows.
    let font_data = include_bytes!("fixtures/OpenSans-Bold.subset.ttf");
    let frozen = freeze_features(font_data, ["pnum"]).unwrap();
    let font = FontRef::new(&frozen).unwrap();
    let cmap = font.cmap().unwrap();

    let records: Vec<_> = cmap.encoding_records().iter().collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].platform_id() as u16, 3);
    assert_eq!(records[0].encoding_id(), 1);

    let subtable = records[0].subtable(cmap.offset_data()).unwrap();
    assert!(matches!(subtable, read_fonts::tables::cmap::CmapSubtable::Format4(_)));
}

#[test]
fn test_freeze_multiple_features_opensans() {
    let font_data = include_bytes!("fixtures/OpenSans-Bold.subset.ttf");

    let frozen = freeze_features(font_data, ["onum", "pnum"]).unwrap();

    let names = cmap_to_names(&frozen);

    assert_eq!(names.get(&0x30), Some(&"zero.os".to_string()));
    assert_eq!(names.get(&0x31), Some(&"one.os".to_string()));
}

#[test]
fn test_freeze_ss01_alternate_substitution() {
    // Equivalent to pyftfeatfreeze test_warn_substituting_glyphs_without_unicode
    // Tests that 'a' maps to first alternate 'a.alt1'
    let font_data = include_bytes!("fixtures/SubGlyphsWithoutUnicode.ttf");

    let frozen = freeze_features(font_data, ["ss01"]).unwrap();

    let names = cmap_to_names(&frozen);

    assert_eq!(names.get(&0x61), Some(&"a.alt1".to_string()));
}

#[test]
fn test_freeze_nonexistent_feature() {
    // Equivalent to pyftfeatfreeze test_cant_open (tests error handling)
    let font_data = include_bytes!("fixtures/OpenSans-Bold.subset.ttf");

    let result = freeze_features(font_data, ["xxxx"]);

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("No matching features")
            || err.to_string().contains("no matching features"),
        "Expected 'no matching features' error, got: {err}"
    );
}

#[test]
fn test_freeze_with_stats() {
    let font_data = include_bytes!("fixtures/OpenSans-Bold.subset.ttf");

    let (frozen, stats) = freeze_features_with_stats(font_data, ["onum"]).unwrap();

    assert_eq!(stats.features_requested, 1);
    assert!(stats.lookups_processed > 0);
    assert!(stats.substitutions_applied > 0);

    let names = cmap_to_names(&frozen);
    assert_eq!(names.get(&0x30), Some(&"zero.os".to_string()));
}

#[test]
fn test_freeze_lnum_keeps_default_numerals() {
    let font_data = include_bytes!("fixtures/OpenSans-Bold.subset.ttf");

    let frozen = freeze_features(font_data, ["lnum"]).unwrap();

    let names = cmap_to_names(&frozen);

    assert_eq!(names.get(&0x30), Some(&"zero".to_string()));
    assert_eq!(names.get(&0x31), Some(&"one".to_string()));
}

#[test]
fn test_original_font_unchanged() {
    let font_data = include_bytes!("fixtures/OpenSans-Bold.subset.ttf");

    let original_names = cmap_to_names(font_data);

    let _frozen = freeze_features(font_data, ["onum"]).unwrap();

    let after_names = cmap_to_names(font_data);

    assert_eq!(original_names, after_names);
    assert_eq!(original_names.get(&0x30), Some(&"zero".to_string()));
}

#[test]
fn test_invalid_font_data() {
    let bad_data = b"not a font";

    let result = freeze_features(bad_data, ["onum"]);

    assert!(result.is_err());
}

// ============================================================================
// Tests for new features (suffix, usesuffix, replacenames, etc.)
// ============================================================================

#[test]
fn test_freeze_with_suffix() {
    let font_data = include_bytes!("fixtures/OpenSans-Bold.subset.ttf");

    let options = FreezeOptions::new(["onum"]).with_suffix();
    let result = freeze(font_data, &options).unwrap();

    // Family name should have " onum" appended
    let family = get_name_record(&result.data, 1).unwrap();
    assert!(family.contains("onum"), "Expected 'onum' in family name: {family}");
}

#[test]
fn test_freeze_with_usesuffix() {
    let font_data = include_bytes!("fixtures/OpenSans-Bold.subset.ttf");

    let options = FreezeOptions::new(["onum"]).with_usesuffix("SC");
    let result = freeze(font_data, &options).unwrap();

    // Family name should be "Open Sans SC"
    let family = get_name_record(&result.data, 1).unwrap();
    assert_eq!(family, "Open Sans SC");

    // PostScript name should be "OpenSansSC-Bold"
    let ps_name = get_name_record(&result.data, 6).unwrap();
    assert_eq!(ps_name, "OpenSansSC-Bold");
}

#[test]
fn test_freeze_with_replacenames() {
    let font_data = include_bytes!("fixtures/OpenSans-Bold.subset.ttf");

    let options = FreezeOptions::new(["onum"]).with_replacenames("Open Sans/Closed Sans");
    let result = freeze(font_data, &options).unwrap();

    let family = get_name_record(&result.data, 1).unwrap();
    assert_eq!(family, "Closed Sans");
}

#[test]
fn test_freeze_warns_on_glyphs_without_unicode() {
    let font_data = include_bytes!("fixtures/SubGlyphsWithoutUnicode.ttf");

    let options = FreezeOptions::new(["ss01"]).with_warnings();
    let result = freeze(font_data, &options).unwrap();

    // Should have warning about xxx -> yyy substitution
    assert!(!result.warnings.is_empty(), "Expected warnings for glyphs without unicode");
    assert!(
        result.warnings.iter().any(|w| w.contains("xxx") && w.contains("yyy")),
        "Expected warning about xxx -> yyy, got: {:?}",
        result.warnings
    );
}

#[test]
fn test_freeze_returns_remapped_names() {
    let font_data = include_bytes!("fixtures/OpenSans-Bold.subset.ttf");

    let options = FreezeOptions::new(["onum"]).with_warnings();
    let result = freeze(font_data, &options).unwrap();

    // Should include remapped glyph names (those with unicode values)
    assert!(!result.remapped_names.is_empty());
    assert!(result.remapped_names.contains(&"zero.os".to_string()));
}

#[test]
fn test_freeze_script_filter_no_match() {
    let font_data = include_bytes!("fixtures/OpenSans-Bold.subset.ttf");

    let options = FreezeOptions::new(["onum"]).with_script("zzzz");
    let result = freeze(font_data, &options);

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("No matching features")
            || err.to_string().contains("no matching features"),
        "Expected 'no matching features' error, got: {err}"
    );
}

// ============================================================================
// Tests for report functionality
// ============================================================================

#[test]
fn test_report() {
    let font_data = include_bytes!("fixtures/OpenSans-Bold.subset.ttf");

    let r = report(font_data).unwrap();

    // Should have latn script
    assert!(r.scripts_langs.iter().any(|s| s.contains("latn")));

    // Should have onum, pnum, lnum, tnum features
    assert!(r.features.contains(&"onum".to_string()));
    assert!(r.features.contains(&"pnum".to_string()));
    assert!(r.features.contains(&"lnum".to_string()));
    assert!(r.features.contains(&"tnum".to_string()));
}
