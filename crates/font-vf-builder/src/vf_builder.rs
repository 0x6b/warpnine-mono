//! Variable font builder implementation.

use std::{collections::HashSet, time::Instant};

use log::info;
use read_fonts::{
    FontData, FontRef, TableProvider,
    types::{F2Dot14, Fixed, GlyphId, NameId, Tag},
};
use write_fonts::{
    FontBuilder,
    from_obj::FromObjRef,
    tables::{
        fvar::{AxisInstanceArrays, Fvar, InstanceRecord, VariationAxisRecord},
        glyf::{GlyfLocaBuilder, Glyph as WriteGlyph},
        gvar::{GlyphDelta, GlyphDeltas, GlyphVariations, Gvar, Tent, iup::iup_delta_optimize},
        head::Head,
        name::{Name, NameRecord},
        stat::{AxisRecord as StatAxisRecord, AxisValue, AxisValueTableFlags, Stat},
    },
};

use crate::{
    designspace::DesignSpace,
    error::{Error, Result},
    variation_model::VariationModel,
};

/// Tables that should NOT be copied (variation-specific or rebuilt).
const SKIP_TABLES: &[Tag] = &[
    Tag::new(b"glyf"),
    Tag::new(b"loca"),
    Tag::new(b"head"),
    Tag::new(b"fvar"),
    Tag::new(b"gvar"),
    Tag::new(b"STAT"),
    Tag::new(b"HVAR"),
    Tag::new(b"MVAR"),
    Tag::new(b"DSIG"),
    Tag::new(b"name"),
    Tag::new(b"GDEF"),
    Tag::new(b"GSUB"),
    Tag::new(b"vhea"),
    Tag::new(b"vmtx"),
];

/// User-defined name ID ranges used by generated variation tables.
const AXIS_NAME_ID_START: u16 = 256;
const INSTANCE_NAME_ID_START: u16 = 300;
const INSTANCE_PS_NAME_ID_START: u16 = 400;

/// Build a variable font from a designspace.
///
/// This function:
/// 1. Loads all master fonts
/// 2. Verifies glyph compatibility across masters
/// 3. Computes glyph deltas using the variation model
/// 4. Builds fvar, gvar, and other required tables
/// 5. Copies other tables from the default master
pub fn build_variable_font(designspace: &DesignSpace) -> Result<Vec<u8>> {
    designspace.validate().map_err(Error::InvalidDesignspace)?;

    info!("Building variable font from {} masters", designspace.sources.len());

    // Load all master fonts
    let master_data: Vec<Vec<u8>> = designspace
        .sources
        .iter()
        .map(|source| {
            read(&source.path).map_err(|e| ReadFont { path: source.path.clone(), source: e })
        })
        .collect::<Result<Vec<_>>>()?;

    let masters: Vec<FontRef> = master_data
        .iter()
        .zip(designspace.sources.iter())
        .map(|(data, source)| {
            FontRef::new(data)
                .map_err(|e| ParseFont { path: source.path.clone(), message: e.to_string() })
        })
        .collect::<Result<Vec<_>>>()?;

    let default_idx = designspace.default_source_index().ok_or(Error::NoDefaultSource)?;
    let default_font = &masters[default_idx];

    // Verify glyph compatibility
    verify_glyph_compatibility(designspace, &masters)?;

    // Build variation model
    let model = VariationModel::new(designspace).ok_or(Error::NoDefaultSource)?;

    info!("Variation model: {} regions", model.regions.len());

    // Ensure we have glyf table
    let _ = default_font.glyf().map_err(|_| MissingTable {
        path: designspace.sources[default_idx].path.clone(),
        table: "glyf".to_string(),
    })?;

    let num_glyphs = default_font.maxp()?.num_glyphs();
    info!("Processing {num_glyphs} glyphs");

    // Build gvar table
    let gvar_start = Instant::now();
    let gvar = build_gvar(designspace, &masters, &model, num_glyphs)?;
    info!("Built gvar table in {:.2}s", gvar_start.elapsed().as_secs_f64());

    // Build glyf/loca tables (copy from default)
    let (new_glyf, new_loca, loca_format) = build_glyf_loca(default_font)?;

    // Build fvar table
    let fvar = build_fvar(designspace)?;
    info!("Built fvar table with {} axes", designspace.axes.len());

    // Build head table
    let head = build_head(default_font, loca_format)?;

    // Build name table with instance names
    let name = build_name(default_font, designspace)?;

    // Build STAT table (style attributes)
    let stat = build_stat(designspace)?;
    info!("Built STAT table");

    // Assemble the font
    let mut builder = FontBuilder::new();

    builder.add_table(&fvar)?;
    builder.add_table(&gvar)?;
    builder.add_table(&new_glyf)?;
    builder.add_table(&new_loca)?;
    builder.add_table(&head)?;
    builder.add_table(&name)?;
    builder.add_table(&stat)?;

    // Copy GDEF without VarStore (source VarStore has wrong axis count)
    if let Ok(gdef) = default_font.gdef() {
        let new_gdef = build_gdef_without_varstore(&gdef);
        builder.add_table(&new_gdef)?;
    }

    // Copy GSUB without FeatureVariations (source uses wrong axis indices)
    if let Ok(gsub) = default_font.gsub() {
        let new_gsub = build_gsub_without_feature_variations(&gsub)?;
        builder.add_table(&new_gsub)?;
    }

    // Copy tables from default master
    let skip_set: HashSet<Tag> = SKIP_TABLES.iter().copied().collect();
    for record in default_font.table_directory.table_records() {
        let tag = record.tag();
        if !skip_set.contains(&tag)
            && let Some(data) = default_font.table_data(tag)
        {
            builder.add_raw(tag, data);
        }
    }

    Ok(builder.build())
}

fn verify_glyph_compatibility(designspace: &DesignSpace, masters: &[FontRef]) -> Result<()> {
    let default_idx = designspace.default_source_index().ok_or(Error::NoDefaultSource)?;
    let default_font = &masters[default_idx];
    let expected_glyphs = default_font.maxp()?.num_glyphs();

    for (idx, master) in masters.iter().enumerate() {
        if idx == default_idx {
            continue;
        }

        let actual_glyphs = master.maxp()?.num_glyphs();
        if actual_glyphs != expected_glyphs {
            return Err(GlyphCountMismatch {
                path: designspace.sources[idx].path.clone(),
                expected: expected_glyphs,
                actual: actual_glyphs,
            });
        }
    }

    Ok(())
}

fn build_fvar(designspace: &DesignSpace) -> Result<Fvar> {
    let axes: Vec<VariationAxisRecord> = designspace
        .axes
        .iter()
        .enumerate()
        .map(|(idx, axis)| {
            let mut tag_bytes = [b' '; 4];
            for (i, b) in axis.tag.bytes().take(4).enumerate() {
                tag_bytes[i] = b;
            }

            VariationAxisRecord {
                axis_tag: Tag::new(&tag_bytes),
                min_value: Fixed::from_f64(f64::from(axis.minimum)),
                default_value: Fixed::from_f64(f64::from(axis.default)),
                max_value: Fixed::from_f64(f64::from(axis.maximum)),
                flags: 0u16,
                axis_name_id: NameId::new(AXIS_NAME_ID_START + idx as u16),
            }
        })
        .collect();

    let instances: Vec<InstanceRecord> = designspace
        .instances
        .iter()
        .enumerate()
        .map(|(idx, instance)| {
            let coordinates: Vec<Fixed> = designspace
                .axes
                .iter()
                .map(|axis| Fixed::from_f64(f64::from(instance.axis_value(axis))))
                .collect();

            let post_script_name_id = instance
                .postscript_name
                .as_ref()
                .map(|_| NameId::new(INSTANCE_PS_NAME_ID_START + idx as u16));

            InstanceRecord {
                subfamily_name_id: NameId::new(INSTANCE_NAME_ID_START + idx as u16),
                flags: 0,
                coordinates,
                post_script_name_id,
            }
        })
        .collect();

    Ok(Fvar {
        axis_instance_arrays: AxisInstanceArrays { axes, instances }.into(),
    })
}

/// Build name table, copying from default and adding instance names.
fn build_name(default_font: &FontRef, designspace: &DesignSpace) -> Result<Name> {
    let name_table = default_font.name().map_err(|_| MissingTable {
        path: designspace.sources[0].path.clone(),
        table: "name".to_string(),
    })?;

    let mut new_records: Vec<NameRecord> = Vec::new();

    let axis_name_ids: HashSet<u16> =
        (AXIS_NAME_ID_START..AXIS_NAME_ID_START + designspace.axes.len() as u16).collect();

    let instance_name_ids: HashSet<u16> = (INSTANCE_NAME_ID_START
        ..INSTANCE_NAME_ID_START + designspace.instances.len() as u16)
        .collect();

    // Name IDs used for STAT table values
    let stat_name_ids: HashSet<u16> = [280, 281, 282, 283, 284, 285, 286, 287, 290, 291]
        .into_iter()
        .collect();

    // Name IDs used for fvar instance PostScript names
    let ps_name_ids: HashSet<u16> = (INSTANCE_PS_NAME_ID_START
        ..INSTANCE_PS_NAME_ID_START + designspace.instances.len() as u16)
        .collect();

    // Copy existing name records (skip any that will be replaced)
    for record in name_table.name_record() {
        let name_id = record.name_id().to_u16();

        // Skip name IDs that will be used for instances, STAT, or instance
        // PostScript names.
        if axis_name_ids.contains(&name_id)
            || instance_name_ids.contains(&name_id)
            || stat_name_ids.contains(&name_id)
            || ps_name_ids.contains(&name_id)
        {
            continue;
        }

        let string = match record.string(name_table.string_data()) {
            Ok(s) => s.chars().collect::<String>(),
            Err(_) => continue,
        };

        new_records.push(NameRecord::new(
            record.platform_id(),
            record.encoding_id(),
            record.language_id(),
            NameId::new(name_id),
            string.into(),
        ));
    }

    // Add variation-axis names referenced by fvar and STAT.
    for (idx, axis) in designspace.axes.iter().enumerate() {
        let name_id = AXIS_NAME_ID_START + idx as u16;
        new_records.push(NameRecord::new(
            3,
            1,
            0x409,
            NameId::new(name_id),
            axis.name.clone().into(),
        ));
        new_records.push(NameRecord::new(1, 0, 0, NameId::new(name_id), axis.name.clone().into()));
    }

    // Add instance names for both platforms (Windows and Mac)
    for (idx, instance) in designspace.instances.iter().enumerate() {
        let name_id = INSTANCE_NAME_ID_START + idx as u16;

        // Windows (platformID=3, encodingID=1, languageID=0x409)
        new_records.push(NameRecord::new(
            3,
            1,
            0x409,
            NameId::new(name_id),
            instance.name.clone().into(),
        ));

        // Mac (platformID=1, encodingID=0, languageID=0)
        new_records.push(NameRecord::new(
            1,
            0,
            0,
            NameId::new(name_id),
            instance.name.clone().into(),
        ));

        // Optional PostScript name (name IDs 300+)
        if let Some(ps_name) = &instance.postscript_name {
            let ps_id = INSTANCE_PS_NAME_ID_START + idx as u16;
            new_records.push(NameRecord::new(
                3,
                1,
                0x409,
                NameId::new(ps_id),
                ps_name.clone().into(),
            ));
            new_records.push(NameRecord::new(1, 0, 0, NameId::new(ps_id), ps_name.clone().into()));
        }
    }

    // Add STAT table name entries
    // Weight values (name IDs 280-287), restricted to the axis range.
    for (_value, name, name_id) in weight_stops_in_range(designspace) {
        // Windows
        new_records.push(NameRecord::new(
            3,
            1,
            0x409,
            NameId::new(name_id),
            name.to_string().into(),
        ));
        // Mac
        new_records.push(NameRecord::new(1, 0, 0, NameId::new(name_id), name.to_string().into()));
    }

    if designspace.axes.iter().any(|axis| axis.tag == "ital") {
        for (name_id, name) in [(290, "Upright"), (291, "Italic")] {
            new_records.push(NameRecord::new(
                3,
                1,
                0x409,
                NameId::new(name_id),
                name.to_string().into(),
            ));
            new_records.push(NameRecord::new(
                1,
                0,
                0,
                NameId::new(name_id),
                name.to_string().into(),
            ));
        }
    }

    // Sort records by (platformID, encodingID, languageID, nameID)
    new_records.sort_by(|a, b| {
        (a.platform_id, a.encoding_id, a.language_id, a.name_id).cmp(&(
            b.platform_id,
            b.encoding_id,
            b.language_id,
            b.name_id,
        ))
    });

    Ok(Name::new(new_records))
}

use std::{
    fs::read,
    result,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use kurbo::{Point, Vec2};
use log::warn;
use read_fonts::tables::glyf::{Anchor, CompositeGlyph, SimpleGlyph};
use warpnine_font_ops::weight_name;
use write_fonts::{
    tables,
    tables::{glyf::Glyph, loca::LocaFormat, stat::AxisRecord},
};

use crate::error::Error::{
    GlyphCountMismatch, MissingTable, ParseFont, PointCountMismatch, ReadFont,
};

static TOTAL_POINTS: AtomicUsize = AtomicUsize::new(0);
static REQUIRED_POINTS: AtomicUsize = AtomicUsize::new(0);
static OPTIONAL_POINTS: AtomicUsize = AtomicUsize::new(0);
static DELTA_COMPUTE_NS: AtomicU64 = AtomicU64::new(0);
static IUP_OPTIMIZE_NS: AtomicU64 = AtomicU64::new(0);

fn build_gvar(
    designspace: &DesignSpace,
    masters: &[FontRef],
    model: &VariationModel,
    num_glyphs: u16,
) -> Result<Gvar> {
    // Reset counters
    TOTAL_POINTS.store(0, Ordering::Relaxed);
    REQUIRED_POINTS.store(0, Ordering::Relaxed);
    OPTIONAL_POINTS.store(0, Ordering::Relaxed);
    DELTA_COMPUTE_NS.store(0, Ordering::Relaxed);
    IUP_OPTIMIZE_NS.store(0, Ordering::Relaxed);

    // Load glyf/loca for all masters
    let master_glyfs: Vec<_> = masters
        .iter()
        .map(TableProvider::glyf)
        .collect::<result::Result<Vec<_>, _>>()?;
    let master_locas: Vec<_> = masters
        .iter()
        .map(|m| m.loca(None))
        .collect::<result::Result<Vec<_>, _>>()?;

    let axis_count = designspace.axes.len() as u16;

    let variations_start = Instant::now();
    let all_variations: Vec<GlyphVariations> = (0..num_glyphs)
        .map(|glyph_idx| {
            let gid = GlyphId::new(u32::from(glyph_idx));
            build_glyph_variations(gid, designspace, &master_glyfs, &master_locas, model)
        })
        .collect::<Result<Vec<_>>>()?;
    let variations_elapsed = variations_start.elapsed().as_secs_f64();

    let total = TOTAL_POINTS.load(Ordering::Relaxed);
    let required = REQUIRED_POINTS.load(Ordering::Relaxed);
    let optional = OPTIONAL_POINTS.load(Ordering::Relaxed);
    info!(
        "Glyph variations computed in {variations_elapsed:.2}s ({num_glyphs} glyphs, {:.0} glyphs/sec)",
        f64::from(num_glyphs) / variations_elapsed
    );
    info!(
        "IUP statistics: {total} total points, {required} required ({:.1}%), {optional} optional ({:.1}%)",
        required as f64 / total as f64 * 100.0,
        optional as f64 / total as f64 * 100.0
    );

    let delta_secs = DELTA_COMPUTE_NS.load(Ordering::Relaxed) as f64 / 1_000_000_000.0;
    let iup_secs = IUP_OPTIMIZE_NS.load(Ordering::Relaxed) as f64 / 1_000_000_000.0;
    info!("Time breakdown: delta_compute={delta_secs:.2}s, iup_optimize={iup_secs:.2}s");

    let gvar_build_start = Instant::now();
    let gvar = Gvar::new(all_variations, axis_count).map_err(Error::GvarBuild)?;
    info!("Gvar::new() took {:.2}s", gvar_build_start.elapsed().as_secs_f64());

    Ok(gvar)
}

fn build_glyph_variations(
    gid: GlyphId,
    designspace: &DesignSpace,
    master_glyfs: &[read_fonts::tables::glyf::Glyf],
    master_locas: &[read_fonts::tables::loca::Loca],
    model: &VariationModel,
) -> Result<GlyphVariations> {
    use read_fonts::tables::glyf::Glyph;

    let default_idx = model.default_idx;

    // Get the default glyph
    let default_glyph = master_locas[default_idx]
        .get_glyf(gid, &master_glyfs[default_idx])
        .ok()
        .flatten();

    let Some(default_glyph) = default_glyph else {
        // Empty glyph - no variations needed
        return Ok(GlyphVariations::new(gid, vec![]));
    };

    match default_glyph {
        Glyph::Simple(simple) => build_simple_glyph_variations(
            gid,
            &simple,
            designspace,
            master_glyfs,
            master_locas,
            model,
        ),
        Glyph::Composite(composite) => build_composite_glyph_variations(
            gid,
            &composite,
            designspace,
            master_glyfs,
            master_locas,
            model,
        ),
    }
}

fn build_simple_glyph_variations(
    gid: GlyphId,
    default_simple: &SimpleGlyph,
    designspace: &DesignSpace,
    master_glyfs: &[read_fonts::tables::glyf::Glyf],
    master_locas: &[read_fonts::tables::loca::Loca],
    model: &VariationModel,
) -> Result<GlyphVariations> {
    use read_fonts::tables::glyf::Glyph;

    let num_points = default_simple.num_points();

    // Collect points from all masters
    let mut master_points: Vec<Vec<(i16, i16)>> = Vec::with_capacity(designspace.sources.len());

    for (master_idx, (glyf, loca)) in master_glyfs.iter().zip(master_locas.iter()).enumerate() {
        let glyph = loca.get_glyf(gid, glyf).ok().flatten();

        let points: Vec<(i16, i16)> = match glyph {
            Some(Glyph::Simple(simple)) => {
                if simple.num_points() != num_points {
                    return Err(PointCountMismatch {
                        path: designspace.sources[master_idx].path.clone(),
                        glyph_id: gid.to_u32(),
                        expected: num_points,
                        actual: simple.num_points(),
                    });
                }
                simple.points().map(|p| (p.x, p.y)).collect()
            }
            _ => {
                // If a master has no glyph or a different type, use default points
                default_simple.points().map(|p| (p.x, p.y)).collect()
            }
        };

        master_points.push(points);
    }

    // Get default master coordinates for IUP optimization
    let default_coords: Vec<Point> = default_simple
        .points()
        .map(|p| Point::new(f64::from(p.x), f64::from(p.y)))
        .collect();

    // Get contour end points for IUP
    let contour_ends: Vec<usize> = default_simple
        .end_pts_of_contours()
        .iter()
        .map(|v| v.get() as usize)
        .collect();

    // Precompute tents for all regions (these are constant per glyph)
    let all_tents: Vec<Vec<Tent>> = model
        .regions
        .iter()
        .map(|region| {
            region
                .axes
                .iter()
                .map(|&(min, peak, max)| {
                    let peak_f2d14 = F2Dot14::from_f32(peak);
                    let intermediate = Some((F2Dot14::from_f32(min), F2Dot14::from_f32(max)));
                    Tent::new(peak_f2d14, intermediate)
                })
                .collect()
        })
        .collect();

    // Compute all deltas for all points across all regions
    // all_deltas[region_idx][point_idx] = (dx, dy)
    let delta_start = Instant::now();
    let num_regions = model.regions.len();
    let num_masters = master_points.len();

    // Pre-allocate point_values buffer to avoid repeated allocations
    let mut point_values: Vec<(i16, i16)> = vec![(0, 0); num_masters];

    // all_raw_deltas[region_idx] = Vec of deltas for that region
    let mut all_raw_deltas: Vec<Vec<Vec2>> =
        (0..num_regions).map(|_| Vec::with_capacity(num_points + 4)).collect();

    // For each point, compute deltas across all regions
    for point_idx in 0..num_points {
        // Fill point_values buffer (no allocation)
        for (master_idx, points) in master_points.iter().enumerate() {
            point_values[master_idx] = points[point_idx];
        }

        // Compute deltas for all regions for this point
        let mut prev_deltas: Vec<(i16, i16)> = Vec::with_capacity(num_regions);
        for (region_idx, raw_deltas) in all_raw_deltas.iter_mut().enumerate() {
            let delta = model.compute_delta_2d_for_region(&point_values, region_idx, &prev_deltas);
            prev_deltas.push(delta);
            raw_deltas.push(Vec2::new(f64::from(delta.0), f64::from(delta.1)));
        }
    }
    DELTA_COMPUTE_NS.fetch_add(delta_start.elapsed().as_nanos() as u64, Ordering::Relaxed);

    // Build GlyphDeltas for each region
    let mut glyph_deltas: Vec<GlyphDeltas> = Vec::with_capacity(num_regions);

    for region_idx in 0..num_regions {
        let tents = all_tents[region_idx].clone();
        let raw_deltas = &mut all_raw_deltas[region_idx];

        // Add 4 phantom point deltas (set to zero for now)
        // Phantom points: LSB origin, advance width, top origin, advance height
        for _ in 0..4 {
            raw_deltas.push(Vec2::ZERO);
        }

        // Extend coordinates with phantom points
        let mut coords_with_phantom = default_coords.clone();
        for _ in 0..4 {
            coords_with_phantom.push(Point::ZERO);
        }

        // Apply IUP optimization with tolerance of 0.5 (half a unit)
        // Note: We keep all deltas including phantom points - gvar requires them
        let iup_start = Instant::now();
        let deltas =
            match iup_delta_optimize(raw_deltas.clone(), coords_with_phantom, 0.5, &contour_ends) {
                Ok(optimized) => {
                    // Track IUP statistics (outline points only, not phantom)
                    let outline_deltas = &optimized[..num_points];
                    let required_count = outline_deltas.iter().filter(|d| d.required).count();
                    let optional_count = outline_deltas.iter().filter(|d| !d.required).count();
                    TOTAL_POINTS.fetch_add(num_points, Ordering::Relaxed);
                    REQUIRED_POINTS.fetch_add(required_count, Ordering::Relaxed);
                    OPTIONAL_POINTS.fetch_add(optional_count, Ordering::Relaxed);

                    // Force all deltas to be required to work around write-fonts bug
                    optimized
                        .into_iter()
                        .map(|d| GlyphDelta::required(d.x, d.y))
                        .collect()
                }
                Err(e) => {
                    // Log the error for debugging
                    warn!("IUP optimization failed for glyph {}: {e:?}", gid.to_u32());
                    // Fall back to marking all as required (including phantom points)
                    raw_deltas
                        .iter()
                        .map(|d| GlyphDelta::required(d.x as i16, d.y as i16))
                        .collect()
                }
            };
        IUP_OPTIMIZE_NS.fetch_add(iup_start.elapsed().as_nanos() as u64, Ordering::Relaxed);

        glyph_deltas.push(GlyphDeltas::new(tents, deltas));
    }

    Ok(GlyphVariations::new(gid, glyph_deltas))
}

fn build_composite_glyph_variations(
    gid: GlyphId,
    default_composite: &CompositeGlyph,
    designspace: &DesignSpace,
    master_glyfs: &[read_fonts::tables::glyf::Glyf],
    master_locas: &[read_fonts::tables::loca::Loca],
    model: &VariationModel,
) -> Result<GlyphVariations> {
    use read_fonts::tables::glyf::Glyph;

    // Count components with offsets that can vary
    let num_components = default_composite.components().count();

    // Collect component offsets from all masters
    let mut master_offsets: Vec<Vec<(i16, i16)>> = Vec::with_capacity(designspace.sources.len());

    for (glyf, loca) in master_glyfs.iter().zip(master_locas.iter()) {
        let glyph = loca.get_glyf(gid, glyf).ok().flatten();

        let offsets: Vec<(i16, i16)> = match glyph {
            Some(Glyph::Composite(composite)) => composite
                .components()
                .map(|c| {
                    let anchor = c.anchor;
                    match anchor {
                        Anchor::Offset { x, y } => (x, y),
                        _ => (0, 0),
                    }
                })
                .collect(),
            _ => {
                // Use default offsets if master doesn't have this glyph
                default_composite
                    .components()
                    .map(|c| match c.anchor {
                        Anchor::Offset { x, y } => (x, y),
                        _ => (0, 0),
                    })
                    .collect()
            }
        };

        master_offsets.push(offsets);
    }

    // Build GlyphDeltas for each region
    let mut glyph_deltas: Vec<GlyphDeltas> = Vec::with_capacity(model.regions.len());

    for region_idx in 0..model.regions.len() {
        // Get the full region (min, peak, max) for proper tent encoding
        let region = &model.regions[region_idx];
        let tents: Vec<Tent> = region
            .axes
            .iter()
            .map(|&(min, peak, max)| {
                let peak_f2d14 = F2Dot14::from_f32(peak);
                let intermediate = Some((F2Dot14::from_f32(min), F2Dot14::from_f32(max)));
                Tent::new(peak_f2d14, intermediate)
            })
            .collect();

        let mut deltas: Vec<GlyphDelta> = Vec::with_capacity(num_components);

        for comp_idx in 0..num_components {
            let offset_values: Vec<(i16, i16)> = master_offsets
                .iter()
                .map(|offsets| offsets.get(comp_idx).copied().unwrap_or((0, 0)))
                .collect();

            let (_, offset_deltas) = model.compute_deltas_2d(&offset_values);
            let delta = offset_deltas[region_idx];

            deltas.push(GlyphDelta::required(delta.0, delta.1));
        }

        // Add 4 phantom point deltas (zero - composite metrics don't vary here)
        for _ in 0..4 {
            deltas.push(GlyphDelta::required(0, 0));
        }

        glyph_deltas.push(GlyphDeltas::new(tents, deltas));
    }

    Ok(GlyphVariations::new(gid, glyph_deltas))
}

fn build_glyf_loca(
    default_font: &FontRef,
) -> Result<(write_fonts::tables::glyf::Glyf, write_fonts::tables::loca::Loca, LocaFormat)> {
    use read_fonts::tables::glyf::Glyph as ReadGlyph;

    let glyf = default_font.glyf()?;
    let loca = default_font.loca(None)?;
    let num_glyphs = default_font.maxp()?.num_glyphs();

    let mut builder = GlyfLocaBuilder::new();

    for glyph_idx in 0..num_glyphs {
        let gid = GlyphId::new(u32::from(glyph_idx));

        let glyph = loca.get_glyf(gid, &glyf).ok().flatten();

        let write_glyph: WriteGlyph = match glyph {
            None => Glyph::Empty,
            Some(ReadGlyph::Simple(simple)) => {
                let write_simple =
                    tables::glyf::SimpleGlyph::from_obj_ref(&simple, FontData::new(&[]));
                Glyph::Simple(write_simple)
            }
            Some(ReadGlyph::Composite(composite)) => {
                let write_composite =
                    tables::glyf::CompositeGlyph::from_obj_ref(&composite, FontData::new(&[]));
                Glyph::Composite(write_composite)
            }
        };

        builder.add_glyph(&write_glyph)?;
    }

    Ok(builder.build())
}

fn build_head(default_font: &FontRef, loca_format: LocaFormat) -> Result<Head> {
    let head = default_font.head()?;

    Ok(Head::new(
        head.font_revision(),
        head.checksum_adjustment(),
        head.flags(),
        head.units_per_em(),
        head.created(),
        head.modified(),
        head.x_min(),
        head.y_min(),
        head.x_max(),
        head.y_max(),
        head.mac_style(),
        head.lowest_rec_ppem(),
        match loca_format {
            LocaFormat::Short => 0,
            LocaFormat::Long => 1,
        },
    ))
}

/// Weight axis stops as `(user_value, name_id)`.
///
/// Name IDs 280-287 hold the corresponding strings in the name table; the
/// human-readable names come from [`warpnine_font_ops::weight_name`].
const WEIGHT_STOPS: [(f64, u16); 8] = [
    (300.0, 280),
    (400.0, 281),
    (500.0, 282),
    (600.0, 283),
    (700.0, 284),
    (800.0, 285),
    (900.0, 286),
    (1000.0, 287),
];

/// Weight stops that fall within the designspace `wght` axis range.
///
/// Sans (`wght` max 900) drops ExtraBlack (1000); Mono (max 1000) keeps it.
fn weight_stops_in_range(designspace: &DesignSpace) -> Vec<(f64, &'static str, u16)> {
    let Some((min, max)) = designspace
        .axes
        .iter()
        .find(|a| a.tag == "wght")
        .map(|a| (f64::from(a.minimum), f64::from(a.maximum)))
    else {
        return Vec::new();
    };

    WEIGHT_STOPS
        .into_iter()
        .filter(|(value, _)| *value >= min && *value <= max)
        .map(|(value, name_id)| (value, weight_name(value as u16), name_id))
        .collect()
}

/// Build STAT table for style attributes.
///
/// The STAT table is required for proper style menu grouping in applications.
fn build_stat(designspace: &DesignSpace) -> Result<Stat> {
    // Build axis records - these describe the axes in the font
    let axis_records: Vec<StatAxisRecord> = designspace
        .axes
        .iter()
        .enumerate()
        .map(|(idx, axis)| {
            let mut tag_bytes = [b' '; 4];
            for (i, b) in axis.tag.bytes().take(4).enumerate() {
                tag_bytes[i] = b;
            }
            AxisRecord::new(
                Tag::new(&tag_bytes),
                NameId::new(AXIS_NAME_ID_START + idx as u16),
                idx as u16,
            )
        })
        .collect();

    // Build axis values for each named instance/weight
    let mut axis_values: Vec<AxisValue> = Vec::new();

    if let Some(weight_axis_index) = designspace.axes.iter().position(|axis| axis.tag == "wght") {
        for (value, _name, name_id) in weight_stops_in_range(designspace) {
            let mut flags = AxisValueTableFlags::empty();
            if (value - 400.0).abs() < 0.1 {
                flags |= AxisValueTableFlags::ELIDABLE_AXIS_VALUE_NAME;
            }
            axis_values.push(AxisValue::format_1(
                weight_axis_index as u16,
                flags,
                NameId::new(name_id),
                Fixed::from_f64(value),
            ));
        }
    }

    if let Some(italic_axis_index) = designspace.axes.iter().position(|axis| axis.tag == "ital") {
        for (value, name_id, is_default) in [(0.0, 290, true), (1.0, 291, false)] {
            let mut flags = AxisValueTableFlags::empty();
            if is_default {
                flags |= AxisValueTableFlags::ELIDABLE_AXIS_VALUE_NAME;
            }
            axis_values.push(AxisValue::format_1(
                italic_axis_index as u16,
                flags,
                NameId::new(name_id),
                Fixed::from_f64(value),
            ));
        }
    }

    let fallback_name_id = if designspace.axes.iter().any(|axis| axis.tag == "wght") {
        281 // Regular
    } else if designspace.axes.iter().any(|axis| axis.tag == "ital") {
        290 // Upright
    } else {
        2 // Default font subfamily name
    };
    Ok(Stat::new(axis_records, axis_values, NameId::new(fallback_name_id)))
}

/// Build a GDEF table without VarStore.
///
/// The source font's GDEF may contain a VarStore with axis counts that don't
/// match our output VF (e.g., Recursive has 5 axes but we only use 2).
/// We copy everything except the VarStore to avoid OTS validation errors.
fn build_gdef_without_varstore(
    gdef: &read_fonts::tables::gdef::Gdef,
) -> write_fonts::tables::gdef::Gdef {
    use write_fonts::from_obj::ToOwnedTable;

    let glyph_class_def = gdef
        .glyph_class_def()
        .transpose()
        .ok()
        .flatten()
        .map(|g| g.to_owned_table());
    let attach_list = gdef
        .attach_list()
        .transpose()
        .ok()
        .flatten()
        .map(|a| a.to_owned_table());
    let lig_caret_list = gdef
        .lig_caret_list()
        .transpose()
        .ok()
        .flatten()
        .map(|l| l.to_owned_table());
    let mark_attach_class_def = gdef
        .mark_attach_class_def()
        .transpose()
        .ok()
        .flatten()
        .map(|m| m.to_owned_table());

    write_fonts::tables::gdef::Gdef::new(
        glyph_class_def,
        attach_list,
        lig_caret_list,
        mark_attach_class_def,
    )
}

/// Build a GSUB table without FeatureVariations.
///
/// The source font's GSUB may contain FeatureVariations with axis indices that
/// reference axes not present in our output VF (e.g., Recursive has 5 axes but
/// we only use 2). We copy everything except FeatureVariations to avoid OTS
/// validation errors.
fn build_gsub_without_feature_variations(
    gsub: &read_fonts::tables::gsub::Gsub,
) -> Result<write_fonts::tables::gsub::Gsub> {
    use write_fonts::from_obj::ToOwnedTable;

    let script_list = gsub.script_list()?.to_owned_table();
    let feature_list = gsub.feature_list()?.to_owned_table();
    let lookup_list = gsub.lookup_list()?.to_owned_table();

    Ok(write_fonts::tables::gsub::Gsub::new(script_list, feature_list, lookup_list))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::designspace::Axis;

    fn stat_axis_indices(stat: &Stat) -> Vec<u16> {
        stat.offset_to_axis_values
            .as_ref()
            .expect("STAT should contain axis values")
            .iter()
            .map(|value| match value.as_ref() {
                AxisValue::Format1(value) => value.axis_index,
                _ => panic!("expected format 1 axis value"),
            })
            .collect()
    }

    fn ds(wght_max: f32) -> DesignSpace {
        DesignSpace::new(
            vec![
                Axis::new("wght", "Weight", 300.0, 400.0, wght_max),
                Axis::new("ital", "Italic", 0.0, 0.0, 1.0),
            ],
            vec![],
        )
    }

    #[test]
    fn mono_keeps_extrablack() {
        let stops = weight_stops_in_range(&ds(1000.0));
        let names: Vec<&str> = stops.iter().map(|(_, n, _)| *n).collect();
        assert_eq!(
            names,
            ["Light", "Regular", "Medium", "SemiBold", "Bold", "ExtraBold", "Black", "ExtraBlack"]
        );
    }

    #[test]
    fn sans_drops_extrablack_beyond_max() {
        let stops = weight_stops_in_range(&ds(900.0));
        let names: Vec<&str> = stops.iter().map(|(_, n, _)| *n).collect();
        assert_eq!(names, ["Light", "Regular", "Medium", "SemiBold", "Bold", "ExtraBold", "Black"]);
        assert!(stops.iter().all(|(v, _, _)| *v <= 900.0));
    }

    #[test]
    fn stat_does_not_reference_a_missing_italic_axis() {
        let designspace =
            DesignSpace::new(vec![Axis::new("wght", "Weight", 300.0, 400.0, 900.0)], vec![]);
        let stat = build_stat(&designspace).unwrap();

        assert_eq!(stat.design_axes.len(), 1);
        assert!(stat_axis_indices(&stat).iter().all(|&index| index == 0));
    }

    #[test]
    fn stat_uses_actual_axis_order() {
        let designspace = DesignSpace::new(
            vec![
                Axis::new("ital", "Italic", 0.0, 0.0, 1.0),
                Axis::new("wght", "Weight", 300.0, 400.0, 900.0),
            ],
            vec![],
        );
        let indices = stat_axis_indices(&build_stat(&designspace).unwrap());

        assert!(indices.contains(&0));
        assert!(indices.contains(&1));
        assert!(indices.iter().all(|&index| index < 2));
    }
}
