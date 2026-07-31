//! Variable font instantiation.

use std::{
    collections::{HashMap, HashSet},
    iter::repeat_n,
};

use kurbo::Rect;
pub use read_fonts::tables::glyf::CurvePoint;
use read_fonts::{
    FontRef, TableProvider,
    tables::{
        cmap::{Cmap, CmapSubtable},
        fvar::Fvar,
        glyf::{
            Anchor as ReadAnchor, CompositeGlyph as ReadCompositeGlyph, Glyph, PointFlags,
            SimpleGlyph as ReadSimpleGlyph,
        },
        gsub::{Gsub, SingleSubst, SubstitutionLookup},
        gvar::Gvar,
        hhea::Hhea,
        layout::Condition,
        mvar::{
            Mvar,
            tags::{
                CPHT, HASC, HCOF, HCRN, HCRS, HDSC, HLGP, SBXO, SBXS, SBYO, SBYS, SPXO, SPXS, SPYO,
                SPYS, STRO, STRS, UNDO, UNDS, XHGT,
            },
        },
        os2::Os2,
        post::Post,
    },
    types::{F2Dot14, Fixed, GlyphId, Point, Tag},
};
use write_fonts::{
    FontBuilder,
    from_obj::ToOwnedTable,
    tables,
    tables::{
        glyf::{
            Anchor::Offset, Bbox, CompositeGlyph, Contour, GlyfLocaBuilder, Glyph as WriteGlyph,
            SimpleGlyph,
        },
        head::Head,
        hhea::Hhea as WriteHhea,
        hmtx::{Hmtx as WriteHmtx, LongMetric},
        loca::LocaFormat,
        os2::Os2 as WriteOs2,
        post::Post as WritePost,
        stat::{AxisRecord, AxisValue, AxisValueTableFlags, Stat},
    },
    types::NameId,
};

use crate::{
    AxisLocation,
    error::{Error, Result},
};

fn clamp_i16(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

const VARIATION_TABLES: [Tag; 8] = [
    Tag::new(b"fvar"),
    Tag::new(b"gvar"),
    Tag::new(b"avar"),
    Tag::new(b"cvar"),
    Tag::new(b"HVAR"),
    Tag::new(b"MVAR"),
    Tag::new(b"VVAR"),
    Tag::new(b"STAT"),
];

const REPLACED_TABLES: [Tag; 7] = [
    Tag::new(b"glyf"),
    Tag::new(b"loca"),
    Tag::new(b"hmtx"),
    Tag::new(b"head"),
    Tag::new(b"hhea"),
    Tag::new(b"OS/2"),
    Tag::new(b"post"),
];

const REMOVED_TABLES: [Tag; 1] = [Tag::new(b"DSIG")];

const PHANTOM_POINTS: usize = 4;

/// Instantiate a variable font at the given axis locations.
///
/// Converts a variable font to a static instance by:
/// 1. Normalizing user-space coordinates
/// 2. Applying gvar deltas to glyph coordinates
/// 3. Updating hmtx from phantom point deltas
/// 4. Removing variation tables (fvar, gvar, avar, etc.)
///
/// # Errors
///
/// - `Error::NotVariableFont` if the font has no fvar table
/// - `Error::NoCff2Support` if the font uses CFF outlines (no glyf table)
/// - `Error::NoGvar` if the font has no gvar table
pub fn instantiate(data: &[u8], locations: &[AxisLocation]) -> Result<Vec<u8>> {
    let font = FontRef::new(data)?;

    let fvar = font.fvar().map_err(|_| Error::NotVariableFont)?;
    let glyf = font.glyf().map_err(|_| Error::NoCff2Support)?;
    let loca = font.loca(None).map_err(|_| Error::NoCff2Support)?;
    let gvar = font.gvar().map_err(|_| Error::NoGvar)?;

    let avar = font.avar().ok();
    let axis_count = fvar.axis_count() as usize;
    let mut normalized_coords = vec![F2Dot14::default(); axis_count];

    let user_coords: Vec<(Tag, Fixed)> = locations
        .iter()
        .map(|loc| (loc.tag, Fixed::from_f64(f64::from(loc.value))))
        .collect();

    fvar.user_to_normalized(avar.as_ref(), user_coords, &mut normalized_coords);

    let maxp = font.maxp()?;
    let num_glyphs = u32::from(maxp.num_glyphs());

    let hmtx = font.hmtx()?;
    let hhea = font.hhea()?;
    let num_h_metrics = hhea.number_of_h_metrics() as usize;

    // Pass 1: Build all glyphs with delta application, collecting bboxes
    let mut glyphs: Vec<WriteGlyph> = Vec::with_capacity(num_glyphs as usize);
    let mut glyph_bboxes: Vec<Option<Bbox>> = Vec::with_capacity(num_glyphs as usize);
    let mut advance_width_deltas = Vec::with_capacity(num_glyphs as usize);
    let mut advances: Vec<u16> = Vec::with_capacity(num_glyphs as usize);
    let mut lsbs: Vec<i16> = Vec::with_capacity(num_glyphs as usize);

    for glyph_id in 0..num_glyphs {
        let gid = GlyphId::new(glyph_id);

        let aw_delta = match gvar.phantom_point_deltas(&glyf, &loca, &normalized_coords, gid) {
            Ok(Some(deltas)) => deltas.get(1).map_or(0, |d| d.x.to_i32() as i16),
            _ => 0,
        };
        advance_width_deltas.push(aw_delta);

        let orig_advance = hmtx.advance(gid).unwrap_or(0);
        let orig_lsb = hmtx.side_bearing(gid).unwrap_or(0);
        let new_advance = (i32::from(orig_advance) + i32::from(aw_delta)).max(0) as u16;
        advances.push(new_advance);
        lsbs.push(orig_lsb);

        let Some(glyph) = loca.get_glyf(gid, &glyf).ok().flatten() else {
            glyphs.push(tables::glyf::Glyph::Empty);
            glyph_bboxes.push(None);
            continue;
        };

        match glyph {
            Glyph::Simple(simple) => {
                let new_glyph =
                    apply_deltas_to_simple_glyph(&simple, &gvar, gid, &normalized_coords)?;
                let bbox = new_glyph.bbox();
                glyph_bboxes.push(bbox);
                glyphs.push(new_glyph);
            }
            Glyph::Composite(composite) => {
                let new_glyph =
                    apply_deltas_to_composite_glyph(&composite, &gvar, gid, &normalized_coords)?;
                // Composite bbox will be recomputed in pass 2
                glyph_bboxes.push(None);
                glyphs.push(new_glyph);
            }
        }
    }

    // Pass 2: Recompute composite glyph bboxes from component bboxes
    recompute_composite_bboxes(&mut glyphs, &mut glyph_bboxes);

    // Pass 2.5: Convert composites that reference empty glyphs to empty glyphs
    // OTS (used by Firefox) warns about composites referencing empty glyphs
    let empty_glyph_gids: HashSet<u16> = glyphs
        .iter()
        .enumerate()
        .filter_map(|(gid, g)| matches!(g, WriteGlyph::Empty).then_some(gid as u16))
        .collect();

    for glyph in &mut glyphs {
        if let tables::glyf::Glyph::Composite(composite) = glyph {
            let references_empty = composite
                .components()
                .iter()
                .any(|comp| empty_glyph_gids.contains(&comp.glyph.to_u16()));
            if references_empty {
                *glyph = tables::glyf::Glyph::Empty;
            }
        }
    }

    // Pass 3: Build glyf table, compute font bounds, and collect new LSBs
    let mut glyf_builder = GlyfLocaBuilder::new();
    let mut bounds = FontBounds::new();
    let mut new_lsbs: Vec<i16> = Vec::with_capacity(num_glyphs as usize);

    for (i, glyph) in glyphs.iter().enumerate() {
        // LSB should equal the glyph's xMin after interpolation
        let new_lsb = get_glyph_xmin(glyph).unwrap_or(lsbs[i]);
        new_lsbs.push(new_lsb);
        bounds.update(glyph, advances[i], new_lsb);
        glyf_builder.add_glyph(glyph)?;
    }

    bounds.finalize();

    let (new_glyf, new_loca, loca_format) = glyf_builder.build();
    let new_hmtx = build_new_hmtx(&advances, &new_lsbs, num_h_metrics);

    // Get MVAR deltas if available
    let mvar = font.mvar().ok();

    let mut builder = FontBuilder::new();
    builder.add_table(&new_glyf)?;
    builder.add_table(&new_loca)?;
    builder.add_table(&new_hmtx)?;

    // Build head table with recalculated bounds
    if let Ok(head) = font.head() {
        let new_head = Head::new(
            head.font_revision(),
            head.checksum_adjustment(),
            head.flags(),
            head.units_per_em(),
            head.created(),
            head.modified(),
            bounds.x_min,
            bounds.y_min,
            bounds.x_max,
            bounds.y_max,
            head.mac_style(),
            head.lowest_rec_ppem(),
            match loca_format {
                LocaFormat::Short => 0,
                LocaFormat::Long => 1,
            },
        );
        builder.add_table(&new_head)?;
    }

    // Build hhea table with recalculated metrics and MVAR deltas
    if let Ok(hhea) = font.hhea() {
        let new_hhea = build_new_hhea(&hhea, &bounds, mvar.as_ref(), &normalized_coords);
        builder.add_table(&new_hhea)?;
    }

    // Build OS/2 table with MVAR deltas and updated weight class
    if let Ok(os2) = font.os2() {
        let new_os2 = build_new_os2(&os2, mvar.as_ref(), &normalized_coords, locations);
        builder.add_table(&new_os2)?;
    }

    // Build post table with MVAR deltas
    if let Ok(post) = font.post() {
        let new_post = build_new_post(&post, mvar.as_ref(), &normalized_coords);
        builder.add_table(&new_post)?;
    }

    // Build STAT table for the static instance
    let stat = build_new_stat(&fvar, locations);
    builder.add_table(&stat)?;

    // Build cmap table with FeatureVariations substitutions applied (only if substitutions exist)
    let cmap_replaced = if let Ok(cmap) = font.cmap() {
        let gsub = font.gsub().ok();
        let substitutions = gsub
            .and_then(|gsub| compute_feature_variation_substitutions(&gsub, &normalized_coords));
        if let Some(subs) = &substitutions {
            if let Some(new_cmap) = build_instanced_cmap(&cmap, Some(subs)) {
                builder.add_table(&new_cmap)?;
                true
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    let cmap_tag = Tag::new(b"cmap");
    for record in font.table_directory.table_records() {
        let tag = record.tag();
        let is_replaced = REPLACED_TABLES.contains(&tag) || (tag == cmap_tag && cmap_replaced);
        if !VARIATION_TABLES.contains(&tag)
            && !is_replaced
            && !REMOVED_TABLES.contains(&tag)
            && let Some(data) = font.table_data(tag)
        {
            builder.add_raw(tag, data);
        }
    }

    Ok(builder.build())
}

fn apply_deltas_to_simple_glyph(
    simple: &ReadSimpleGlyph,
    gvar: &Gvar,
    glyph_id: GlyphId,
    coords: &[F2Dot14],
) -> Result<WriteGlyph> {
    let num_points = simple.num_points();
    if num_points == 0 {
        return Ok(tables::glyf::Glyph::Empty);
    }

    let end_pts: Vec<usize> = simple
        .end_pts_of_contours()
        .iter()
        .map(|x| x.get() as usize)
        .collect();

    // Use Fixed (16.16) for delta accumulation to preserve fractional precision
    let mut points: Vec<Point<Fixed>> = Vec::with_capacity(num_points + PHANTOM_POINTS);
    let mut flags: Vec<PointFlags> = Vec::with_capacity(num_points + PHANTOM_POINTS);

    for point in simple.points() {
        points.push(Point::new(
            Fixed::from_i32(i32::from(point.x)),
            Fixed::from_i32(i32::from(point.y)),
        ));
        flags.push(if point.on_curve {
            PointFlags::on_curve()
        } else {
            PointFlags::off_curve_quad()
        });
    }

    // Phantom points for gvar delta application
    for _ in 0..PHANTOM_POINTS {
        points.push(Point::default());
        flags.push(PointFlags::on_curve());
    }

    if let Ok(Some(var_data)) = gvar.glyph_variation_data(glyph_id) {
        for (tuple, scalar) in var_data.active_tuples_at(coords) {
            if tuple.has_deltas_for_all_points() {
                for delta in tuple.deltas() {
                    let idx = delta.position as usize;
                    if let Some(point) = points.get_mut(idx) {
                        let scaled: Point<Fixed> = delta.apply_scalar(scalar);
                        point.x += scaled.x;
                        point.y += scaled.y;
                    }
                }
            } else {
                // Sparse deltas - need IUP (Interpolate Untouched Points)
                let mut has_delta = vec![false; points.len()];
                let mut deltas = vec![Point::<Fixed>::default(); points.len()];

                for delta in tuple.deltas() {
                    let idx = delta.position as usize;
                    if let Some(slot) = deltas.get_mut(idx) {
                        has_delta[idx] = true;
                        *slot = delta.apply_scalar(scalar);
                    }
                }

                // Convert points to i32 for IUP calculation (IUP works on integer coordinates)
                let points_i32: Vec<Point<i32>> = points
                    .iter()
                    .map(|p| Point::new(p.x.to_i32(), p.y.to_i32()))
                    .collect();
                let mut deltas_i32: Vec<Point<i32>> = deltas
                    .iter()
                    .map(|d| Point::new(d.x.to_i32(), d.y.to_i32()))
                    .collect();

                let mut start = 0;
                for &end in &end_pts {
                    iup_contour(&mut deltas_i32, &has_delta, &points_i32, start, end);
                    start = end + 1;
                }

                for (point, delta) in points.iter_mut().zip(&deltas_i32) {
                    point.x += Fixed::from_i32(delta.x);
                    point.y += Fixed::from_i32(delta.y);
                }
            }
        }
    }

    let mut contours = Vec::with_capacity(end_pts.len());
    let mut start = 0;
    for &end in &end_pts {
        let contour_points: Vec<CurvePoint> = (start..=end)
            .map(|i| {
                // Round Fixed to i32, then clamp to i16
                CurvePoint::new(
                    clamp_i16(points[i].x.round().to_i32()),
                    clamp_i16(points[i].y.round().to_i32()),
                    flags[i].is_on_curve(),
                )
            })
            .collect();
        contours.push(Contour::from(contour_points));
        start = end + 1;
    }

    let mut glyph = SimpleGlyph {
        bbox: Bbox::default(),
        contours,
        instructions: simple.instructions().to_vec(),
        overlaps: simple.has_overlapping_contours(),
    };
    glyph.recompute_bounding_box();

    Ok(tables::glyf::Glyph::Simple(glyph))
}

fn apply_deltas_to_composite_glyph(
    composite: &ReadCompositeGlyph,
    gvar: &Gvar,
    glyph_id: GlyphId,
    coords: &[F2Dot14],
) -> Result<WriteGlyph> {
    use write_fonts::tables::glyf::{Anchor, Component, ComponentFlags, Transform};

    let components: Vec<_> = composite.components().collect();
    if components.is_empty() {
        return Ok(tables::glyf::Glyph::Empty);
    }

    // Use Fixed (16.16) for delta accumulation to preserve fractional precision
    let mut offsets: Vec<Point<Fixed>> = components
        .iter()
        .map(|c| match c.anchor {
            ReadAnchor::Offset { x, y } => {
                Point::new(Fixed::from_i32(i32::from(x)), Fixed::from_i32(i32::from(y)))
            }
            ReadAnchor::Point { .. } => Point::default(),
        })
        .collect();

    // Phantom points
    offsets.extend(repeat_n(Point::default(), PHANTOM_POINTS));

    if let Ok(Some(var_data)) = gvar.glyph_variation_data(glyph_id) {
        for (tuple, scalar) in var_data.active_tuples_at(coords) {
            for delta in tuple.deltas() {
                let idx = delta.position as usize;
                if let Some(point) = offsets.get_mut(idx) {
                    let scaled: Point<Fixed> = delta.apply_scalar(scalar);
                    point.x += scaled.x;
                    point.y += scaled.y;
                }
            }
        }
    }

    let mut new_components = components.iter().enumerate().map(|(i, comp)| {
        let anchor = match comp.anchor {
            ReadAnchor::Offset { .. } => Offset {
                x: clamp_i16(offsets[i].x.round().to_i32()),
                y: clamp_i16(offsets[i].y.round().to_i32()),
            },
            ReadAnchor::Point { base, component } => Anchor::Point { base, component },
        };

        let t = comp.transform;
        let transform = Transform { xx: t.xx, yx: t.yx, xy: t.xy, yy: t.yy };

        Component::new(comp.glyph, anchor, transform, ComponentFlags::default())
    });

    let Some(first) = new_components.next() else {
        return Ok(tables::glyf::Glyph::Empty);
    };
    let bbox = Rect::new(
        f64::from(composite.x_min()),
        f64::from(composite.y_min()),
        f64::from(composite.x_max()),
        f64::from(composite.y_max()),
    );

    let mut new_composite = CompositeGlyph::new(first, bbox);
    for comp in new_components {
        new_composite.add_component(comp, Rect::ZERO);
    }

    Ok(tables::glyf::Glyph::Composite(new_composite))
}

/// Recompute bounding boxes for composite glyphs from their components.
///
/// This resolves composite glyph bboxes by transforming each component's bbox
/// using the component's transform and offset, then taking the union.
fn recompute_composite_bboxes(glyphs: &mut [WriteGlyph], bboxes: &mut [Option<Bbox>]) {
    // We need to handle nested composites, so iterate until no changes
    let mut changed = true;
    while changed {
        changed = false;
        for glyph_id in 0..glyphs.len() {
            if bboxes[glyph_id].is_some() {
                continue;
            }

            let tables::glyf::Glyph::Composite(composite) = &glyphs[glyph_id] else {
                continue;
            };

            // Try to compute bbox from components
            if let Some(new_bbox) = compute_composite_bbox(composite, bboxes) {
                bboxes[glyph_id] = Some(new_bbox);
                // Update the composite glyph's stored bbox
                if let tables::glyf::Glyph::Composite(c) = &mut glyphs[glyph_id] {
                    c.bbox = new_bbox;
                }
                changed = true;
            }
        }
    }
}

/// Compute a composite glyph's bbox from its components' bboxes.
/// Returns None if any non-empty component's bbox is not yet resolved.
fn compute_composite_bbox(composite: &CompositeGlyph, bboxes: &[Option<Bbox>]) -> Option<Bbox> {
    use write_fonts::tables::glyf::Anchor;

    let mut x_min = i16::MAX;
    let mut y_min = i16::MAX;
    let mut x_max = i16::MIN;
    let mut y_max = i16::MIN;
    let mut has_content = false;

    for comp in composite.components() {
        let component_gid = comp.glyph.to_u32() as usize;

        // Get component bbox, handling missing entries and empty glyphs
        let component_bbox = match bboxes.get(component_gid) {
            Some(Some(bbox)) => bbox,
            Some(None) => continue, // Empty glyph (e.g., space) - skip
            None => return None,    // Component not yet processed - retry later
        };

        // Skip zero bboxes (empty glyphs with explicit zero bounds)
        if component_bbox.x_min == 0
            && component_bbox.x_max == 0
            && component_bbox.y_min == 0
            && component_bbox.y_max == 0
        {
            continue;
        }

        // Get the component offset
        let (offset_x, offset_y) = match comp.anchor {
            Anchor::Offset { x, y } => (f64::from(x), f64::from(y)),
            Anchor::Point { .. } => (0.0, 0.0), // Point anchors don't add offset
        };

        // Get the transform (2x2 matrix: xx, xy, yx, yy)
        let t = &comp.transform;
        let xx = f64::from(t.xx.to_f32());
        let xy = f64::from(t.xy.to_f32());
        let yx = f64::from(t.yx.to_f32());
        let yy = f64::from(t.yy.to_f32());

        // Transform all four corners of the component bbox
        let corners = [
            (f64::from(component_bbox.x_min), f64::from(component_bbox.y_min)),
            (f64::from(component_bbox.x_min), f64::from(component_bbox.y_max)),
            (f64::from(component_bbox.x_max), f64::from(component_bbox.y_min)),
            (f64::from(component_bbox.x_max), f64::from(component_bbox.y_max)),
        ];

        for (cx, cy) in corners {
            // Apply affine transform: [xx xy] [x]   [e]
            //                         [yx yy] [y] + [f]
            let tx = xx * cx + xy * cy + offset_x;
            let ty = yx * cx + yy * cy + offset_y;

            // Round to integer for comparison
            let ix = tx.round() as i16;
            let iy = ty.round() as i16;

            x_min = x_min.min(ix);
            y_min = y_min.min(iy);
            x_max = x_max.max(ix);
            y_max = y_max.max(iy);
            has_content = true;
        }
    }

    if !has_content {
        return Some(Bbox { x_min: 0, y_min: 0, x_max: 0, y_max: 0 });
    }

    Some(Bbox { x_min, y_min, x_max, y_max })
}

fn iup_contour(
    deltas: &mut [Point<i32>],
    has_delta: &[bool],
    points: &[Point<i32>],
    start: usize,
    end: usize,
) {
    if start > end {
        return;
    }

    let contour_len = end - start + 1;
    let Some(first_touched) = (0..contour_len).position(|i| has_delta[start + i]) else {
        return;
    };

    let mut touched_points = vec![first_touched];
    let mut i = (first_touched + 1) % contour_len;
    while i != first_touched {
        if has_delta[start + i] {
            touched_points.push(i);
        }
        i = (i + 1) % contour_len;
    }

    if touched_points.len() == 1 {
        let touch = touched_points[0];
        let d = deltas[start + touch];
        for i in 0..contour_len {
            if i != touch {
                deltas[start + i] = d;
            }
        }
        return;
    }

    for window in touched_points.windows(2) {
        let (touch1, touch2) = (window[0], window[1]);
        interpolate_between(deltas, points, start, contour_len, touch1, touch2);
    }

    let last = *touched_points.last().unwrap();
    let first = touched_points[0];
    interpolate_between(deltas, points, start, contour_len, last, first);
}

fn interpolate_between(
    deltas: &mut [Point<i32>],
    points: &[Point<i32>],
    start: usize,
    contour_len: usize,
    touch1: usize,
    touch2: usize,
) {
    if (touch1 + 1) % contour_len == touch2 {
        return;
    }

    let p1 = points[start + touch1];
    let p2 = points[start + touch2];
    let d1 = deltas[start + touch1];
    let d2 = deltas[start + touch2];

    let mut i = (touch1 + 1) % contour_len;
    while i != touch2 {
        let p = points[start + i];
        deltas[start + i] = Point::new(
            iup_single(p1.x, p2.x, p.x, d1.x, d2.x),
            iup_single(p1.y, p2.y, p.y, d1.y, d2.y),
        );
        i = (i + 1) % contour_len;
    }
}

fn iup_single(c1: i32, c2: i32, c: i32, d1: i32, d2: i32) -> i32 {
    if c1 == c2 {
        return if d1 == d2 { d1 } else { 0 };
    }

    let (c1, c2, d1, d2) = if c1 > c2 { (c2, c1, d2, d1) } else { (c1, c2, d1, d2) };

    if c <= c1 {
        d1
    } else if c >= c2 {
        d2
    } else {
        let t = f64::from(c - c1) / f64::from(c2 - c1);
        (f64::from(d1) + t * f64::from(d2 - d1)).round() as i32
    }
}

/// Get the xMin value from a glyph's bounding box.
fn get_glyph_xmin(glyph: &WriteGlyph) -> Option<i16> {
    match glyph {
        tables::glyf::Glyph::Simple(s) => Some(s.bbox.x_min),
        tables::glyf::Glyph::Composite(c) => Some(c.bbox.x_min),
        tables::glyf::Glyph::Empty => None,
    }
}

fn build_new_hmtx(advances: &[u16], lsbs: &[i16], num_h_metrics: usize) -> WriteHmtx {
    let num_glyphs = advances.len();
    let mut h_metrics = Vec::with_capacity(num_h_metrics);
    let mut left_side_bearings = Vec::with_capacity(num_glyphs.saturating_sub(num_h_metrics));

    for gid in 0..num_glyphs {
        let advance = advances[gid];
        let lsb = lsbs[gid];

        if gid < num_h_metrics {
            h_metrics.push(LongMetric { advance, side_bearing: lsb });
        } else {
            left_side_bearings.push(lsb);
        }
    }

    WriteHmtx { h_metrics, left_side_bearings }
}

/// Bounding box and metrics information calculated from glyph data.
#[derive(Debug, Clone, Copy, Default)]
struct FontBounds {
    x_min: i16,
    x_max: i16,
    y_min: i16,
    y_max: i16,
    min_left_side_bearing: i16,
    min_right_side_bearing: i16,
    x_max_extent: i16,
    advance_width_max: u16,
}

impl FontBounds {
    fn new() -> Self {
        FontBounds {
            x_min: i16::MAX,
            x_max: i16::MIN,
            y_min: i16::MAX,
            y_max: i16::MIN,
            min_left_side_bearing: i16::MAX,
            min_right_side_bearing: i16::MAX,
            x_max_extent: i16::MIN,
            advance_width_max: 0,
        }
    }

    fn update(&mut self, glyph: &WriteGlyph, advance: u16, _orig_lsb: i16) {
        self.advance_width_max = self.advance_width_max.max(advance);

        let bbox = match glyph {
            tables::glyf::Glyph::Simple(s) => s.bbox,
            tables::glyf::Glyph::Composite(c) => c.bbox,
            tables::glyf::Glyph::Empty => return,
        };

        if bbox.x_min == 0 && bbox.x_max == 0 && bbox.y_min == 0 && bbox.y_max == 0 {
            return;
        }

        self.x_min = self.x_min.min(bbox.x_min);
        self.x_max = self.x_max.max(bbox.x_max);
        self.y_min = self.y_min.min(bbox.y_min);
        self.y_max = self.y_max.max(bbox.y_max);

        // LSB is the glyph's x_min (left bearing from origin to leftmost point)
        let lsb = bbox.x_min;
        self.min_left_side_bearing = self.min_left_side_bearing.min(lsb);

        // RSB = advance_width - LSB - glyph_width
        let glyph_width = bbox.x_max.saturating_sub(bbox.x_min);
        let rsb = (advance as i16).saturating_sub(lsb).saturating_sub(glyph_width);
        self.min_right_side_bearing = self.min_right_side_bearing.min(rsb);

        let extent = lsb.saturating_add(glyph_width);
        self.x_max_extent = self.x_max_extent.max(extent);
    }

    fn finalize(&mut self) {
        if self.x_min == i16::MAX {
            self.x_min = 0;
        }
        if self.x_max == i16::MIN {
            self.x_max = 0;
        }
        if self.y_min == i16::MAX {
            self.y_min = 0;
        }
        if self.y_max == i16::MIN {
            self.y_max = 0;
        }
        if self.min_left_side_bearing == i16::MAX {
            self.min_left_side_bearing = 0;
        }
        if self.min_right_side_bearing == i16::MAX {
            self.min_right_side_bearing = 0;
        }
        if self.x_max_extent == i16::MIN {
            self.x_max_extent = 0;
        }
    }
}

fn get_mvar_delta(mvar: Option<&Mvar>, tag: Tag, coords: &[F2Dot14]) -> i32 {
    mvar.and_then(|m| m.metric_delta(tag, coords).ok())
        .map_or(0, Fixed::to_i32)
}

fn build_new_hhea(
    original: &Hhea,
    bounds: &FontBounds,
    mvar: Option<&Mvar>,
    coords: &[F2Dot14],
) -> WriteHhea {
    let ascender_delta = get_mvar_delta(mvar, HASC, coords);
    let descender_delta = get_mvar_delta(mvar, HDSC, coords);
    let line_gap_delta = get_mvar_delta(mvar, HLGP, coords);
    let caret_slope_rise_delta = get_mvar_delta(mvar, HCRS, coords);
    let caret_slope_run_delta = get_mvar_delta(mvar, HCRN, coords);
    let caret_offset_delta = get_mvar_delta(mvar, HCOF, coords);

    tables::hhea::Hhea::new(
        clamp_i16(i32::from(original.ascender().to_i16()) + ascender_delta).into(),
        clamp_i16(i32::from(original.descender().to_i16()) + descender_delta).into(),
        clamp_i16(i32::from(original.line_gap().to_i16()) + line_gap_delta).into(),
        bounds.advance_width_max.into(),
        bounds.min_left_side_bearing.into(),
        bounds.min_right_side_bearing.into(),
        bounds.x_max_extent.into(),
        clamp_i16(i32::from(original.caret_slope_rise()) + caret_slope_rise_delta),
        clamp_i16(i32::from(original.caret_slope_run()) + caret_slope_run_delta),
        clamp_i16(i32::from(original.caret_offset()) + caret_offset_delta),
        original.number_of_h_metrics(),
    )
}

/// Convert wdth axis value (percentage, typically 50-200) to usWidthClass (1-9)
fn wdth_to_width_class(wdth: f32) -> u16 {
    // OpenType spec usWidthClass values:
    // 1=Ultra-condensed (50%), 2=Extra-condensed (62.5%), 3=Condensed (75%),
    // 4=Semi-condensed (87.5%), 5=Medium/Normal (100%), 6=Semi-expanded (112.5%),
    // 7=Expanded (125%), 8=Extra-expanded (150%), 9=Ultra-expanded (200%)
    match wdth {
        w if w <= 56.25 => 1,  // Ultra-condensed
        w if w <= 68.75 => 2,  // Extra-condensed
        w if w <= 81.25 => 3,  // Condensed
        w if w <= 93.75 => 4,  // Semi-condensed
        w if w <= 106.25 => 5, // Medium (Normal)
        w if w <= 118.75 => 6, // Semi-expanded
        w if w <= 137.5 => 7,  // Expanded
        w if w <= 175.0 => 8,  // Extra-expanded
        _ => 9,                // Ultra-expanded
    }
}

fn build_new_os2(
    original: &Os2,
    mvar: Option<&Mvar>,
    coords: &[F2Dot14],
    locations: &[AxisLocation],
) -> WriteOs2 {
    let mut os2: WriteOs2 = original.to_owned_table();

    // Update usWeightClass if wght axis is specified
    if let Some(wght) = locations.iter().find(|loc| loc.tag == Tag::new(b"wght")) {
        os2.us_weight_class = wght.value.round() as u16;
    }

    // Update usWidthClass if wdth axis is specified
    // wdth axis uses percentage (50-200), usWidthClass uses 1-9 scale
    if let Some(wdth) = locations.iter().find(|loc| loc.tag == Tag::new(b"wdth")) {
        os2.us_width_class = wdth_to_width_class(wdth.value);
    }

    os2.y_strikeout_size =
        clamp_i16(i32::from(original.y_strikeout_size()) + get_mvar_delta(mvar, STRS, coords));
    os2.y_strikeout_position =
        clamp_i16(i32::from(original.y_strikeout_position()) + get_mvar_delta(mvar, STRO, coords));
    os2.s_typo_ascender =
        clamp_i16(i32::from(original.s_typo_ascender()) + get_mvar_delta(mvar, HASC, coords));
    os2.s_typo_descender =
        clamp_i16(i32::from(original.s_typo_descender()) + get_mvar_delta(mvar, HDSC, coords));
    os2.s_typo_line_gap =
        clamp_i16(i32::from(original.s_typo_line_gap()) + get_mvar_delta(mvar, HLGP, coords));

    os2.y_subscript_x_offset =
        clamp_i16(i32::from(original.y_subscript_x_offset()) + get_mvar_delta(mvar, SBXO, coords));
    os2.y_subscript_y_offset =
        clamp_i16(i32::from(original.y_subscript_y_offset()) + get_mvar_delta(mvar, SBYO, coords));
    os2.y_subscript_x_size =
        clamp_i16(i32::from(original.y_subscript_x_size()) + get_mvar_delta(mvar, SBXS, coords));
    os2.y_subscript_y_size =
        clamp_i16(i32::from(original.y_subscript_y_size()) + get_mvar_delta(mvar, SBYS, coords));

    os2.y_superscript_x_offset = clamp_i16(
        i32::from(original.y_superscript_x_offset()) + get_mvar_delta(mvar, SPXO, coords),
    );
    os2.y_superscript_y_offset = clamp_i16(
        i32::from(original.y_superscript_y_offset()) + get_mvar_delta(mvar, SPYO, coords),
    );
    os2.y_superscript_x_size =
        clamp_i16(i32::from(original.y_superscript_x_size()) + get_mvar_delta(mvar, SPXS, coords));
    os2.y_superscript_y_size =
        clamp_i16(i32::from(original.y_superscript_y_size()) + get_mvar_delta(mvar, SPYS, coords));

    if let Some(sx_height) = original.sx_height() {
        os2.sx_height = Some(clamp_i16(i32::from(sx_height) + get_mvar_delta(mvar, XHGT, coords)));
    }

    if let Some(s_cap_height) = original.s_cap_height() {
        os2.s_cap_height =
            Some(clamp_i16(i32::from(s_cap_height) + get_mvar_delta(mvar, CPHT, coords)));
    }

    os2
}

fn build_new_post(original: &Post, mvar: Option<&Mvar>, coords: &[F2Dot14]) -> WritePost {
    let mut post: WritePost = original.to_owned_table();

    let underline_position_delta = get_mvar_delta(mvar, UNDO, coords);
    let underline_thickness_delta = get_mvar_delta(mvar, UNDS, coords);

    post.underline_position =
        clamp_i16(i32::from(original.underline_position().to_i16()) + underline_position_delta)
            .into();
    post.underline_thickness =
        clamp_i16(i32::from(original.underline_thickness().to_i16()) + underline_thickness_delta)
            .into();

    post
}

fn build_new_stat(fvar: &Fvar, locations: &[AxisLocation]) -> Stat {
    let Ok(axis_arrays) = fvar.axis_instance_arrays() else {
        return Stat::new(vec![], vec![], NameId::new(2));
    };
    let axes = axis_arrays.axes();

    let design_axes: Vec<AxisRecord> = axes
        .iter()
        .enumerate()
        .map(|(i, axis)| AxisRecord::new(axis.axis_tag(), axis.axis_name_id(), i as u16))
        .collect();

    let axis_values: Vec<AxisValue> = locations
        .iter()
        .filter_map(|loc| {
            let axis_index = axes.iter().position(|a| a.axis_tag() == loc.tag)?;
            let axis = axes.get(axis_index)?;

            let mut flags = AxisValueTableFlags::empty();
            if loc.value == axis.default_value().to_f64() as f32 {
                flags |= AxisValueTableFlags::ELIDABLE_AXIS_VALUE_NAME;
            }

            Some(AxisValue::format_1(
                axis_index as u16,
                flags,
                axis.axis_name_id(),
                Fixed::from_f64(f64::from(loc.value)),
            ))
        })
        .collect();

    Stat::new(design_axes, axis_values, NameId::new(2))
}

fn compute_feature_variation_substitutions(
    gsub: &Gsub,
    normalized_coords: &[F2Dot14],
) -> Option<HashMap<GlyphId, GlyphId>> {
    let feature_variations = gsub.feature_variations()?.ok()?;
    let lookup_list = gsub.lookup_list().ok()?;
    let feature_list = gsub.feature_list().ok()?;

    for record in feature_variations.feature_variation_records() {
        let Some(Ok(condition_set)) = record.condition_set(feature_variations.offset_data()) else {
            continue;
        };

        let all_conditions_match = condition_set.conditions().iter().all(|cond| {
            let Ok(Condition::Format1AxisRange(c)) = cond else {
                return false;
            };
            let axis_index = c.axis_index() as usize;
            if axis_index >= normalized_coords.len() {
                return false;
            }
            let coord = normalized_coords[axis_index];
            coord >= c.filter_range_min_value() && coord <= c.filter_range_max_value()
        });

        if !all_conditions_match {
            continue;
        }

        let Some(Ok(feature_table_subst)) =
            record.feature_table_substitution(feature_variations.offset_data())
        else {
            continue;
        };

        let mut substitutions = HashMap::new();

        for subst_record in feature_table_subst.substitutions() {
            let feature_index = subst_record.feature_index();

            let Ok(feature_record) =
                feature_list.feature_records().get(feature_index as usize).ok_or(())
            else {
                continue;
            };
            let feature_tag = feature_record.feature_tag();

            if feature_tag != Tag::new(b"rvrn") {
                continue;
            }

            let Ok(alternate_feature) =
                subst_record.alternate_feature(feature_table_subst.offset_data())
            else {
                continue;
            };

            for lookup_index in alternate_feature.lookup_list_indices() {
                let lookup_idx = lookup_index.get() as usize;
                let Ok(lookup) = lookup_list.lookups().get(lookup_idx) else {
                    continue;
                };

                if let SubstitutionLookup::Single(single_lookup) = lookup {
                    for subtable_result in single_lookup.subtables().iter() {
                        let Ok(subtable) = subtable_result else {
                            continue;
                        };
                        match subtable {
                            SingleSubst::Format1(f1) => {
                                let Ok(coverage) = f1.coverage() else {
                                    continue;
                                };
                                let delta = i32::from(f1.delta_glyph_id());
                                for glyph in coverage.iter() {
                                    let from_gid = GlyphId::from(glyph);
                                    let to_gid =
                                        GlyphId::new((from_gid.to_u32() as i32 + delta) as u32);
                                    substitutions.insert(from_gid, to_gid);
                                }
                            }
                            SingleSubst::Format2(f2) => {
                                let Ok(coverage) = f2.coverage() else {
                                    continue;
                                };
                                let substitute_glyph_ids = f2.substitute_glyph_ids();
                                for (i, glyph) in coverage.iter().enumerate() {
                                    if let Some(substitute) = substitute_glyph_ids.get(i) {
                                        let from_gid = GlyphId::from(glyph);
                                        let to_gid = GlyphId::from(substitute.get());
                                        substitutions.insert(from_gid, to_gid);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if !substitutions.is_empty() {
            return Some(substitutions);
        }
    }

    None
}

fn build_instanced_cmap(
    cmap: &Cmap,
    substitutions: Option<&HashMap<GlyphId, GlyphId>>,
) -> Option<tables::cmap::Cmap> {
    let mut mappings: Vec<(char, GlyphId)> = Vec::new();

    for record in cmap.encoding_records() {
        let Ok(subtable) = record.subtable(cmap.offset_data()) else {
            continue;
        };
        match subtable {
            CmapSubtable::Format4(f4) => {
                for (codepoint, glyph_id) in f4.iter() {
                    if let Some(ch) = char::from_u32(codepoint) {
                        let final_gid = substitutions
                            .and_then(|subs| subs.get(&glyph_id).copied())
                            .unwrap_or(glyph_id);
                        mappings.push((ch, final_gid));
                    }
                }
            }
            CmapSubtable::Format12(f12) => {
                for (codepoint, glyph_id) in f12.iter() {
                    if let Some(ch) = char::from_u32(codepoint) {
                        let final_gid = substitutions
                            .and_then(|subs| subs.get(&glyph_id).copied())
                            .unwrap_or(glyph_id);
                        mappings.push((ch, final_gid));
                    }
                }
            }
            _ => continue,
        }
        break;
    }

    if mappings.is_empty() {
        return None;
    }

    tables::cmap::Cmap::from_mappings(mappings).ok()
}

#[cfg(test)]
mod tests {

    #[cfg(test)]
    use font_test_data::CANTARELL_VF_TRIMMED;
    #[cfg(test)]
    use font_test_data::SIMPLE_GLYF;
    #[cfg(test)]
    use font_test_data::VAZIRMATN_VAR;
    use read_fonts::{FontRef, TableProvider};

    use super::*;

    fn get_glyph_coords(font: &FontRef, glyph_id: u32) -> Option<Vec<(i16, i16)>> {
        let glyf = font.glyf().ok()?;
        let loca = font.loca(None).ok()?;
        let glyph = loca.get_glyf(GlyphId::new(glyph_id), &glyf).ok()??;

        match glyph {
            Glyph::Simple(simple) => Some(simple.points().map(|p| (p.x, p.y)).collect()),
            Glyph::Composite(_) => None,
        }
    }

    fn get_advance_width(font: &FontRef, glyph_id: u32) -> Option<u16> {
        font.hmtx().ok()?.advance(GlyphId::new(glyph_id))
    }

    #[test]
    fn instantiate_at_default() {
        let data = VAZIRMATN_VAR;
        let result = instantiate(data, &[AxisLocation::new("wght", 400.0)]).unwrap();

        let output = FontRef::new(&result).unwrap();
        assert!(output.fvar().is_err());
        assert!(output.gvar().is_err());
        assert!(output.glyf().is_ok());
        assert!(output.hmtx().is_ok());
    }

    #[test]
    fn instantiate_at_min() {
        let data = VAZIRMATN_VAR;
        let result = instantiate(data, &[AxisLocation::new("wght", 100.0)]).unwrap();

        let output = FontRef::new(&result).unwrap();
        assert!(output.fvar().is_err());
        assert!(get_glyph_coords(&output, 1).is_some());
    }

    #[test]
    fn instantiate_at_max() {
        let data = VAZIRMATN_VAR;
        let result = instantiate(data, &[AxisLocation::new("wght", 900.0)]).unwrap();

        let output = FontRef::new(&result).unwrap();
        assert!(output.fvar().is_err());
        assert!(get_glyph_coords(&output, 1).is_some());
    }

    #[test]
    fn preserves_glyph_count() {
        let data = VAZIRMATN_VAR;
        let input = FontRef::new(data).unwrap();
        let input_count = input.maxp().unwrap().num_glyphs();

        let result = instantiate(data, &[AxisLocation::new("wght", 700.0)]).unwrap();
        let output = FontRef::new(&result).unwrap();
        let output_count = output.maxp().unwrap().num_glyphs();

        assert_eq!(input_count, output_count);
    }

    #[test]
    fn updates_advance_widths() {
        let data = VAZIRMATN_VAR;

        let result_min = instantiate(data, &[AxisLocation::new("wght", 100.0)]).unwrap();
        let result_max = instantiate(data, &[AxisLocation::new("wght", 900.0)]).unwrap();

        let font_min = FontRef::new(&result_min).unwrap();
        let font_max = FontRef::new(&result_max).unwrap();

        let aw_min = get_advance_width(&font_min, 1).unwrap();
        let aw_max = get_advance_width(&font_max, 1).unwrap();

        assert_ne!(aw_min, aw_max);
    }

    #[test]
    fn rejects_cff_font() {
        let data = CANTARELL_VF_TRIMMED;
        let result = instantiate(data, &[AxisLocation::new("wght", 700.0)]);
        assert!(matches!(result, Err(Error::NoCff2Support)));
    }

    #[test]
    fn rejects_non_variable_font() {
        let data = SIMPLE_GLYF;
        let result = instantiate(data, &[AxisLocation::new("wght", 400.0)]);
        assert!(matches!(result, Err(Error::NotVariableFont)));
    }

    #[test]
    fn handles_empty_locations() {
        let data = VAZIRMATN_VAR;
        let result = instantiate(data, &[]).unwrap();

        let output = FontRef::new(&result).unwrap();
        assert!(output.fvar().is_err());
    }

    #[test]
    fn coordinates_differ_at_extremes() {
        let data = VAZIRMATN_VAR;

        let result_min = instantiate(data, &[AxisLocation::new("wght", 100.0)]).unwrap();
        let result_max = instantiate(data, &[AxisLocation::new("wght", 900.0)]).unwrap();

        let font_min = FontRef::new(&result_min).unwrap();
        let font_max = FontRef::new(&result_max).unwrap();

        let coords_min = get_glyph_coords(&font_min, 1).unwrap();
        let coords_max = get_glyph_coords(&font_max, 1).unwrap();

        assert_eq!(coords_min.len(), coords_max.len());
        assert_ne!(coords_min, coords_max);
    }

    #[test]
    fn lsb_equals_glyph_xmin() {
        let data = VAZIRMATN_VAR;

        // Test at an interpolated position (not axis default)
        let result = instantiate(data, &[AxisLocation::new("wght", 500.0)]).unwrap();
        let font = FontRef::new(&result).unwrap();

        let glyf = font.glyf().unwrap();
        let loca = font.loca(None).unwrap();
        let hmtx = font.hmtx().unwrap();

        // Check that LSB equals xMin for simple glyphs
        for gid in 1..font.maxp().unwrap().num_glyphs().min(20) {
            let glyph_id = GlyphId::new(u32::from(gid));
            let lsb = hmtx.side_bearing(glyph_id).unwrap_or(0);

            if let Some(Glyph::Simple(simple)) = loca.get_glyf(glyph_id, &glyf).ok().flatten()
                && simple.num_points() > 0
            {
                let x_min = simple.points().map(|p| p.x).min().unwrap_or(0);
                assert_eq!(lsb, x_min, "glyph {gid}: LSB ({lsb}) should equal xMin ({x_min})");
            }
        }
    }
}
