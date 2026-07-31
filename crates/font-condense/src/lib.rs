//! Horizontal width transformation for fonts.
//!
//! This crate provides functionality to apply horizontal scaling to font glyphs,
//! useful for creating condensed or extended font variants.

use anyhow::Result;
use read_fonts::{
    FontRef, TableProvider,
    tables::{
        glyf,
        glyf::{CompositeGlyphFlags, CurvePoint},
    },
    types::{GlyphId, Tag},
};
use write_fonts::{
    FontBuilder,
    from_obj::ToOwnedTable,
    tables::{
        glyf::{
            Anchor,
            Anchor::{Offset, Point},
            Bbox, Component, ComponentFlags, CompositeGlyph, Contour, GlyfLocaBuilder, Glyph,
            SimpleGlyph, Transform,
        },
        head::Head,
        hhea::Hhea,
        hmtx::{Hmtx, LongMetric},
        os2::Os2,
    },
};

const TAG_GLYF: Tag = Tag::new(b"glyf");
const TAG_LOCA: Tag = Tag::new(b"loca");
const TAG_HMTX: Tag = Tag::new(b"hmtx");
const TAG_HEAD: Tag = Tag::new(b"head");
const TAG_HHEA: Tag = Tag::new(b"hhea");
const TAG_OS2: Tag = Tag::new(b"OS/2");

/// Scales a simple glyph's x-coordinates by the given factor.
// `.iter().map(|e| e.get())` over `&BigEndian<T>` reads as a redundant closure to
// clippy, but the method-path form does not type-check: `get` takes `self` by
// value while `iter()` yields `&BigEndian<T>`.
#[allow(clippy::redundant_closure_for_method_calls)]
fn scale_simple_glyph(glyph: &glyf::SimpleGlyph, scale_x: f32) -> SimpleGlyph {
    let mut contours = Vec::new();
    let end_pts: Vec<u16> = glyph.end_pts_of_contours().iter().map(|e| e.get()).collect();
    let all_points: Vec<CurvePoint> = glyph.points().collect();

    let mut start = 0usize;
    for end in end_pts {
        let end_idx = end as usize + 1;
        let scaled_points: Vec<CurvePoint> = all_points[start..end_idx]
            .iter()
            .map(|p| CurvePoint::new((f32::from(p.x) * scale_x).round() as i16, p.y, p.on_curve))
            .collect();
        contours.push(Contour::from(scaled_points));
        start = end_idx;
    }

    let bbox = Bbox {
        x_min: (f32::from(glyph.x_min()) * scale_x).round() as i16,
        y_min: glyph.y_min(),
        x_max: (f32::from(glyph.x_max()) * scale_x).round() as i16,
        y_max: glyph.y_max(),
    };

    SimpleGlyph {
        bbox,
        contours,
        instructions: glyph.instructions().to_vec(),
        overlaps: glyph.has_overlapping_contours(),
    }
}

/// Scales a composite glyph's component offsets by the given factor.
fn scale_composite_glyph(glyph: &glyf::CompositeGlyph, scale_x: f32) -> CompositeGlyph {
    let mut components = Vec::new();

    for c in glyph.components() {
        let new_anchor = match c.anchor {
            Anchor::Offset { x, y } => Offset { x: (f32::from(x) * scale_x).round() as i16, y },
            Anchor::Point { base, component } => Point { base, component },
        };

        let new_transform = Transform {
            xx: c.transform.xx,
            yx: c.transform.yx,
            xy: c.transform.xy,
            yy: c.transform.yy,
        };

        components.push(Component {
            glyph: c.glyph,
            anchor: new_anchor,
            flags: ComponentFlags {
                round_xy_to_grid: c.flags.contains(CompositeGlyphFlags::ROUND_XY_TO_GRID),
                use_my_metrics: c.flags.contains(CompositeGlyphFlags::USE_MY_METRICS),
                scaled_component_offset: c
                    .flags
                    .contains(CompositeGlyphFlags::SCALED_COMPONENT_OFFSET),
                unscaled_component_offset: c
                    .flags
                    .contains(CompositeGlyphFlags::UNSCALED_COMPONENT_OFFSET),
                overlap_compound: c.flags.contains(CompositeGlyphFlags::OVERLAP_COMPOUND),
            },
            transform: new_transform,
        });
    }

    let bbox = Bbox {
        x_min: (f32::from(glyph.x_min()) * scale_x).round() as i16,
        y_min: glyph.y_min(),
        x_max: (f32::from(glyph.x_max()) * scale_x).round() as i16,
        y_max: glyph.y_max(),
    };

    let first = components.remove(0);
    let mut composite = CompositeGlyph::new(first, bbox);
    for c in components {
        composite.add_component(c, bbox);
    }
    composite
}

/// Applies horizontal scaling to a font.
///
/// This function scales all glyph outlines and metrics by the given `scale_x` factor,
/// producing a condensed (scale_x < 1.0) or extended (scale_x > 1.0) variant of the font.
///
/// # Arguments
///
/// * `font_data` - The raw bytes of the input font
/// * `scale_x` - The horizontal scale factor (e.g., 0.85 for 85% width)
/// * `width_class` - Optional OS/2 usWidthClass value (1-9, where 5 is normal)
/// * `weight_class` - Optional OS/2 usWeightClass value (e.g., 400 for regular)
///
/// # Returns
///
/// The scaled font as raw bytes.
///
/// # Tables Modified
///
/// - `glyf`: Scales glyph outlines horizontally
/// - `loca`: Rebuilt to match new glyf table
/// - `head`: Updates xMin/xMax bounds and loca format
/// - `hhea`: Updates horizontal metrics bounds
/// - `hmtx`: Scales advance widths and left side bearings
/// - `OS/2`: Updates xAvgCharWidth, usWidthClass, and usWeightClass
pub fn apply_horizontal_scale(
    font_data: &[u8],
    scale_x: f32,
    width_class: Option<u16>,
    weight_class: Option<u16>,
) -> Result<Vec<u8>> {
    let font = FontRef::new(font_data)?;

    let mut builder = FontBuilder::new();

    for record in font.table_directory.table_records() {
        let tag = record.tag();
        if matches!(tag, TAG_GLYF | TAG_LOCA | TAG_HMTX | TAG_HEAD | TAG_HHEA | TAG_OS2) {
            continue;
        }
        if let Some(table_data) = font.table_data(tag) {
            builder.add_raw(tag, table_data);
        }
    }

    if let Ok(glyf) = font.glyf() {
        if let Ok(loca) = font.loca(None) {
            let num_glyphs = loca.len();
            let mut glyf_builder = GlyfLocaBuilder::new();

            for gid in 0..num_glyphs {
                let glyph_data = loca.get_glyf(GlyphId::new(gid as u32), &glyf);
                let glyph = match glyph_data {
                    Ok(Some(glyf::Glyph::Simple(simple))) => {
                        Glyph::Simple(scale_simple_glyph(&simple, scale_x))
                    }
                    Ok(Some(glyf::Glyph::Composite(composite))) => {
                        Glyph::Composite(scale_composite_glyph(&composite, scale_x))
                    }
                    _ => Glyph::Empty,
                };
                glyf_builder.add_glyph(&glyph)?;
            }

            let (new_glyf, new_loca, loca_format) = glyf_builder.build();
            builder.add_table(&new_glyf)?;
            builder.add_table(&new_loca)?;

            if let Ok(head) = font.head() {
                let mut new_head: Head = head.to_owned_table();
                new_head.x_min = (f32::from(new_head.x_min) * scale_x).round() as i16;
                new_head.x_max = (f32::from(new_head.x_max) * scale_x).round() as i16;
                new_head.index_to_loc_format = loca_format as i16;
                builder.add_table(&new_head)?;
            }
        }
    } else if let Ok(head) = font.head() {
        let mut new_head: Head = head.to_owned_table();
        new_head.x_min = (f32::from(new_head.x_min) * scale_x).round() as i16;
        new_head.x_max = (f32::from(new_head.x_max) * scale_x).round() as i16;
        builder.add_table(&new_head)?;
    }

    if let Ok(hmtx) = font.hmtx() {
        let num_glyphs = font.maxp().map_or(0, |m| m.num_glyphs()) as usize;
        let num_long_metrics = font.hhea().map_or(0, |h| h.number_of_h_metrics()) as usize;

        let mut new_h_metrics = Vec::with_capacity(num_long_metrics);
        let mut new_lsbs = Vec::new();

        for gid in 0..num_glyphs {
            let glyph_id = GlyphId::new(gid as u32);
            let advance = hmtx.advance(glyph_id).unwrap_or(0);
            let lsb = hmtx.side_bearing(glyph_id).unwrap_or(0);

            if gid < num_long_metrics {
                new_h_metrics.push(LongMetric {
                    advance: (f32::from(advance) * scale_x).round() as u16,
                    side_bearing: (f32::from(lsb) * scale_x).round() as i16,
                });
            } else {
                new_lsbs.push((f32::from(lsb) * scale_x).round() as i16);
            }
        }

        let new_hmtx = Hmtx::new(new_h_metrics, new_lsbs);
        builder.add_table(&new_hmtx)?;
    }

    if let Ok(hhea) = font.hhea() {
        let mut new_hhea: Hhea = hhea.to_owned_table();
        let adv_max = new_hhea.advance_width_max.to_u16();
        let min_lsb = new_hhea.min_left_side_bearing.to_i16();
        let min_rsb = new_hhea.min_right_side_bearing.to_i16();
        let x_max = new_hhea.x_max_extent.to_i16();

        new_hhea.advance_width_max = ((f32::from(adv_max) * scale_x).round() as u16).into();
        new_hhea.min_left_side_bearing = ((f32::from(min_lsb) * scale_x).round() as i16).into();
        new_hhea.min_right_side_bearing = ((f32::from(min_rsb) * scale_x).round() as i16).into();
        new_hhea.x_max_extent = ((f32::from(x_max) * scale_x).round() as i16).into();
        builder.add_table(&new_hhea)?;
    }

    if let Ok(os2) = font.os2() {
        let mut new_os2: Os2 = os2.to_owned_table();
        new_os2.x_avg_char_width = (f32::from(new_os2.x_avg_char_width) * scale_x).round() as i16;
        if let Some(wc) = width_class {
            new_os2.us_width_class = wc;
        }
        if let Some(wc) = weight_class {
            new_os2.us_weight_class = wc;
        }
        builder.add_table(&new_os2)?;
    }

    Ok(builder.build())
}
