//! vhea table merging

use font_types::{FWord, UfWord};
use read_fonts::{
    FontRef, TableProvider,
    tables::{vhea, vhea::Vhea as ReadVhea},
};
use write_fonts::tables::vhea::Vhea;

use crate::{
    Result,
    strategies::{first, max, min},
};

pub fn merge_vhea(fonts: &[FontRef], num_v_metrics: u16) -> Result<Option<Vhea>> {
    let tables: Vec<ReadVhea> = fonts.iter().filter_map(|f| f.vhea().ok()).collect();

    if tables.is_empty() {
        return Ok(None);
    }

    let ascenders: Vec<i16> = tables.iter().map(|t| t.ascender().to_i16()).collect();
    let descenders: Vec<i16> = tables.iter().map(|t| t.descender().to_i16()).collect();
    let line_gaps: Vec<i16> = tables.iter().map(|t| t.line_gap().to_i16()).collect();
    let advance_height_maxs: Vec<u16> =
        tables.iter().map(|t| t.advance_height_max().to_u16()).collect();
    let min_tsbs: Vec<i16> = tables.iter().map(|t| t.min_top_side_bearing().to_i16()).collect();
    let min_bsbs: Vec<i16> = tables.iter().map(|t| t.min_bottom_side_bearing().to_i16()).collect();
    let y_max_extents: Vec<i16> = tables.iter().map(|t| t.y_max_extent().to_i16()).collect();

    Ok(Some(Vhea {
        version: first(&tables.iter().map(vhea::Vhea::version).collect::<Vec<_>>())?,
        ascender: FWord::new(max(&ascenders)?),
        descender: FWord::new(min(&descenders)?),
        line_gap: FWord::new(max(&line_gaps)?),
        advance_height_max: UfWord::new(max(&advance_height_maxs)?),
        min_top_side_bearing: FWord::new(min(&min_tsbs)?),
        min_bottom_side_bearing: FWord::new(min(&min_bsbs)?),
        y_max_extent: FWord::new(max(&y_max_extents)?),
        caret_slope_rise: first(
            &tables.iter().map(vhea::Vhea::caret_slope_rise).collect::<Vec<_>>(),
        )?,
        caret_slope_run: first(
            &tables.iter().map(vhea::Vhea::caret_slope_run).collect::<Vec<_>>(),
        )?,
        caret_offset: first(&tables.iter().map(vhea::Vhea::caret_offset).collect::<Vec<_>>())?,
        number_of_long_ver_metrics: num_v_metrics,
    }))
}
