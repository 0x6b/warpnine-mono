//! GSUB/GPOS layout table merging

use std::{collections::HashMap, result};

use font_types::{BigEndian, GlyphId16};
use read_fonts::{
    TableProvider, tables,
    tables::{gpos::PositionSubtables, gsub::SubstitutionSubtables, layout},
    types::Tag,
};
use write_fonts::tables::{
    gpos::{
        AnchorTable, BaseArray, BaseRecord, Class1Record, Class2Record, ComponentRecord,
        CursivePosFormat1, EntryExitRecord, Gpos, LigatureArray, LigatureAttach, Mark2Array,
        Mark2Record, MarkArray, MarkBasePosFormat1, MarkLigPosFormat1, MarkMarkPosFormat1, PairPos,
        PairPosFormat1, PairPosFormat2, PairSet, PairValueRecord, PositionChainContext,
        PositionLookup, PositionLookupList, PositionSequenceContext, SinglePos, SinglePosFormat1,
        SinglePosFormat2, ValueRecord,
    },
    gsub::{
        Gsub, SingleSubst, SubstitutionChainContext, SubstitutionLookupList,
        SubstitutionSequenceContext,
    },
    layout::{
        ChainedClassSequenceRule, ChainedClassSequenceRuleSet, ChainedSequenceContext,
        ChainedSequenceContextFormat1, ChainedSequenceContextFormat2,
        ChainedSequenceContextFormat3, ChainedSequenceRule, ChainedSequenceRuleSet, ClassDef,
        ClassSequenceRule, ClassSequenceRuleSet, CoverageTable, Feature, FeatureList,
        FeatureRecord, LangSys, LangSysRecord, Lookup, LookupFlag, Script, ScriptList,
        ScriptRecord, SequenceContext, SequenceContextFormat1, SequenceContextFormat2,
        SequenceContextFormat3, SequenceLookupRecord, SequenceRule, SequenceRuleSet,
    },
};

use crate::{
    Result,
    context::{GidRemap, GlyphOrder, MergeContext},
    convert::{MarkArrayExt, ToWrite},
    tables::{
        cmap::DuplicateGlyphInfo,
        layout_types::{
            FeatureIndex, LangTag, LookupIndex, MergedFeatureList, ScriptLangFeatureMap, ScriptTag,
        },
    },
};

/// Merge GSUB tables from multiple fonts
pub fn merge_gsub(ctx: &MergeContext) -> Result<Option<Gsub>> {
    let fonts = ctx.fonts();
    let has_gsub = fonts.iter().any(|f| f.gsub().is_ok());
    if !has_gsub {
        return Ok(None);
    }

    let mut scripts = ScriptLangFeatureMap::new();
    let mut features = MergedFeatureList::new();
    let mut lookups: Vec<write_fonts::tables::gsub::SubstitutionLookup> = Vec::new();

    for (_font_idx, font, remap) in ctx.fonts_with_remap() {
        let Ok(gsub) = font.gsub() else {
            continue;
        };

        let lookup_offset = LookupIndex::new(lookups.len() as u16);
        if let Ok(lookup_list) = gsub.lookup_list() {
            for lookup_idx in 0..lookup_list.lookup_count() {
                if let Ok(lookup) = lookup_list.lookups().get(lookup_idx as usize)
                    && let Some(converted) = convert_gsub_lookup(&lookup, remap, lookup_offset)
                {
                    lookups.push(converted);
                }
            }
        }

        let feature_offset = features.len();
        if let Ok(feature_list) = gsub.feature_list() {
            let records = feature_list.feature_records();
            for i in 0..feature_list.feature_count() as usize {
                if let Some(record) = records.get(i)
                    && let Ok(feature) = record.feature(feature_list.offset_data())
                {
                    let tag = record.feature_tag();
                    let lookup_indices: Vec<LookupIndex> = feature
                        .lookup_list_indices()
                        .iter()
                        .map(|idx| LookupIndex::new(idx.get() + lookup_offset.as_u16()))
                        .collect();
                    features.add(tag, lookup_indices);
                }
            }
        }

        if let Ok(script_list) = gsub.script_list() {
            collect_scripts_typed(&script_list, &mut scripts, feature_offset);
        }
    }

    let duplicate_info = ctx.duplicate_info();
    if duplicate_info.per_font.iter().any(|m| !m.is_empty()) {
        add_locl_lookups_typed(
            &mut lookups,
            &mut features,
            &mut scripts,
            duplicate_info,
            ctx.glyph_order(),
        );
    }

    let gsub = build_gsub(scripts.into_raw(), features.into_raw(), lookups)?;

    Ok(Some(gsub))
}

/// Convert a read-fonts GSUB lookup to write-fonts format with GID remapping
fn convert_gsub_lookup(
    lookup: &read_fonts::tables::gsub::SubstitutionLookup,
    gid_remap: &GidRemap,
    lookup_offset: LookupIndex,
) -> Option<write_fonts::tables::gsub::SubstitutionLookup> {
    let _lookup_offset = lookup_offset.as_u16();
    use write_fonts::tables::gsub::{
        AlternateSet, AlternateSubstFormat1, Ligature, LigatureSet, LigatureSubstFormat1,
        MultipleSubstFormat1, ReverseChainSingleSubstFormat1, Sequence, SingleSubst,
        SubstitutionLookup as WriteLookup,
    };

    // Dispatch via `subtables()` rather than the outer `SubstitutionLookup`
    // enum so that LookupType 7 (Extension) is auto-unwrapped to its inner
    // type. Matching on the outer enum would force us to either drop
    // extensions or hand-roll the unwrap; the read-fonts API already handles
    // this cleanly.
    let flag = lookup.lookup_flag();
    let read_subs = lookup.subtables().ok()?;
    match read_subs {
        SubstitutionSubtables::Single(read_subs) => {
            let mut subtables = Vec::new();
            for subtable in read_subs.iter().filter_map(result::Result::ok) {
                match subtable {
                    tables::gsub::SingleSubst::Format1(f1) => {
                        if let Ok(coverage) = f1.coverage() {
                            let delta = f1.delta_glyph_id();
                            // Format1 uses delta, need to convert each glyph
                            let mut glyph_array = Vec::new();
                            let mut subst_glyphs = Vec::new();
                            for gid in coverage.iter() {
                                let old_gid = gid.to_u32() as u16;
                                let subst_old =
                                    ((i32::from(old_gid) + i32::from(delta)) & 0xFFFF) as u16;
                                // Only include this substitution if BOTH source and target can be
                                // remapped
                                if let (Some(new_gid), Some(subst_new)) =
                                    (gid_remap.get_u16(old_gid), gid_remap.get_u16(subst_old))
                                {
                                    glyph_array.push(GlyphId16::new(new_gid));
                                    subst_glyphs.push(GlyphId16::new(subst_new));
                                }
                            }
                            if !glyph_array.is_empty() {
                                let cov = CoverageTable::format_1(glyph_array);
                                subtables.push(SingleSubst::format_2(cov, subst_glyphs));
                            }
                        }
                    }
                    tables::gsub::SingleSubst::Format2(f2) => {
                        if let Ok(coverage) = f2.coverage() {
                            let mut glyph_array = Vec::new();
                            let mut subst_glyphs = Vec::new();
                            for (gid, subst_gid) in coverage.iter().zip(f2.substitute_glyph_ids()) {
                                let old_gid = gid.to_u32() as u16;
                                let subst_old = subst_gid.get().to_u32() as u16;
                                // Only include this substitution if BOTH source and target can be
                                // remapped
                                if let (Some(new_gid), Some(subst_new)) =
                                    (gid_remap.get_u16(old_gid), gid_remap.get_u16(subst_old))
                                {
                                    glyph_array.push(GlyphId16::new(new_gid));
                                    subst_glyphs.push(GlyphId16::new(subst_new));
                                }
                            }
                            if !glyph_array.is_empty() {
                                let cov = CoverageTable::format_1(glyph_array);
                                subtables.push(SingleSubst::format_2(cov, subst_glyphs));
                            }
                        }
                    }
                }
            }
            if subtables.is_empty() {
                return None;
            }
            Some(WriteLookup::Single(Lookup::new(flag, subtables)))
        }
        SubstitutionSubtables::Multiple(read_subs) => {
            let mut subtables = Vec::new();
            for subtable in read_subs.iter().filter_map(result::Result::ok) {
                if let Ok(coverage) = subtable.coverage() {
                    let mut glyph_array = Vec::new();
                    let mut sequences = Vec::new();
                    for (gid, seq) in coverage.iter().zip(subtable.sequences().iter()) {
                        let old_gid = gid.to_u32() as u16;
                        if let (Some(new_gid), Ok(seq)) = (gid_remap.get_u16(old_gid), seq) {
                            // Remap all substitute glyphs; skip this entry if any can't be remapped
                            let subst_glyphs: Option<Vec<GlyphId16>> = seq
                                .substitute_glyph_ids()
                                .iter()
                                .map(|g| {
                                    let old = g.get().to_u32() as u16;
                                    gid_remap.get_u16(old).map(GlyphId16::new)
                                })
                                .collect();
                            if let Some(subst_glyphs) = subst_glyphs {
                                glyph_array.push(GlyphId16::new(new_gid));
                                sequences.push(Sequence::new(subst_glyphs));
                            }
                        }
                    }
                    if !glyph_array.is_empty() {
                        let cov = CoverageTable::format_1(glyph_array);
                        subtables.push(MultipleSubstFormat1::new(cov, sequences));
                    }
                }
            }
            if subtables.is_empty() {
                return None;
            }
            Some(WriteLookup::Multiple(Lookup::new(flag, subtables)))
        }
        SubstitutionSubtables::Alternate(read_subs) => {
            let mut subtables = Vec::new();
            for subtable in read_subs.iter().filter_map(result::Result::ok) {
                if let Ok(coverage) = subtable.coverage() {
                    let mut glyph_array = Vec::new();
                    let mut alt_sets = Vec::new();
                    for (gid, alt) in coverage.iter().zip(subtable.alternate_sets().iter()) {
                        let old_gid = gid.to_u32() as u16;
                        if let (Some(new_gid), Ok(alt)) = (gid_remap.get_u16(old_gid), alt) {
                            // Remap all alternate glyphs; skip this entry if any can't be remapped
                            let alt_glyphs: Option<Vec<GlyphId16>> = alt
                                .alternate_glyph_ids()
                                .iter()
                                .map(|g| {
                                    let old = g.get().to_u32() as u16;
                                    gid_remap.get_u16(old).map(GlyphId16::new)
                                })
                                .collect();
                            if let Some(alt_glyphs) = alt_glyphs {
                                glyph_array.push(GlyphId16::new(new_gid));
                                alt_sets.push(AlternateSet::new(alt_glyphs));
                            }
                        }
                    }
                    if !glyph_array.is_empty() {
                        let cov = CoverageTable::format_1(glyph_array);
                        subtables.push(AlternateSubstFormat1::new(cov, alt_sets));
                    }
                }
            }
            if subtables.is_empty() {
                return None;
            }
            Some(WriteLookup::Alternate(Lookup::new(flag, subtables)))
        }
        SubstitutionSubtables::Ligature(read_subs) => {
            let mut subtables = Vec::new();
            for subtable in read_subs.iter().filter_map(result::Result::ok) {
                if let Ok(coverage) = subtable.coverage() {
                    let mut glyph_array = Vec::new();
                    let mut lig_sets = Vec::new();
                    for (gid, lig_set) in coverage.iter().zip(subtable.ligature_sets().iter()) {
                        let old_gid = gid.to_u32() as u16;
                        if let (Some(new_gid), Ok(lig_set)) = (gid_remap.get_u16(old_gid), lig_set)
                        {
                            // Filter ligatures to only include those where all GIDs can be remapped
                            let ligatures: Vec<Ligature> = lig_set
                                .ligatures()
                                .iter()
                                .filter_map(result::Result::ok)
                                .filter_map(|lig| {
                                    let lig_glyph_old = lig.ligature_glyph().to_u32() as u16;
                                    let lig_glyph = gid_remap.get_u16(lig_glyph_old)?;
                                    let components: Option<Vec<GlyphId16>> = lig
                                        .component_glyph_ids()
                                        .iter()
                                        .map(|g| {
                                            let old = g.get().to_u32() as u16;
                                            gid_remap.get_u16(old).map(GlyphId16::new)
                                        })
                                        .collect();
                                    components.map(|c| Ligature::new(GlyphId16::new(lig_glyph), c))
                                })
                                .collect();
                            if !ligatures.is_empty() {
                                glyph_array.push(GlyphId16::new(new_gid));
                                lig_sets.push(LigatureSet::new(ligatures));
                            }
                        }
                    }
                    if !glyph_array.is_empty() {
                        let cov = CoverageTable::format_1(glyph_array);
                        subtables.push(LigatureSubstFormat1::new(cov, lig_sets));
                    }
                }
            }
            if subtables.is_empty() {
                return None;
            }
            Some(WriteLookup::Ligature(Lookup::new(flag, subtables)))
        }
        SubstitutionSubtables::Contextual(read_subs) => {
            let mut subtables = Vec::new();
            for subtable in read_subs.iter().filter_map(result::Result::ok) {
                if let Some(converted) =
                    convert_gsub_sequence_context(&subtable, gid_remap, _lookup_offset)
                {
                    subtables.push(converted);
                }
            }
            if subtables.is_empty() {
                return None;
            }
            Some(WriteLookup::Contextual(Lookup::new(flag, subtables)))
        }
        SubstitutionSubtables::ChainContextual(read_subs) => {
            let mut subtables = Vec::new();
            for subtable in read_subs.iter().filter_map(result::Result::ok) {
                if let Some(converted) =
                    convert_gsub_chained_context(&subtable, gid_remap, _lookup_offset)
                {
                    subtables.push(converted);
                }
            }
            if subtables.is_empty() {
                return None;
            }
            Some(WriteLookup::ChainContextual(Lookup::new(flag, subtables)))
        }
        SubstitutionSubtables::Reverse(read_subs) => {
            let mut subtables = Vec::new();
            for subtable in read_subs.iter().filter_map(result::Result::ok) {
                if let Ok(coverage) = subtable.coverage() {
                    let remapped_cov = remap_coverage(&coverage, gid_remap);

                    let backtrack: Vec<CoverageTable> = subtable
                        .backtrack_coverages()
                        .iter()
                        .filter_map(result::Result::ok)
                        .map(|c| remap_coverage(&c, gid_remap))
                        .collect();

                    let lookahead: Vec<CoverageTable> = subtable
                        .lookahead_coverages()
                        .iter()
                        .filter_map(result::Result::ok)
                        .map(|c| remap_coverage(&c, gid_remap))
                        .collect();

                    // Remap substitute glyphs; skip any that can't be remapped
                    let subst_glyphs: Option<Vec<GlyphId16>> = subtable
                        .substitute_glyph_ids()
                        .iter()
                        .map(|g| {
                            let old = g.get().to_u32() as u16;
                            gid_remap.get_u16(old).map(GlyphId16::new)
                        })
                        .collect();

                    if let Some(subst_glyphs) = subst_glyphs {
                        subtables.push(ReverseChainSingleSubstFormat1::new(
                            remapped_cov,
                            backtrack,
                            lookahead,
                            subst_glyphs,
                        ));
                    }
                }
            }
            if subtables.is_empty() {
                return None;
            }
            Some(WriteLookup::Reverse(Lookup::new(flag, subtables)))
        }
        SubstitutionSubtables::EmptyExtension => None,
    }
}

/// Merge GPOS tables from multiple fonts
pub fn merge_gpos(ctx: &MergeContext) -> Result<Option<Gpos>> {
    let fonts = ctx.fonts();
    let has_gpos = fonts.iter().any(|f| f.gpos().is_ok());
    if !has_gpos {
        return Ok(None);
    }

    let mut scripts = ScriptLangFeatureMap::new();
    let mut features = MergedFeatureList::new();
    let mut lookups: Vec<PositionLookup> = Vec::new();

    for (_font_idx, font, remap) in ctx.fonts_with_remap() {
        let Ok(gpos) = font.gpos() else {
            continue;
        };

        let lookup_offset = LookupIndex::new(lookups.len() as u16);
        if let Ok(lookup_list) = gpos.lookup_list() {
            for lookup_idx in 0..lookup_list.lookup_count() {
                if let Ok(lookup) = lookup_list.lookups().get(lookup_idx as usize)
                    && let Some(converted) = convert_gpos_lookup(&lookup, remap, lookup_offset)
                {
                    lookups.push(converted);
                }
            }
        }

        let feature_offset = features.len();
        if let Ok(feature_list) = gpos.feature_list() {
            let records = feature_list.feature_records();
            for i in 0..feature_list.feature_count() as usize {
                if let Some(record) = records.get(i)
                    && let Ok(feature) = record.feature(feature_list.offset_data())
                {
                    let tag = record.feature_tag();
                    let lookup_indices: Vec<LookupIndex> = feature
                        .lookup_list_indices()
                        .iter()
                        .map(|idx| LookupIndex::new(idx.get() + lookup_offset.as_u16()))
                        .collect();
                    features.add(tag, lookup_indices);
                }
            }
        }

        if let Ok(script_list) = gpos.script_list() {
            collect_scripts_typed(&script_list, &mut scripts, feature_offset);
        }
    }

    if lookups.is_empty() && features.is_empty() {
        return Ok(None);
    }

    let gpos = build_gpos(scripts.into_raw(), features.into_raw(), lookups)?;

    Ok(Some(gpos))
}

/// Convert a read-fonts GPOS lookup to write-fonts format with GID remapping
fn convert_gpos_lookup(
    lookup: &tables::gpos::PositionLookup,
    gid_remap: &GidRemap,
    lookup_offset: LookupIndex,
) -> Option<PositionLookup> {
    let _lookup_offset = lookup_offset.as_u16();
    // See the matching comment in `convert_gsub_lookup`: dispatch via
    // `subtables()` so LookupType 9 (Extension) is auto-unwrapped.
    let flag = lookup.lookup_flag();
    let read_subs = lookup.subtables().ok()?;
    match read_subs {
        PositionSubtables::Single(read_subs) => {
            let mut subtables = Vec::new();
            for subtable in read_subs.iter().filter_map(result::Result::ok) {
                match subtable {
                    tables::gpos::SinglePos::Format1(f1) => {
                        if let Ok(coverage) = f1.coverage() {
                            let remapped_cov = remap_coverage(&coverage, gid_remap);
                            let value_record = f1.value_record().to_write();
                            subtables.push(SinglePos::Format1(SinglePosFormat1::new(
                                remapped_cov,
                                value_record,
                            )));
                        }
                    }
                    tables::gpos::SinglePos::Format2(f2) => {
                        if let Ok(coverage) = f2.coverage() {
                            let remapped_cov = remap_coverage(&coverage, gid_remap);
                            let value_records: Vec<ValueRecord> = f2
                                .value_records()
                                .iter()
                                .filter_map(result::Result::ok)
                                .map(|vr| vr.to_write())
                                .collect();
                            subtables.push(SinglePos::Format2(SinglePosFormat2::new(
                                remapped_cov,
                                value_records,
                            )));
                        }
                    }
                }
            }
            if subtables.is_empty() {
                return None;
            }
            Some(PositionLookup::Single(Lookup::new(flag, subtables)))
        }
        PositionSubtables::Pair(read_subs) => {
            let mut subtables = Vec::new();
            for subtable in read_subs.iter().filter_map(result::Result::ok) {
                match subtable {
                    tables::gpos::PairPos::Format1(f1) => {
                        if let Ok(coverage) = f1.coverage() {
                            let remapped_cov = remap_coverage(&coverage, gid_remap);
                            let pair_sets: Vec<PairSet> = f1
                                .pair_sets()
                                .iter()
                                .filter_map(result::Result::ok)
                                .map(|ps| {
                                    // Filter to only include records where second glyph can be
                                    // remapped
                                    let records: Vec<PairValueRecord> = ps
                                        .pair_value_records()
                                        .iter()
                                        .filter_map(result::Result::ok)
                                        .filter_map(|pvr| {
                                            let second_old = pvr.second_glyph().to_u32() as u16;
                                            let second_new = gid_remap.get_u16(second_old)?;
                                            Some(PairValueRecord::new(
                                                GlyphId16::new(second_new),
                                                pvr.value_record1().to_write(),
                                                pvr.value_record2().to_write(),
                                            ))
                                        })
                                        .collect();
                                    PairSet::new(records)
                                })
                                .collect();
                            subtables.push(PairPos::Format1(PairPosFormat1::new(
                                remapped_cov,
                                pair_sets,
                            )));
                        }
                    }
                    tables::gpos::PairPos::Format2(f2) => {
                        if let Ok(coverage) = f2.coverage() {
                            let remapped_cov = remap_coverage(&coverage, gid_remap);
                            let class_def1 = if let Ok(cd) = f2.class_def1() {
                                remap_class_def(&cd, gid_remap)
                            } else {
                                ClassDef::default()
                            };
                            let class_def2 = if let Ok(cd) = f2.class_def2() {
                                remap_class_def(&cd, gid_remap)
                            } else {
                                ClassDef::default()
                            };
                            let class1_records: Vec<Class1Record> = f2
                                .class1_records()
                                .iter()
                                .filter_map(result::Result::ok)
                                .map(|c1r| {
                                    let class2_records: Vec<Class2Record> = c1r
                                        .class2_records()
                                        .iter()
                                        .filter_map(result::Result::ok)
                                        .map(|c2r| {
                                            Class2Record::new(
                                                c2r.value_record1().to_write(),
                                                c2r.value_record2().to_write(),
                                            )
                                        })
                                        .collect();
                                    Class1Record::new(class2_records)
                                })
                                .collect();
                            subtables.push(PairPos::Format2(PairPosFormat2::new(
                                remapped_cov,
                                class_def1,
                                class_def2,
                                class1_records,
                            )));
                        }
                    }
                }
            }
            if subtables.is_empty() {
                return None;
            }
            Some(PositionLookup::Pair(Lookup::new(flag, subtables)))
        }
        PositionSubtables::Cursive(read_subs) => {
            let mut subtables = Vec::new();
            for subtable in read_subs.iter().filter_map(result::Result::ok) {
                if let Ok(coverage) = subtable.coverage() {
                    let remapped_cov = remap_coverage(&coverage, gid_remap);
                    let entry_exit_records: Vec<EntryExitRecord> = subtable
                        .entry_exit_record()
                        .iter()
                        .map(|eer| {
                            let entry = eer
                                .entry_anchor(subtable.offset_data())
                                .and_then(result::Result::ok)
                                .map(|a| a.to_write());
                            let exit = eer
                                .exit_anchor(subtable.offset_data())
                                .and_then(result::Result::ok)
                                .map(|a| a.to_write());
                            EntryExitRecord::new(entry, exit)
                        })
                        .collect();
                    subtables.push(CursivePosFormat1::new(remapped_cov, entry_exit_records));
                }
            }
            if subtables.is_empty() {
                return None;
            }
            Some(PositionLookup::Cursive(Lookup::new(flag, subtables)))
        }
        PositionSubtables::MarkToBase(read_subs) => {
            let mut subtables = Vec::new();
            for subtable in read_subs.iter().filter_map(result::Result::ok) {
                if let (Ok(mark_cov), Ok(base_cov)) =
                    (subtable.mark_coverage(), subtable.base_coverage())
                {
                    let remapped_mark_cov = remap_coverage(&mark_cov, gid_remap);
                    let remapped_base_cov = remap_coverage(&base_cov, gid_remap);

                    let mark_array = if let Ok(ma) = subtable.mark_array() {
                        ma.to_write()
                    } else {
                        MarkArray::new(vec![])
                    };

                    let base_array = if let Ok(ba) = subtable.base_array() {
                        let base_records: Vec<BaseRecord> = ba
                            .base_records()
                            .iter()
                            .filter_map(result::Result::ok)
                            .map(|br| {
                                let anchors: Vec<Option<AnchorTable>> = br
                                    .base_anchors(ba.offset_data())
                                    .iter()
                                    .map(|a| a.and_then(result::Result::ok).map(|a| a.to_write()))
                                    .collect();
                                BaseRecord::new(anchors)
                            })
                            .collect();
                        BaseArray::new(base_records)
                    } else {
                        BaseArray::new(vec![])
                    };

                    subtables.push(MarkBasePosFormat1::new(
                        remapped_mark_cov,
                        remapped_base_cov,
                        mark_array,
                        base_array,
                    ));
                }
            }
            if subtables.is_empty() {
                return None;
            }
            Some(PositionLookup::MarkToBase(Lookup::new(flag, subtables)))
        }
        PositionSubtables::MarkToLig(read_subs) => {
            let mut subtables = Vec::new();
            for subtable in read_subs.iter().filter_map(result::Result::ok) {
                if let (Ok(mark_cov), Ok(lig_cov)) =
                    (subtable.mark_coverage(), subtable.ligature_coverage())
                {
                    let remapped_mark_cov = remap_coverage(&mark_cov, gid_remap);
                    let remapped_lig_cov = remap_coverage(&lig_cov, gid_remap);

                    let mark_array = if let Ok(ma) = subtable.mark_array() {
                        ma.to_write()
                    } else {
                        MarkArray::new(vec![])
                    };

                    let ligature_array = if let Ok(la) = subtable.ligature_array() {
                        let lig_attaches: Vec<LigatureAttach> = la
                            .ligature_attaches()
                            .iter()
                            .filter_map(result::Result::ok)
                            .map(|la| {
                                let component_records: Vec<ComponentRecord> = la
                                    .component_records()
                                    .iter()
                                    .filter_map(result::Result::ok)
                                    .map(|cr| {
                                        let anchors: Vec<Option<AnchorTable>> = cr
                                            .ligature_anchors(la.offset_data())
                                            .iter()
                                            .map(|a| {
                                                a.and_then(result::Result::ok).map(|a| a.to_write())
                                            })
                                            .collect();
                                        ComponentRecord::new(anchors)
                                    })
                                    .collect();
                                LigatureAttach::new(component_records)
                            })
                            .collect();
                        LigatureArray::new(lig_attaches)
                    } else {
                        LigatureArray::new(vec![])
                    };

                    subtables.push(MarkLigPosFormat1::new(
                        remapped_mark_cov,
                        remapped_lig_cov,
                        mark_array,
                        ligature_array,
                    ));
                }
            }
            if subtables.is_empty() {
                return None;
            }
            Some(PositionLookup::MarkToLig(Lookup::new(flag, subtables)))
        }
        PositionSubtables::MarkToMark(read_subs) => {
            let mut subtables = Vec::new();
            for subtable in read_subs.iter().filter_map(result::Result::ok) {
                if let (Ok(mark1_cov), Ok(mark2_cov)) =
                    (subtable.mark1_coverage(), subtable.mark2_coverage())
                {
                    let remapped_mark1_cov = remap_coverage(&mark1_cov, gid_remap);
                    let remapped_mark2_cov = remap_coverage(&mark2_cov, gid_remap);

                    let mark1_array = if let Ok(ma) = subtable.mark1_array() {
                        ma.to_write()
                    } else {
                        MarkArray::new(vec![])
                    };

                    let mark2_array = if let Ok(m2a) = subtable.mark2_array() {
                        let mark2_records: Vec<Mark2Record> = m2a
                            .mark2_records()
                            .iter()
                            .filter_map(result::Result::ok)
                            .map(|m2r| {
                                let anchors: Vec<Option<AnchorTable>> = m2r
                                    .mark2_anchors(m2a.offset_data())
                                    .iter()
                                    .map(|a| a.and_then(result::Result::ok).map(|a| a.to_write()))
                                    .collect();
                                Mark2Record::new(anchors)
                            })
                            .collect();
                        Mark2Array::new(mark2_records)
                    } else {
                        Mark2Array::new(vec![])
                    };

                    subtables.push(MarkMarkPosFormat1::new(
                        remapped_mark1_cov,
                        remapped_mark2_cov,
                        mark1_array,
                        mark2_array,
                    ));
                }
            }
            if subtables.is_empty() {
                return None;
            }
            Some(PositionLookup::MarkToMark(Lookup::new(flag, subtables)))
        }
        PositionSubtables::Contextual(read_subs) => {
            let mut subtables = Vec::new();
            for subtable in read_subs.iter().filter_map(result::Result::ok) {
                if let Some(converted) =
                    convert_gpos_sequence_context(&subtable, gid_remap, _lookup_offset)
                {
                    subtables.push(converted);
                }
            }
            if subtables.is_empty() {
                return None;
            }
            Some(PositionLookup::Contextual(Lookup::new(flag, subtables)))
        }
        PositionSubtables::ChainContextual(read_subs) => {
            let mut subtables = Vec::new();
            for subtable in read_subs.iter().filter_map(result::Result::ok) {
                if let Some(converted) =
                    convert_gpos_chained_context(&subtable, gid_remap, _lookup_offset)
                {
                    subtables.push(converted);
                }
            }
            if subtables.is_empty() {
                return None;
            }
            Some(PositionLookup::ChainContextual(Lookup::new(flag, subtables)))
        }
        PositionSubtables::EmptyExtension => None,
    }
}

/// Remap coverage table glyphs
fn remap_coverage(coverage: &layout::CoverageTable, gid_remap: &GidRemap) -> CoverageTable {
    let glyphs: Vec<GlyphId16> = coverage
        .iter()
        .filter_map(|gid| {
            let old = gid.to_u32() as u16;
            gid_remap.get_u16(old).map(GlyphId16::new)
        })
        .collect();
    CoverageTable::format_1(glyphs)
}

/// Remap class definition
fn remap_class_def(class_def: &layout::ClassDef, gid_remap: &GidRemap) -> ClassDef {
    let mappings: Vec<(GlyphId16, u16)> = class_def
        .iter()
        .filter_map(|(gid, class)| {
            let old = gid.to_u32() as u16;
            gid_remap.get_u16(old).map(|new| (GlyphId16::new(new), class))
        })
        .collect();
    ClassDef::from_iter(mappings)
}

/// Remap a glyph ID array, returning None if any glyph can't be remapped
fn remap_glyph_array(
    glyphs: &[BigEndian<GlyphId16>],
    gid_remap: &GidRemap,
) -> Option<Vec<GlyphId16>> {
    glyphs
        .iter()
        .map(|g| {
            let old = g.get().to_u32() as u16;
            gid_remap.get_u16(old).map(GlyphId16::new)
        })
        .collect()
}

/// Remap SequenceLookupRecords with lookup offset adjustment
fn remap_seq_lookup_records(
    records: &[layout::SequenceLookupRecord],
    lookup_offset: u16,
) -> Vec<SequenceLookupRecord> {
    records
        .iter()
        .map(|r| {
            SequenceLookupRecord::new(r.sequence_index(), r.lookup_list_index() + lookup_offset)
        })
        .collect()
}

/// Convert GSUB SequenceContext (contextual substitution type 5)
fn convert_gsub_sequence_context(
    subtable: &layout::SequenceContext,
    gid_remap: &GidRemap,
    lookup_offset: u16,
) -> Option<SubstitutionSequenceContext> {
    match subtable {
        layout::SequenceContext::Format1(f1) => {
            let coverage = f1.coverage().ok()?;
            let remapped_cov = remap_coverage(&coverage, gid_remap);

            let seq_rule_sets: Vec<Option<SequenceRuleSet>> = f1
                .seq_rule_sets()
                .iter()
                .map(|opt_result| {
                    opt_result.and_then(result::Result::ok).map(|srs| {
                        // Filter rules where all glyphs can be remapped
                        let seq_rules: Vec<SequenceRule> = srs
                            .seq_rules()
                            .iter()
                            .filter_map(result::Result::ok)
                            .filter_map(|rule| {
                                let input_seq =
                                    remap_glyph_array(rule.input_sequence(), gid_remap)?;
                                let seq_lookups = remap_seq_lookup_records(
                                    rule.seq_lookup_records(),
                                    lookup_offset,
                                );
                                Some(SequenceRule::new(input_seq, seq_lookups))
                            })
                            .collect();
                        SequenceRuleSet::new(seq_rules)
                    })
                })
                .collect();

            Some(SubstitutionSequenceContext::from(SequenceContext::Format1(
                SequenceContextFormat1::new(remapped_cov, seq_rule_sets),
            )))
        }
        layout::SequenceContext::Format2(f2) => {
            let coverage = f2.coverage().ok()?;
            let class_def = f2.class_def().ok()?;
            let remapped_cov = remap_coverage(&coverage, gid_remap);
            let remapped_class_def = remap_class_def(&class_def, gid_remap);

            let class_seq_rule_sets: Vec<Option<ClassSequenceRuleSet>> = f2
                .class_seq_rule_sets()
                .iter()
                .map(|opt_result| {
                    opt_result.and_then(result::Result::ok).map(|csrs| {
                        let class_seq_rules: Vec<ClassSequenceRule> = csrs
                            .class_seq_rules()
                            .iter()
                            .filter_map(result::Result::ok)
                            .map(|rule| {
                                let input_seq: Vec<u16> =
                                    rule.input_sequence().iter().map(BigEndian::get).collect();
                                let seq_lookups = remap_seq_lookup_records(
                                    rule.seq_lookup_records(),
                                    lookup_offset,
                                );
                                ClassSequenceRule::new(input_seq, seq_lookups)
                            })
                            .collect();
                        ClassSequenceRuleSet::new(class_seq_rules)
                    })
                })
                .collect();

            Some(SubstitutionSequenceContext::from(SequenceContext::Format2(
                SequenceContextFormat2::new(remapped_cov, remapped_class_def, class_seq_rule_sets),
            )))
        }
        layout::SequenceContext::Format3(f3) => {
            let coverages: Vec<CoverageTable> = f3
                .coverages()
                .iter()
                .filter_map(result::Result::ok)
                .map(|c| remap_coverage(&c, gid_remap))
                .collect();

            let seq_lookups = remap_seq_lookup_records(f3.seq_lookup_records(), lookup_offset);

            Some(SubstitutionSequenceContext::from(SequenceContext::Format3(
                SequenceContextFormat3::new(coverages, seq_lookups),
            )))
        }
    }
}

/// Convert GSUB ChainedSequenceContext (chained contextual substitution type 6)
fn convert_gsub_chained_context(
    subtable: &layout::ChainedSequenceContext,
    gid_remap: &GidRemap,
    lookup_offset: u16,
) -> Option<SubstitutionChainContext> {
    match subtable {
        layout::ChainedSequenceContext::Format1(f1) => {
            let coverage = f1.coverage().ok()?;
            let remapped_cov = remap_coverage(&coverage, gid_remap);

            let chained_seq_rule_sets: Vec<Option<ChainedSequenceRuleSet>> = f1
                .chained_seq_rule_sets()
                .iter()
                .map(|opt_result| {
                    opt_result.and_then(result::Result::ok).map(|csrs| {
                        // Filter rules where all glyphs can be remapped
                        let chained_seq_rules: Vec<ChainedSequenceRule> = csrs
                            .chained_seq_rules()
                            .iter()
                            .filter_map(result::Result::ok)
                            .filter_map(|rule| {
                                let backtrack =
                                    remap_glyph_array(rule.backtrack_sequence(), gid_remap)?;
                                let input = remap_glyph_array(rule.input_sequence(), gid_remap)?;
                                let lookahead =
                                    remap_glyph_array(rule.lookahead_sequence(), gid_remap)?;
                                let seq_lookups = remap_seq_lookup_records(
                                    rule.seq_lookup_records(),
                                    lookup_offset,
                                );
                                Some(ChainedSequenceRule::new(
                                    backtrack,
                                    input,
                                    lookahead,
                                    seq_lookups,
                                ))
                            })
                            .collect();
                        ChainedSequenceRuleSet::new(chained_seq_rules)
                    })
                })
                .collect();

            Some(SubstitutionChainContext::from(ChainedSequenceContext::Format1(
                ChainedSequenceContextFormat1::new(remapped_cov, chained_seq_rule_sets),
            )))
        }
        layout::ChainedSequenceContext::Format2(f2) => {
            let coverage = f2.coverage().ok()?;
            let backtrack_class_def = f2.backtrack_class_def().ok()?;
            let input_class_def = f2.input_class_def().ok()?;
            let lookahead_class_def = f2.lookahead_class_def().ok()?;

            let remapped_cov = remap_coverage(&coverage, gid_remap);
            let remapped_backtrack_cd = remap_class_def(&backtrack_class_def, gid_remap);
            let remapped_input_cd = remap_class_def(&input_class_def, gid_remap);
            let remapped_lookahead_cd = remap_class_def(&lookahead_class_def, gid_remap);

            let chained_class_seq_rule_sets: Vec<Option<ChainedClassSequenceRuleSet>> = f2
                .chained_class_seq_rule_sets()
                .iter()
                .map(|opt_result| {
                    opt_result.and_then(result::Result::ok).map(|ccsrs| {
                        let chained_class_seq_rules: Vec<ChainedClassSequenceRule> = ccsrs
                            .chained_class_seq_rules()
                            .iter()
                            .filter_map(result::Result::ok)
                            .map(|rule| {
                                let backtrack: Vec<u16> =
                                    rule.backtrack_sequence().iter().map(BigEndian::get).collect();
                                let input: Vec<u16> =
                                    rule.input_sequence().iter().map(BigEndian::get).collect();
                                let lookahead: Vec<u16> =
                                    rule.lookahead_sequence().iter().map(BigEndian::get).collect();
                                let seq_lookups = remap_seq_lookup_records(
                                    rule.seq_lookup_records(),
                                    lookup_offset,
                                );
                                ChainedClassSequenceRule::new(
                                    backtrack,
                                    input,
                                    lookahead,
                                    seq_lookups,
                                )
                            })
                            .collect();
                        ChainedClassSequenceRuleSet::new(chained_class_seq_rules)
                    })
                })
                .collect();

            Some(SubstitutionChainContext::from(ChainedSequenceContext::Format2(
                ChainedSequenceContextFormat2::new(
                    remapped_cov,
                    remapped_backtrack_cd,
                    remapped_input_cd,
                    remapped_lookahead_cd,
                    chained_class_seq_rule_sets,
                ),
            )))
        }
        layout::ChainedSequenceContext::Format3(f3) => {
            let backtrack_coverages: Vec<CoverageTable> = f3
                .backtrack_coverages()
                .iter()
                .filter_map(result::Result::ok)
                .map(|c| remap_coverage(&c, gid_remap))
                .collect();

            let input_coverages: Vec<CoverageTable> = f3
                .input_coverages()
                .iter()
                .filter_map(result::Result::ok)
                .map(|c| remap_coverage(&c, gid_remap))
                .collect();

            let lookahead_coverages: Vec<CoverageTable> = f3
                .lookahead_coverages()
                .iter()
                .filter_map(result::Result::ok)
                .map(|c| remap_coverage(&c, gid_remap))
                .collect();

            let seq_lookups = remap_seq_lookup_records(f3.seq_lookup_records(), lookup_offset);

            Some(SubstitutionChainContext::from(ChainedSequenceContext::Format3(
                ChainedSequenceContextFormat3::new(
                    backtrack_coverages,
                    input_coverages,
                    lookahead_coverages,
                    seq_lookups,
                ),
            )))
        }
    }
}

/// Convert GPOS SequenceContext (contextual positioning type 7)
fn convert_gpos_sequence_context(
    subtable: &layout::SequenceContext,
    gid_remap: &GidRemap,
    lookup_offset: u16,
) -> Option<PositionSequenceContext> {
    match subtable {
        layout::SequenceContext::Format1(f1) => {
            let coverage = f1.coverage().ok()?;
            let remapped_cov = remap_coverage(&coverage, gid_remap);

            let seq_rule_sets: Vec<Option<SequenceRuleSet>> = f1
                .seq_rule_sets()
                .iter()
                .map(|opt_result| {
                    opt_result.and_then(result::Result::ok).map(|srs| {
                        // Filter rules where all glyphs can be remapped
                        let seq_rules: Vec<SequenceRule> = srs
                            .seq_rules()
                            .iter()
                            .filter_map(result::Result::ok)
                            .filter_map(|rule| {
                                let input_seq =
                                    remap_glyph_array(rule.input_sequence(), gid_remap)?;
                                let seq_lookups = remap_seq_lookup_records(
                                    rule.seq_lookup_records(),
                                    lookup_offset,
                                );
                                Some(SequenceRule::new(input_seq, seq_lookups))
                            })
                            .collect();
                        SequenceRuleSet::new(seq_rules)
                    })
                })
                .collect();

            Some(PositionSequenceContext::from(SequenceContext::Format1(
                SequenceContextFormat1::new(remapped_cov, seq_rule_sets),
            )))
        }
        layout::SequenceContext::Format2(f2) => {
            let coverage = f2.coverage().ok()?;
            let class_def = f2.class_def().ok()?;
            let remapped_cov = remap_coverage(&coverage, gid_remap);
            let remapped_class_def = remap_class_def(&class_def, gid_remap);

            let class_seq_rule_sets: Vec<Option<ClassSequenceRuleSet>> = f2
                .class_seq_rule_sets()
                .iter()
                .map(|opt_result| {
                    opt_result.and_then(result::Result::ok).map(|csrs| {
                        let class_seq_rules: Vec<ClassSequenceRule> = csrs
                            .class_seq_rules()
                            .iter()
                            .filter_map(result::Result::ok)
                            .map(|rule| {
                                let input_seq: Vec<u16> =
                                    rule.input_sequence().iter().map(BigEndian::get).collect();
                                let seq_lookups = remap_seq_lookup_records(
                                    rule.seq_lookup_records(),
                                    lookup_offset,
                                );
                                ClassSequenceRule::new(input_seq, seq_lookups)
                            })
                            .collect();
                        ClassSequenceRuleSet::new(class_seq_rules)
                    })
                })
                .collect();

            Some(PositionSequenceContext::from(SequenceContext::Format2(
                SequenceContextFormat2::new(remapped_cov, remapped_class_def, class_seq_rule_sets),
            )))
        }
        layout::SequenceContext::Format3(f3) => {
            let coverages: Vec<CoverageTable> = f3
                .coverages()
                .iter()
                .filter_map(result::Result::ok)
                .map(|c| remap_coverage(&c, gid_remap))
                .collect();

            let seq_lookups = remap_seq_lookup_records(f3.seq_lookup_records(), lookup_offset);

            Some(PositionSequenceContext::from(SequenceContext::Format3(
                SequenceContextFormat3::new(coverages, seq_lookups),
            )))
        }
    }
}

/// Convert GPOS ChainedSequenceContext (chained contextual positioning type 8)
fn convert_gpos_chained_context(
    subtable: &layout::ChainedSequenceContext,
    gid_remap: &GidRemap,
    lookup_offset: u16,
) -> Option<PositionChainContext> {
    match subtable {
        layout::ChainedSequenceContext::Format1(f1) => {
            let coverage = f1.coverage().ok()?;
            let remapped_cov = remap_coverage(&coverage, gid_remap);

            let chained_seq_rule_sets: Vec<Option<ChainedSequenceRuleSet>> = f1
                .chained_seq_rule_sets()
                .iter()
                .map(|opt_result| {
                    opt_result.and_then(result::Result::ok).map(|csrs| {
                        // Filter rules where all glyphs can be remapped
                        let chained_seq_rules: Vec<ChainedSequenceRule> = csrs
                            .chained_seq_rules()
                            .iter()
                            .filter_map(result::Result::ok)
                            .filter_map(|rule| {
                                let backtrack =
                                    remap_glyph_array(rule.backtrack_sequence(), gid_remap)?;
                                let input = remap_glyph_array(rule.input_sequence(), gid_remap)?;
                                let lookahead =
                                    remap_glyph_array(rule.lookahead_sequence(), gid_remap)?;
                                let seq_lookups = remap_seq_lookup_records(
                                    rule.seq_lookup_records(),
                                    lookup_offset,
                                );
                                Some(ChainedSequenceRule::new(
                                    backtrack,
                                    input,
                                    lookahead,
                                    seq_lookups,
                                ))
                            })
                            .collect();
                        ChainedSequenceRuleSet::new(chained_seq_rules)
                    })
                })
                .collect();

            Some(PositionChainContext::from(ChainedSequenceContext::Format1(
                ChainedSequenceContextFormat1::new(remapped_cov, chained_seq_rule_sets),
            )))
        }
        layout::ChainedSequenceContext::Format2(f2) => {
            let coverage = f2.coverage().ok()?;
            let backtrack_class_def = f2.backtrack_class_def().ok()?;
            let input_class_def = f2.input_class_def().ok()?;
            let lookahead_class_def = f2.lookahead_class_def().ok()?;

            let remapped_cov = remap_coverage(&coverage, gid_remap);
            let remapped_backtrack_cd = remap_class_def(&backtrack_class_def, gid_remap);
            let remapped_input_cd = remap_class_def(&input_class_def, gid_remap);
            let remapped_lookahead_cd = remap_class_def(&lookahead_class_def, gid_remap);

            let chained_class_seq_rule_sets: Vec<Option<ChainedClassSequenceRuleSet>> = f2
                .chained_class_seq_rule_sets()
                .iter()
                .map(|opt_result| {
                    opt_result.and_then(result::Result::ok).map(|ccsrs| {
                        let chained_class_seq_rules: Vec<ChainedClassSequenceRule> = ccsrs
                            .chained_class_seq_rules()
                            .iter()
                            .filter_map(result::Result::ok)
                            .map(|rule| {
                                let backtrack: Vec<u16> =
                                    rule.backtrack_sequence().iter().map(BigEndian::get).collect();
                                let input: Vec<u16> =
                                    rule.input_sequence().iter().map(BigEndian::get).collect();
                                let lookahead: Vec<u16> =
                                    rule.lookahead_sequence().iter().map(BigEndian::get).collect();
                                let seq_lookups = remap_seq_lookup_records(
                                    rule.seq_lookup_records(),
                                    lookup_offset,
                                );
                                ChainedClassSequenceRule::new(
                                    backtrack,
                                    input,
                                    lookahead,
                                    seq_lookups,
                                )
                            })
                            .collect();
                        ChainedClassSequenceRuleSet::new(chained_class_seq_rules)
                    })
                })
                .collect();

            Some(PositionChainContext::from(ChainedSequenceContext::Format2(
                ChainedSequenceContextFormat2::new(
                    remapped_cov,
                    remapped_backtrack_cd,
                    remapped_input_cd,
                    remapped_lookahead_cd,
                    chained_class_seq_rule_sets,
                ),
            )))
        }
        layout::ChainedSequenceContext::Format3(f3) => {
            let backtrack_coverages: Vec<CoverageTable> = f3
                .backtrack_coverages()
                .iter()
                .filter_map(result::Result::ok)
                .map(|c| remap_coverage(&c, gid_remap))
                .collect();

            let input_coverages: Vec<CoverageTable> = f3
                .input_coverages()
                .iter()
                .filter_map(result::Result::ok)
                .map(|c| remap_coverage(&c, gid_remap))
                .collect();

            let lookahead_coverages: Vec<CoverageTable> = f3
                .lookahead_coverages()
                .iter()
                .filter_map(result::Result::ok)
                .map(|c| remap_coverage(&c, gid_remap))
                .collect();

            let seq_lookups = remap_seq_lookup_records(f3.seq_lookup_records(), lookup_offset);

            Some(PositionChainContext::from(ChainedSequenceContext::Format3(
                ChainedSequenceContextFormat3::new(
                    backtrack_coverages,
                    input_coverages,
                    lookahead_coverages,
                    seq_lookups,
                ),
            )))
        }
    }
}

fn collect_scripts_typed(
    script_list: &layout::ScriptList,
    merged: &mut ScriptLangFeatureMap,
    feature_offset: u16,
) {
    let records = script_list.script_records();
    for i in 0..script_list.script_count() as usize {
        if let Some(record) = records.get(i) {
            let script_tag = ScriptTag::new(record.script_tag());
            if let Ok(script) = record.script(script_list.offset_data()) {
                if let Some(Ok(default_lang)) = script.default_lang_sys() {
                    let indices = default_lang
                        .feature_indices()
                        .iter()
                        .map(|idx| FeatureIndex::new(idx.get() + feature_offset));
                    merged.add_features(script_tag.clone(), LangTag::dflt(), indices);
                }

                let lang_records = script.lang_sys_records();
                for j in 0..script.lang_sys_count() as usize {
                    if let Some(lang_record) = lang_records.get(j) {
                        let lang_tag = LangTag::new(lang_record.lang_sys_tag());
                        if let Ok(lang_sys) = lang_record.lang_sys(script.offset_data()) {
                            let indices = lang_sys
                                .feature_indices()
                                .iter()
                                .map(|idx| FeatureIndex::new(idx.get() + feature_offset));
                            merged.add_features(script_tag.clone(), lang_tag, indices);
                        }
                    }
                }
            }
        }
    }
}

fn add_locl_lookups_typed(
    lookups: &mut Vec<write_fonts::tables::gsub::SubstitutionLookup>,
    features: &mut MergedFeatureList,
    scripts: &mut ScriptLangFeatureMap,
    duplicate_info: &DuplicateGlyphInfo,
    glyph_order: &GlyphOrder,
) {
    for dups in &duplicate_info.per_font {
        if dups.is_empty() {
            continue;
        }

        let mut from_glyphs: Vec<GlyphId16> = Vec::new();
        let mut to_glyphs: Vec<GlyphId16> = Vec::new();

        for (from_name, to_name) in dups {
            if let (Some(from_gid), Some(to_gid)) =
                (glyph_order.mega_id(from_name), glyph_order.mega_id(to_name))
            {
                from_glyphs.push(GlyphId16::new(from_gid.to_u16()));
                to_glyphs.push(GlyphId16::new(to_gid.to_u16()));
            }
        }

        if from_glyphs.is_empty() {
            continue;
        }

        let coverage = CoverageTable::format_1(from_glyphs);
        let single_subst = SingleSubst::format_2(coverage, to_glyphs);

        let lookup = write_fonts::tables::gsub::SubstitutionLookup::Single(Lookup::new(
            LookupFlag::empty(),
            vec![single_subst],
        ));

        lookups.push(lookup);
        let lookup_idx = LookupIndex::new((lookups.len() - 1) as u16);

        let locl_tag = Tag::new(b"locl");
        let feature_idx = features.add(locl_tag, vec![lookup_idx]);

        scripts.add_feature_to_all_scripts(feature_idx);
    }
}

fn build_gsub(
    scripts: HashMap<Tag, HashMap<Tag, Vec<u16>>>,
    features: Vec<(Tag, Vec<u16>)>,
    lookups: Vec<write_fonts::tables::gsub::SubstitutionLookup>,
) -> Result<Gsub> {
    // Build script records
    let mut script_records: Vec<ScriptRecord> = Vec::new();

    for (script_tag, lang_map) in scripts {
        let mut lang_sys_records: Vec<LangSysRecord> = Vec::new();
        let mut default_lang_sys = None;

        for (lang_tag, feature_indices) in lang_map {
            let lang_sys = LangSys::new(feature_indices);

            if lang_tag == Tag::new(b"dflt") {
                default_lang_sys = Some(lang_sys);
            } else {
                lang_sys_records.push(LangSysRecord::new(lang_tag, lang_sys));
            }
        }

        // Sort LangSysRecords by tag (OpenType spec requirement)
        lang_sys_records.sort_by_key(|r| r.lang_sys_tag);

        let script = Script::new(default_lang_sys, lang_sys_records);
        script_records.push(ScriptRecord::new(script_tag, script));
    }

    // Sort ScriptRecords by tag (OpenType spec requirement for binary search)
    script_records.sort_by_key(|r| r.script_tag);

    let script_list = ScriptList::new(script_records);

    // Build feature records
    // Note: FeatureRecords should ideally be sorted by tag per OpenType spec,
    // but since LangSys records reference features by index, we would need to
    // remap all indices after sorting. The current order from the source font
    // should be acceptable as HarfBuzz can handle unsorted FeatureList.
    let feature_records: Vec<FeatureRecord> = features
        .into_iter()
        .map(|(tag, lookup_indices)| {
            let feature = Feature::new(None, lookup_indices);
            FeatureRecord::new(tag, feature)
        })
        .collect();

    let feature_list = FeatureList::new(feature_records);

    // Build lookup list
    let lookup_list = SubstitutionLookupList::new(lookups);

    Ok(Gsub::new(script_list, feature_list, lookup_list))
}

fn build_gpos(
    scripts: HashMap<Tag, HashMap<Tag, Vec<u16>>>,
    features: Vec<(Tag, Vec<u16>)>,
    lookups: Vec<PositionLookup>,
) -> Result<Gpos> {
    // Build script records
    let mut script_records: Vec<ScriptRecord> = Vec::new();

    for (script_tag, lang_map) in scripts {
        let mut lang_sys_records: Vec<LangSysRecord> = Vec::new();
        let mut default_lang_sys = None;

        for (lang_tag, feature_indices) in lang_map {
            let lang_sys = LangSys::new(feature_indices);

            if lang_tag == Tag::new(b"dflt") {
                default_lang_sys = Some(lang_sys);
            } else {
                lang_sys_records.push(LangSysRecord::new(lang_tag, lang_sys));
            }
        }

        // Sort LangSysRecords by tag (OpenType spec requirement)
        lang_sys_records.sort_by_key(|r| r.lang_sys_tag);

        let script = Script::new(default_lang_sys, lang_sys_records);
        script_records.push(ScriptRecord::new(script_tag, script));
    }

    // Sort ScriptRecords by tag (OpenType spec requirement for binary search)
    script_records.sort_by_key(|r| r.script_tag);

    let script_list = ScriptList::new(script_records);

    // Build feature records
    // Note: FeatureRecords should ideally be sorted by tag per OpenType spec,
    // but since LangSys records reference features by index, we would need to
    // remap all indices after sorting. The current order from the source font
    // should be acceptable as HarfBuzz can handle unsorted FeatureList.
    let feature_records: Vec<FeatureRecord> = features
        .into_iter()
        .map(|(tag, lookup_indices)| {
            let feature = Feature::new(None, lookup_indices);
            FeatureRecord::new(tag, feature)
        })
        .collect();

    let feature_list = FeatureList::new(feature_records);

    // Build lookup list
    let lookup_list = PositionLookupList::new(lookups);

    Ok(Gpos::new(script_list, feature_list, lookup_list))
}
