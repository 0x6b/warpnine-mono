//! cmap table merging

use std::collections::HashMap;

use font_types::BigEndian;
use indexmap::{IndexMap, map::Entry};
use read_fonts::{
    FontRef, TableProvider,
    tables::cmap::{Cmap as ReadCmap, CmapSubtable, PlatformId},
};
use write_fonts::tables::cmap::{
    Cmap, Cmap4, Cmap12, CmapSubtable as WriteCmapSubtable, EncodingRecord,
    PlatformId as WritePlatformId, SequentialMapGroup,
};

use crate::{
    Result,
    context::GlyphOrder,
    glyph_order::GlyphName,
    types::{Codepoint, GlyphId},
};

/// Information about duplicate glyphs (same codepoint, different glyphs)
#[derive(Debug, Default)]
pub struct DuplicateGlyphInfo {
    /// Per-font mapping of original glyph name to disambiguated name
    pub per_font: Vec<HashMap<GlyphName, GlyphName>>,
}

/// Merge cmap tables from multiple fonts
///
/// Returns the merged cmap and information about duplicate glyphs
pub fn merge_cmap(
    fonts: &[FontRef],
    glyph_order: &GlyphOrder,
) -> Result<(Cmap, DuplicateGlyphInfo)> {
    let mut codepoint_to_glyph: IndexMap<Codepoint, GlyphName> = IndexMap::new();
    let mut duplicate_info = DuplicateGlyphInfo { per_font: vec![HashMap::new(); fonts.len()] };

    for (font_idx, font) in fonts.iter().enumerate() {
        let cmap = font.cmap()?;
        let mapping = glyph_order.font_mapping(font_idx);

        // Find best cmap subtable (prefer Unicode BMP, then full Unicode)
        if let Some(subtable) = find_best_subtable(&cmap) {
            for (codepoint, glyph_id) in iter_cmap_subtable(&subtable) {
                if let Some(name) = mapping.get(&glyph_id) {
                    match codepoint_to_glyph.entry(codepoint) {
                        Entry::Vacant(slot) => {
                            slot.insert(name.clone());
                        }
                        Entry::Occupied(slot) => {
                            let existing = slot.get();
                            if existing != name {
                                duplicate_info.per_font[font_idx]
                                    .insert(name.clone(), existing.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    // Emit both a format 4 (BMP) and a format 12 (full repertoire) subtable. Windows
    // requires the platform/encoding IDs referenced by the 'name' table (3, 1) to have a
    // matching 'cmap' subtable, or it refuses to load the font entirely ("not a valid font
    // file"); a format-12-only cmap satisfies macOS/CoreText but not Windows. See
    // https://learn.microsoft.com/en-us/typography/opentype/spec/recom#cmap-table
    //
    // write-fonts' own format-4 builder (used by `Cmap::from_mappings`) panics on this font:
    // it tries to encode large char-to-glyph deltas (common in a merged CJK glyph order,
    // where glyph IDs can sit tens of thousands of positions away from their codepoints)
    // into `idDelta`, which is a 16-bit field, and its overflow fallback
    // (`delta.rem_euclid(0x10000).try_into().unwrap()`) itself panics for delta magnitudes
    // above i16::MAX. We build format 4 by hand below, always using the explicit
    // glyph-id-array form (never `idDelta`), which sidesteps that bug entirely.
    let mut mappings: Vec<(u32, u32)> = codepoint_to_glyph
        .iter()
        .filter_map(|(cp, name)| {
            let mega_gid = glyph_order.mega_id(name)?;
            Some((cp.to_u32(), mega_gid.to_u32()))
        })
        .collect();

    mappings.sort_by_key(|(cp, _)| *cp);

    let encoding_records = build_encoding_records(&mappings);

    Ok((Cmap::new(encoding_records), duplicate_info))
}

/// Build the full set of 'cmap' encoding records: a format 4 subtable for the BMP range
/// (platform 0 encoding 3, platform 3 encoding 1) and a format 12 subtable for the full
/// Unicode repertoire (platform 0 encoding 4, platform 3 encoding 10).
fn build_encoding_records(mappings: &[(u32, u32)]) -> Vec<EncodingRecord> {
    let mut records = Vec::new();

    if let Some(cmap4) = build_format4(mappings) {
        records.push(EncodingRecord::new(
            WritePlatformId::Unicode,
            3, // Unicode BMP
            WriteCmapSubtable::Format4(cmap4.clone()),
        ));
        records.push(EncodingRecord::new(
            WritePlatformId::Windows,
            1, // Windows Unicode BMP
            WriteCmapSubtable::Format4(cmap4),
        ));
    }

    let groups = build_sequential_groups(mappings);
    let cmap12 = Cmap12 { language: 0, groups };
    records.push(EncodingRecord::new(
        WritePlatformId::Unicode,
        4, // Unicode full repertoire
        WriteCmapSubtable::Format12(cmap12.clone()),
    ));
    records.push(EncodingRecord::new(
        WritePlatformId::Windows,
        10, // Windows full repertoire
        WriteCmapSubtable::Format12(cmap12),
    ));

    // The spec requires encoding records to be sorted by (platform ID, encoding ID);
    // some Windows cmap parsers rely on this for lookup and treat an unsorted table
    // as malformed.
    records.sort();
    records
}

/// Build a format 4 subtable covering the BMP-range (<= 0xFFFF) prefix of `mappings`.
///
/// Returns `None` if no codepoints are in the BMP. Every segment uses the explicit
/// glyph-id-array encoding rather than `idDelta`, so segment boundaries only need to
/// track contiguous codepoint runs (not contiguous glyph ids too). The per-char array
/// entry costs 2 bytes; for a font with tens of thousands of BMP characters that's tens
/// of KB against a multi-MB font, an acceptable trade for avoiding delta-overflow entirely.
fn build_format4(mappings: &[(u32, u32)]) -> Option<Cmap4> {
    // U+FFFF is excluded even though it's <= 0xFFFF: it collides with the char-code range
    // of the mandatory terminating segment appended below, and (being a Unicode
    // noncharacter) is never legitimately mapped by a real font anyway.
    let bmp: Vec<(u16, u16)> = mappings
        .iter()
        .take_while(|(cp, _)| *cp <= 0xFFFF)
        .filter(|(cp, _)| *cp != 0xFFFF)
        .map(|&(cp, gid)| (cp as u16, gid as u16))
        .collect();

    if bmp.is_empty() {
        return None;
    }

    // Segment boundaries: contiguous runs of consecutive codepoints.
    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut seg_start = 0;
    for i in 1..bmp.len() {
        if bmp[i].0 != bmp[i - 1].0 + 1 {
            segments.push((seg_start, i - 1));
            seg_start = i;
        }
    }
    segments.push((seg_start, bmp.len() - 1));

    // Plus the mandatory terminating segment (0xFFFF, 0xFFFF, idDelta=1).
    let n_segments = segments.len() + 1;

    let mut start_code = Vec::with_capacity(n_segments);
    let mut end_code = Vec::with_capacity(n_segments);
    let mut id_delta = Vec::with_capacity(n_segments);
    let mut id_range_offsets = Vec::with_capacity(n_segments);
    let mut glyph_id_array = Vec::new();

    for (i, &(start_ix, end_ix)) in segments.iter().enumerate() {
        start_code.push(bmp[start_ix].0);
        end_code.push(bmp[end_ix].0);
        id_delta.push(0i16);

        // Byte offset from this segment's own idRangeOffset slot to its glyph ids,
        // mirroring the sfnt memory layout (remaining idRangeOffset entries, then
        // the glyph ids already emitted by earlier segments).
        let n_following_segments = n_segments - i;
        let id_range_offset = (n_following_segments + glyph_id_array.len()) * 2;
        id_range_offsets.push(id_range_offset as u16);

        glyph_id_array.extend(bmp[start_ix..=end_ix].iter().map(|(_, gid)| *gid));
    }

    // Mandatory terminating segment.
    start_code.push(0xFFFF);
    end_code.push(0xFFFF);
    id_delta.push(1);
    id_range_offsets.push(0);

    Some(Cmap4::new(0, end_code, start_code, id_delta, id_range_offsets, glyph_id_array))
}

/// Build sequential map groups from sorted (codepoint, glyph_id) pairs.
///
/// Groups consecutive codepoints that map to consecutive glyph IDs, as required by
/// format 12's `SequentialMapGroup` (glyphId = startGlyphID + (charCode - startCharCode)).
fn build_sequential_groups(mappings: &[(u32, u32)]) -> Vec<SequentialMapGroup> {
    if mappings.is_empty() {
        return Vec::new();
    }

    let mut groups = Vec::new();
    let mut group_start_cp = mappings[0].0;
    let mut group_start_gid = mappings[0].1;
    let mut prev_cp = group_start_cp;
    let mut prev_gid = group_start_gid;

    for &(cp, gid) in &mappings[1..] {
        // Check if this continues the current group (consecutive codepoint AND glyph ID)
        if cp == prev_cp + 1 && gid == prev_gid + 1 {
            prev_cp = cp;
            prev_gid = gid;
        } else {
            // End the current group and start a new one
            groups.push(SequentialMapGroup::new(group_start_cp, prev_cp, group_start_gid));
            group_start_cp = cp;
            group_start_gid = gid;
            prev_cp = cp;
            prev_gid = gid;
        }
    }

    // Don't forget the last group
    groups.push(SequentialMapGroup::new(group_start_cp, prev_cp, group_start_gid));

    groups
}

fn find_best_subtable<'a>(cmap: &'a ReadCmap<'a>) -> Option<CmapSubtable<'a>> {
    // Priority: Format 12 (full Unicode) > Format 4 (BMP) > others
    let records = cmap.encoding_records();

    // Try to find format 12 first (Unicode full)
    for record in records {
        if (record.platform_id() == PlatformId::Unicode
            || (record.platform_id() == PlatformId::Windows && record.encoding_id() == 10))
            && let Ok(subtable) = record.subtable(cmap.offset_data())
            && matches!(subtable, CmapSubtable::Format12(_))
        {
            return Some(subtable);
        }
    }

    // Fall back to format 4 (BMP)
    for record in records {
        if (record.platform_id() == PlatformId::Unicode
            || (record.platform_id() == PlatformId::Windows && record.encoding_id() == 1))
            && let Ok(subtable) = record.subtable(cmap.offset_data())
            && matches!(subtable, CmapSubtable::Format4(_))
        {
            return Some(subtable);
        }
    }

    // Take any subtable
    records.iter().find_map(|r| r.subtable(cmap.offset_data()).ok())
}

fn iter_cmap_subtable(subtable: &CmapSubtable) -> Vec<(Codepoint, GlyphId)> {
    let mut mappings = Vec::new();

    match subtable {
        CmapSubtable::Format4(f4) => {
            // Iterate through segments
            let end_codes = f4.end_code();
            let start_codes = f4.start_code();
            let id_deltas = f4.id_delta();
            let id_range_offsets = f4.id_range_offsets();
            let glyph_id_array = f4.glyph_id_array();

            let seg_count = f4.seg_count_x2() as usize / 2;
            for seg in 0..seg_count {
                let end_code = end_codes.get(seg).map_or(0xFFFF, BigEndian::get);
                let start_code = start_codes.get(seg).map_or(0, BigEndian::get);
                let id_delta = id_deltas.get(seg).map_or(0, BigEndian::get);
                let id_range_offset = id_range_offsets.get(seg).map_or(0, BigEndian::get);

                if start_code == 0xFFFF {
                    continue;
                }

                for cp in start_code..=end_code {
                    let gid = if id_range_offset == 0 {
                        ((i32::from(cp) + i32::from(id_delta)) & 0xFFFF) as u16
                    } else {
                        let glyph_idx = (id_range_offset as usize / 2) + (cp - start_code) as usize
                            - (seg_count - seg);
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
                        mappings.push((Codepoint::new(u32::from(cp)), GlyphId::new(gid)));
                    }
                }
            }
        }
        CmapSubtable::Format12(f12) => {
            for group in f12.groups() {
                let start = group.start_char_code();
                let end = group.end_char_code();
                for (gid, cp) in (group.start_glyph_id()..).zip(start..=end) {
                    if gid != 0 {
                        mappings.push((Codepoint::new(cp), GlyphId::new(gid as u16)));
                    }
                }
            }
        }
        CmapSubtable::Format6(f6) => {
            let first = u32::from(f6.first_code());
            for (i, gid) in f6.glyph_id_array().iter().enumerate() {
                let gid = gid.get();
                if gid != 0 {
                    mappings.push((Codepoint::new(first + i as u32), GlyphId::new(gid)));
                }
            }
        }
        _ => {}
    }

    mappings
}
