//! WOFF2 conversion with automatic problematic glyph handling.
//!
//! This crate provides WOFF2 font conversion that automatically handles
//! problematic glyphs (like U+F8FF Apple logo) that cause OTS validation errors.
//!
//! # Example
//!
//! ```no_run
//! use warpnine_font_woff2::convert_to_woff2;
//!
//! let ttf_data: &[u8] = &[];
//! let woff2_data = convert_to_woff2(ttf_data).unwrap();
//! ```

use anyhow::{Context, Result, bail};
use hb_subset::{Blob, FontFace, SubsetInput, Tag};
use read_fonts::{
    FontRef, TableProvider,
    tables::cmap::{Cmap, Cmap4, Cmap12, CmapSubtable},
};
use ttf2woff2::{BrotliQuality, encode};

/// Codepoints known to cause WOFF2 OTS validation errors.
///
/// U+F8FF (Apple logo) references `.notdef` as a composite component,
/// which Chrome's OTS parser rejects during WOFF2 decompression.
pub const PROBLEMATIC_CODEPOINTS: &[u32] = &[0xF8FF];

/// Layout features to retain during subsetting.
const LAYOUT_FEATURES: &[&[u8; 4]] = &[
    b"aalt", b"afrc", b"ccmp", b"dlig", b"fwid", b"hwid", b"jp78", b"jp83", b"jp90", b"liga",
    b"locl", b"nlck", b"pnum", b"pwid", b"titl", b"vert", b"vjmo", b"vrt2", b"halt", b"vhal",
    b"kern", b"mark", b"mkmk", b"calt", b"rvrn", b"ss01", b"ss02", b"ss03", b"ss04", b"ss05",
    b"ss06", b"ss07", b"ss08", b"ss09", b"ss10", b"ss11", b"ss12", b"ss20", b"dnom", b"numr",
    b"frac", b"ordn", b"sups", b"subs", b"sinf", b"case", b"zero",
];

/// Standard OpenType name IDs to retain. HarfBuzz's defaults omit
/// typographic family/subfamily IDs 16/17, which breaks family grouping in
/// applications that prefer typographic names.
const STANDARD_NAME_IDS: std::ops::RangeInclusive<u32> = 0..=25;

/// Converts TTF font data to WOFF2 format.
///
/// This function:
/// 1. Subsets the font to exclude problematic codepoints (U+F8FF)
/// 2. Compresses the result to WOFF2 format
///
/// # Arguments
///
/// * `data` - Raw TTF font data
///
/// # Returns
///
/// WOFF2 compressed font data, or an error if conversion fails.
pub fn convert_to_woff2(data: &[u8]) -> Result<Vec<u8>> {
    let subset_data = subset_for_woff2(data)?;
    encode(&subset_data, BrotliQuality::default()).context("Failed to convert to WOFF2")
}

/// Subsets font data excluding problematic codepoints for WOFF2 conversion.
///
/// Reads the font's cmap table to get all mapped codepoints, then creates
/// a subset that excludes [`PROBLEMATIC_CODEPOINTS`]. The resulting TTF
/// can be safely converted to WOFF2.
///
/// # Arguments
///
/// * `data` - Raw TTF font data
///
/// # Returns
///
/// Subset TTF font data, or an error if subsetting fails.
pub fn subset_for_woff2(data: &[u8]) -> Result<Vec<u8>> {
    let font = FontRef::new(data).context("Failed to parse font")?;
    let cmap = font.cmap().context("Failed to read cmap table")?;

    let codepoints = extract_codepoints(&cmap);

    if codepoints.is_empty() {
        bail!("No valid codepoints found in font");
    }

    if !codepoints.iter().any(|cp| PROBLEMATIC_CODEPOINTS.contains(cp)) {
        return Ok(data.to_vec());
    }

    let filtered: Vec<u32> = codepoints
        .into_iter()
        .filter(|cp| !PROBLEMATIC_CODEPOINTS.contains(cp))
        .collect();

    let mut input = SubsetInput::new()?;

    {
        let mut name_ids = input.name_id_set();
        for name_id in STANDARD_NAME_IDS {
            name_ids.insert(name_id);
        }
    }

    {
        let mut feature_set = input.layout_feature_tag_set();
        for tag in LAYOUT_FEATURES {
            feature_set.insert(Tag::new(*tag));
        }
    }

    {
        let mut unicode_set = input.unicode_set();
        for cp in &filtered {
            if let Some(c) = char::from_u32(*cp) {
                unicode_set.insert(c);
            }
        }
    }

    let font_face = FontFace::new(Blob::from_bytes(data)?)?;
    let subset_font = input.subset_font(&font_face)?;
    Ok(subset_font.underlying_blob().to_vec())
}

fn extract_codepoints(cmap: &Cmap) -> Vec<u32> {
    let records = cmap.encoding_records();

    // Try format 12 first (full Unicode)
    for record in records {
        if let Ok(CmapSubtable::Format12(f12)) = record.subtable(cmap.offset_data()) {
            return extract_from_format12(&f12);
        }
    }

    // Fall back to format 4
    for record in records {
        if let Ok(CmapSubtable::Format4(f4)) = record.subtable(cmap.offset_data()) {
            return extract_from_format4(&f4);
        }
    }

    Vec::new()
}

fn extract_from_format12(f12: &Cmap12) -> Vec<u32> {
    let mut codepoints = Vec::new();
    for group in f12.groups() {
        let start = group.start_char_code();
        let end = group.end_char_code();
        for (gid, cp) in (group.start_glyph_id()..).zip(start..=end) {
            if gid != 0 {
                codepoints.push(cp);
            }
        }
    }
    codepoints
}

// `.get()` closures over `&BigEndian<T>` read as redundant to clippy, but the
// method-path form does not type-check: `get` takes `self` by value while the
// `.get(seg)` lookups yield references.
#[allow(clippy::redundant_closure_for_method_calls)]
fn extract_from_format4(f4: &Cmap4) -> Vec<u32> {
    let mut codepoints = Vec::new();

    let end_codes = f4.end_code();
    let start_codes = f4.start_code();
    let id_deltas = f4.id_delta();
    let id_range_offsets = f4.id_range_offsets();
    let glyph_id_array = f4.glyph_id_array();

    let seg_count = f4.seg_count_x2() as usize / 2;
    for seg in 0..seg_count {
        let end_code = end_codes.get(seg).map_or(0xFFFF, |v| v.get());
        let start_code = start_codes.get(seg).map_or(0, |v| v.get());
        let id_delta = id_deltas.get(seg).map_or(0, |v| v.get());
        let id_range_offset = id_range_offsets.get(seg).map_or(0, |v| v.get());

        if start_code == 0xFFFF {
            continue;
        }

        for cp in start_code..=end_code {
            let gid = if id_range_offset == 0 {
                ((i32::from(cp) + i32::from(id_delta)) & 0xFFFF) as u16
            } else {
                let glyph_idx =
                    (id_range_offset as usize / 2) + (cp - start_code) as usize - (seg_count - seg);
                if let Some(gid) = glyph_id_array.get(glyph_idx) {
                    let gid = gid.get();
                    if gid != 0 {
                        ((i32::from(gid) + i32::from(id_delta)) & 0xFFFF) as u16
                    } else {
                        0
                    }
                } else {
                    0
                }
            };

            if gid != 0 {
                codepoints.push(u32::from(cp));
            }
        }
    }
    codepoints
}

#[cfg(test)]
mod tests {
    use super::*;
    use read_fonts::TableProvider;

    #[test]
    fn test_problematic_codepoints() {
        assert!(PROBLEMATIC_CODEPOINTS.contains(&0xF8FF));
    }

    #[test]
    fn converts_real_variable_font_and_preserves_variations() {
        let input = font_test_data::VAZIRMATN_VAR;
        let subset = subset_for_woff2(input).unwrap();
        let subset_font = FontRef::new(&subset).unwrap();

        assert!(subset_font.cmap().is_ok());
        assert!(subset_font.fvar().is_ok());
        assert!(subset_font.gvar().is_ok());

        let woff2 = convert_to_woff2(input).unwrap();
        assert_eq!(&woff2[..4], b"wOF2");
    }
}
