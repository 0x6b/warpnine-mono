//! glyf table merging (TrueType outlines)
//!
//! When merging TrueType fonts, per-glyph hinting instructions are stripped from
//! all fonts except the first. This matches fontTools behavior which calls
//! `removeHinting()` on glyphs from subsequent fonts.
//!
//! The reason is that per-glyph instructions may reference functions defined in
//! `fpgm` or values in `cvt`, which are only copied from the first font. Using
//! instructions that reference incompatible function numbers or CVT indices
//! could cause rendering errors or crashes.

use std::collections::{HashMap, HashSet};

use font_types::GlyphId16;
use read_fonts::{
    TableProvider, tables,
    tables::glyf::{Anchor, CurvePoint, Glyph as ReadGlyph},
    types,
};
use write_fonts::tables::{
    glyf::{
        Anchor::{Offset, Point},
        Bbox, Component, ComponentFlags, CompositeGlyph, Contour, Glyf, GlyfLocaBuilder, Glyph,
        SimpleGlyph, Transform,
    },
    loca::{Loca, LocaFormat},
};

use crate::{
    Result,
    context::MergeContext,
    glyph_order::GlyphName,
    types::{GlyphId, MegaGlyphId},
};

/// Merge glyf tables from multiple fonts
///
/// Returns the glyf table, loca table, and loca format.
///
/// Per-glyph hinting instructions are stripped from all fonts except the first,
/// matching fontTools behavior. This is because instructions may reference
/// `fpgm` functions or `cvt` values that only exist in the first font.
pub fn merge_glyf(ctx: &MergeContext) -> Result<Option<(Glyf, Loca, LocaFormat)>> {
    let fonts = ctx.fonts();

    // Check if fonts have glyf tables (TrueType outlines)
    let has_glyf = fonts.iter().any(|f| f.glyf().is_ok());
    if !has_glyf {
        return Ok(None);
    }

    // Build a map from glyph name to glyph data
    let mut glyph_map: HashMap<GlyphName, Glyph> = HashMap::new();

    // Build a reverse map for component remapping: font_idx -> old_gid -> glyph_name
    let gid_to_name: Vec<HashMap<GlyphId, GlyphName>> = (0..fonts.len())
        .map(|i| {
            ctx.font_mapping(i)
                .iter()
                .map(|(gid, name)| (*gid, name.clone()))
                .collect()
        })
        .collect();

    // Build name to new GID map
    let name_to_new_gid: &HashMap<GlyphName, MegaGlyphId> = ctx.glyph_order().name_to_mega_map();

    for (font_idx, font) in fonts.iter().enumerate() {
        let mapping = ctx.font_mapping(font_idx);

        let Ok(glyf) = font.glyf() else {
            continue;
        };
        let Ok(loca) = font.loca(None) else {
            continue;
        };

        for (gid, glyph_name) in mapping {
            if glyph_map.contains_key(glyph_name) {
                continue;
            }

            let Ok(Some(glyph)) = loca.get_glyf(types::GlyphId::new(gid.to_u32()), &glyf) else {
                glyph_map.insert(glyph_name.clone(), Glyph::Empty);
                continue;
            };

            let strip_hinting = font_idx > 0;
            let converted =
                convert_glyph(&glyph, font_idx, &gid_to_name, name_to_new_gid, strip_hinting);

            glyph_map.insert(glyph_name.clone(), converted);
        }
    }

    // Build set of empty glyph GIDs for composite validation
    // OTS (used by Firefox) rejects composites referencing empty glyphs
    let empty_glyph_gids: HashSet<u16> = ctx
        .mega()
        .iter()
        .enumerate()
        .filter_map(|(gid, name)| {
            glyph_map
                .get(name)
                .and_then(|g| matches!(g, Glyph::Empty).then_some(gid as u16))
        })
        .collect();

    // Convert composites that reference empty glyphs to empty glyphs
    for glyph in glyph_map.values_mut() {
        if let Glyph::Composite(composite) = glyph {
            let references_empty = composite
                .components()
                .iter()
                .any(|comp| empty_glyph_gids.contains(&comp.glyph.to_u16()));
            if references_empty {
                *glyph = Glyph::Empty;
            }
        }
    }

    // Build the glyf and loca tables using GlyfLocaBuilder
    let mut builder = GlyfLocaBuilder::new();

    for name in ctx.mega() {
        let glyph = glyph_map.remove(name).unwrap_or(Glyph::Empty);
        // Ignore validation errors for empty/invalid glyphs
        let _ = builder.add_glyph(&glyph);
    }

    let (glyf, loca, format) = builder.build();
    Ok(Some((glyf, loca, format)))
}

/// Convert a read-fonts glyph to a write-fonts glyph
///
/// If `strip_hinting` is true, per-glyph instructions are removed.
/// This is used for glyphs from non-first fonts to avoid referencing
/// incompatible fpgm functions or cvt values.
fn convert_glyph(
    glyph: &ReadGlyph,
    font_idx: usize,
    gid_to_name: &[HashMap<GlyphId, GlyphName>],
    name_to_new_gid: &HashMap<GlyphName, MegaGlyphId>,
    strip_hinting: bool,
) -> Glyph {
    match glyph {
        tables::glyf::Glyph::Simple(simple) => {
            // Build contours manually
            let mut contours: Vec<Contour> = Vec::new();

            let end_pts = simple.end_pts_of_contours();
            let mut points_iter = simple.points();
            let mut current_point = 0usize;

            for end_pt in end_pts {
                let end = end_pt.get() as usize;
                let mut contour_points = Vec::new();

                while current_point <= end {
                    if let Some(pt) = points_iter.next() {
                        contour_points.push(CurvePoint { x: pt.x, y: pt.y, on_curve: pt.on_curve });
                    }
                    current_point += 1;
                }

                contours.push(contour_points.into());
            }

            let bbox = Bbox {
                x_min: simple.x_min(),
                y_min: simple.y_min(),
                x_max: simple.x_max(),
                y_max: simple.y_max(),
            };

            // Strip hinting instructions from non-first fonts
            let instructions = if strip_hinting { vec![] } else { simple.instructions().to_vec() };

            let simple_glyph = SimpleGlyph {
                bbox,
                contours,
                instructions,
                overlaps: simple.has_overlapping_contours(),
            };

            Glyph::Simple(simple_glyph)
        }
        tables::glyf::Glyph::Composite(composite) => {
            // Build components with remapped GIDs
            let mut components: Vec<Component> = Vec::new();

            for comp in composite.components() {
                let old_gid = GlyphId::new(comp.glyph.to_u32() as u16);

                // Find the new GID for this component
                let new_gid = gid_to_name
                    .get(font_idx)
                    .and_then(|m| m.get(&old_gid))
                    .and_then(|name| name_to_new_gid.get(name))
                    .map_or(0, |m| m.to_u16());

                let anchor = match comp.anchor {
                    Anchor::Offset { x, y } => Offset { x, y },
                    Anchor::Point { base, component } => Point { base, component },
                };

                let transform = Transform {
                    xx: comp.transform.xx,
                    yx: comp.transform.yx,
                    xy: comp.transform.xy,
                    yy: comp.transform.yy,
                };

                // Convert flags using From impl
                let flags: ComponentFlags = comp.flags.into();

                let new_component = Component {
                    glyph: GlyphId16::new(new_gid),
                    anchor,
                    transform,
                    flags,
                };
                components.push(new_component);
            }

            if components.is_empty() {
                return Glyph::Empty;
            }

            let bbox = Bbox {
                x_min: composite.x_min(),
                y_min: composite.y_min(),
                x_max: composite.x_max(),
                y_max: composite.y_max(),
            };

            let first_component = components.remove(0);
            let mut composite_glyph = CompositeGlyph::new(first_component, bbox);

            for comp in components {
                composite_glyph.add_component(comp, bbox);
            }

            Glyph::Composite(composite_glyph)
        }
    }
}
