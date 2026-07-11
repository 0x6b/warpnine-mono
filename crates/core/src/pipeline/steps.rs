//! Pipeline step definitions.

use std::{
    collections::BTreeMap,
    fs::{copy, create_dir_all, rename, write},
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use rayon::prelude::*;
use warpnine_font_ops::copy_gsub_without_feature_variations;

use super::{
    PipelineContext,
    clean::clean,
    download::download,
    vf::{build_warpnine_condensed_vf, build_warpnine_mono_vf, build_warpnine_sans_vf},
};
use crate::{
    MonospaceSettings, Subsetter, convert_to_woff2,
    freeze_batch::{AutoRvrn, freeze_features},
    instance::{AxisLocation, InstanceDef, create_instances_batch},
    io::{check_results, glob_fonts, read_font, write_font},
    merge::merge_with_fallbacks,
    styles::{FeatureTag, MONO_FEATURES, MONO_STYLES, SANS_FEATURES, SANS_STYLES, duotone_casl},
    warpnine::{
        condense::create_condensed,
        ligatures::remove_grave_ligature,
        naming::{FontNaming, set_name, set_ribbi_names_for_pattern},
        sans::create_sans,
    },
};

pub type PipelineStep = (&'static str, fn(&PipelineContext) -> Result<()>);

pub const MONO_STEPS: &[PipelineStep] = &[
    ("clean", step_clean),
    ("download", step_download),
    ("extract-duotone", step_extract_duotone),
    ("remove-ligatures", step_remove_ligatures),
    ("extract-noto-weights", step_extract_noto_weights),
    ("subset-noto", step_subset_noto),
    ("subset-jetbrains-box", step_subset_jetbrains_box),
    ("merge", step_merge),
    ("set-names-mono", step_set_names_mono),
    ("freeze-static-mono", step_freeze_static_mono),
    ("backup-frozen", step_backup_frozen),
    ("build-vf", step_build_vf),
    ("copy-gsub", step_copy_gsub),
    ("restore-frozen", step_restore_frozen),
    ("set-names-vf", step_set_names_vf),
    ("set-monospace", step_set_monospace),
];

pub const SANS_STEPS: &[PipelineStep] = &[
    ("create-condensed", step_create_condensed),
    ("create-sans", step_create_sans),
    ("set-names-sans", step_set_names_sans),
    ("freeze-vf-and-sans", step_freeze_vf_and_sans),
    ("build-sans-vf", step_build_sans_vf),
    ("build-condensed-vf", step_build_condensed_vf),
    ("set-names-sans-vf", step_set_names_sans_vf),
    ("set-names-condensed-vf", step_set_names_condensed_vf),
];

pub const FINAL_STEPS: &[PipelineStep] = &[
    ("set-version", step_set_version),
    ("generate-woff2", step_generate_woff2),
    ("generate-woff2-sans", step_generate_woff2_sans),
    ("generate-woff2-condensed", step_generate_woff2_condensed),
];

pub const MONO_FINAL_STEPS: &[PipelineStep] =
    &[("set-version-mono", step_set_version_mono), ("generate-woff2", step_generate_woff2)];

pub const SANS_ONLY_STEPS: &[PipelineStep] = &[
    ("download", step_download),
    ("create-sans", step_create_sans),
    ("set-names-sans-only", step_set_names_sans_only),
    ("freeze-sans", step_freeze_sans),
    ("build-sans-vf", step_build_sans_vf),
    ("set-names-sans-vf", step_set_names_sans_vf),
    ("set-version-sans", step_set_version_sans),
    ("generate-woff2-sans", step_generate_woff2_sans),
];

pub const CONDENSED_ONLY_STEPS: &[PipelineStep] = &[
    ("download", step_download),
    ("create-condensed", step_create_condensed),
    ("set-names-condensed-only", step_set_names_condensed_only),
    ("freeze-condensed", step_freeze_condensed),
    ("build-condensed-vf", step_build_condensed_vf),
    ("set-names-condensed-vf", step_set_names_condensed_vf),
    ("set-version-condensed", step_set_version_condensed),
    ("generate-woff2-condensed", step_generate_woff2_condensed),
];

fn step_clean(ctx: &PipelineContext) -> Result<()> {
    clean(&ctx.build_dir, &ctx.dist_dir)
}

fn step_download(ctx: &PipelineContext) -> Result<()> {
    download(&ctx.build_dir)
}

fn step_extract_duotone(ctx: &PipelineContext) -> Result<()> {
    println!("  Extracting 16 Duotone instances from Recursive VF...");

    let instances: Vec<InstanceDef> = MONO_STYLES
        .iter()
        .map(|style| InstanceDef {
            name: format!("RecMonoDuotone-{}", style.name),
            axes: vec![
                AxisLocation::new("MONO", 1.0),
                AxisLocation::new("CASL", duotone_casl(style.weight.value())),
                AxisLocation::new("wght", style.weight.value()),
                AxisLocation::new("slnt", style.slant.slnt()),
                AxisLocation::new("CRSV", style.slant.crsv()),
            ],
        })
        .collect();

    create_instances_batch(&ctx.recursive_vf, &ctx.build_dir, &instances)
}

fn step_remove_ligatures(ctx: &PipelineContext) -> Result<()> {
    let fonts = ctx.build_fonts("RecMonoDuotone-*.ttf")?;
    println!("  Removing triple-backtick ligature from {} fonts...", fonts.len());

    let results: Vec<_> = fonts.par_iter().map(|path| remove_grave_ligature(path)).collect();
    check_results(&results, "remove ligatures")
}

/// Noto CJK VF wght values that participate in the merge.
///
/// The source `NotoSansMonoCJKjp-VF.ttf` axis spans 400-700. We extract the
/// four Latin weights that fall inside that range so each WarpnineMono master
/// gets a CJK donor whose weight matches its Latin weight; this produces real
/// gvar deltas for CJK glyphs in the assembled VF. Latin weights below 400 use
/// Noto-400, weights above 700 use Noto-700 (clamped at the source axis ends).
const NOTO_WEIGHTS: &[u16] = &[400, 500, 600, 700];

fn step_extract_noto_weights(ctx: &PipelineContext) -> Result<()> {
    println!("  Extracting weights {NOTO_WEIGHTS:?} from Noto CJK VF...");

    let instances: Vec<InstanceDef> = NOTO_WEIGHTS
        .iter()
        .map(|w| InstanceDef {
            name: format!("Noto-{w}"),
            axes: vec![AxisLocation::new("wght", f32::from(*w))],
        })
        .collect();

    create_instances_batch(&ctx.noto_vf, &ctx.build_dir, &instances)
}

fn step_subset_noto(ctx: &PipelineContext) -> Result<()> {
    println!("  Subsetting Noto fonts to Japanese Unicode ranges...");

    for weight in NOTO_WEIGHTS {
        let input = ctx.build_dir.join(format!("Noto-{weight}.ttf"));
        let output = ctx.build_dir.join(format!("Noto-{weight}-subset.ttf"));
        let data = read_font(&input)?;
        let subset_data = Subsetter::japanese()
            .exclude_codepoints([
                0x25CB, // ○ WHITE CIRCLE
                0x25CF, // ● BLACK CIRCLE
            ])
            .subset(&data)?;
        write_font(&output, subset_data)?;
    }
    Ok(())
}

fn step_subset_jetbrains_box(ctx: &PipelineContext) -> Result<()> {
    println!("  Subsetting JetBrains Mono to box drawing characters...");

    let input = &ctx.jetbrains_mono;
    let output = ctx.build_dir.join("JetBrainsMono-BoxDrawing.ttf");
    let data = read_font(input)?;
    let subset_data = Subsetter::box_drawing().subset(&data)?;
    write_font(&output, subset_data)?;
    Ok(())
}

/// Map a Latin wght value to the nearest Noto CJK wght master.
///
/// Noto CJK Mono VF only spans 400-700, so Latin weights below/above are
/// clamped to the source axis ends. Values inside the range round to one of
/// the four masters we extract.
fn noto_weight_for(latin_wght: f32) -> u16 {
    let w = latin_wght.round() as i32;
    match w {
        ..=449 => 400,
        450..=549 => 500,
        550..=649 => 600,
        _ => 700,
    }
}

fn step_merge(ctx: &PipelineContext) -> Result<()> {
    println!("  Merging Duotone + JetBrains (box) + Noto CJK into WarpnineMono...");

    let jetbrains_box = ctx.build_dir.join("JetBrainsMono-BoxDrawing.ttf");

    // Group Latin statics by which Noto weight they should be merged with.
    let mut groups: BTreeMap<u16, Vec<PathBuf>> = BTreeMap::new();
    for style in MONO_STYLES {
        let path = ctx.build_dir.join(format!("RecMonoDuotone-{}.ttf", style.name));
        if !path.exists() {
            return Err(anyhow!("Missing duotone static: {}", path.display()));
        }
        groups
            .entry(noto_weight_for(style.weight.value()))
            .or_default()
            .push(path);
    }

    create_dir_all(&ctx.dist_dir)?;

    for (noto_w, bases) in &groups {
        let noto_subset = ctx.build_dir.join(format!("Noto-{noto_w}-subset.ttf"));
        println!("    Merging {} statics with Noto-{noto_w}", bases.len());
        merge_with_fallbacks(bases, &[&jetbrains_box, &noto_subset], &ctx.dist_dir)?;
    }

    for font in ctx.dist_fonts("RecMonoDuotone-*.ttf")? {
        let new_name = font
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("Invalid filename: {}", font.display()))?
            .replace("RecMonoDuotone-", "WarpnineMono-");
        let new_path = ctx.dist_dir.join(new_name);
        rename(&font, &new_path)?;
    }

    Ok(())
}

fn step_set_names_mono(ctx: &PipelineContext) -> Result<()> {
    const MONO_COPYRIGHT: &str =
        "Warpnine Mono is based on Recursive Mono Duotone and Noto Sans Mono CJK JP.";

    set_ribbi_names_for_pattern(
        &ctx.dist_dir,
        "WarpnineMono-*.ttf",
        "Warpnine Mono",
        "WarpnineMono",
        MONO_COPYRIGHT,
        "WarpnineMono-",
        MONO_STYLES,
    )?;
    Ok(())
}

fn step_freeze_static_mono(ctx: &PipelineContext) -> Result<()> {
    let fonts = ctx.static_mono_fonts()?;
    println!("  Freezing features in {} static mono fonts...", fonts.len());
    freeze_features(&fonts, MONO_FEATURES, AutoRvrn::Enabled)
}

fn step_backup_frozen(ctx: &PipelineContext) -> Result<()> {
    let backup_dir = ctx.frozen_backup_dir();
    create_dir_all(&backup_dir)?;

    let fonts = ctx.static_mono_fonts()?;
    println!("  Backing up {} frozen static fonts...", fonts.len());

    for font in &fonts {
        let file_name = font
            .file_name()
            .ok_or_else(|| anyhow!("Invalid filename: {}", font.display()))?;
        copy(font, backup_dir.join(file_name))?;
    }
    Ok(())
}

fn step_build_vf(ctx: &PipelineContext) -> Result<()> {
    build_warpnine_mono_vf(&ctx.dist_dir, &ctx.vf_output())
}

fn step_copy_gsub(ctx: &PipelineContext) -> Result<()> {
    println!("  Copying GSUB from Recursive VF to WarpnineMono VF...");
    let source = &ctx.recursive_vf;
    let target = &ctx.vf_output();
    let source_data = read_font(source)?;
    let target_data = read_font(target)?;
    let new_data = copy_gsub_without_feature_variations(&source_data, &target_data)?;
    write_font(target, new_data)?;
    println!(
        "Copied GSUB table (without FeatureVariations) from {} to {}",
        source.display(),
        target.display()
    );
    Ok(())
}

fn step_restore_frozen(ctx: &PipelineContext) -> Result<()> {
    let backup_dir = ctx.frozen_backup_dir();

    if !backup_dir.exists() {
        println!("  No backup directory found, skipping restore");
        return Ok(());
    }

    let backups = glob_fonts(&backup_dir, "WarpnineMono-*.ttf")?;
    println!("  Restoring {} frozen static fonts...", backups.len());

    for backup in &backups {
        let file_name = backup
            .file_name()
            .ok_or_else(|| anyhow!("Invalid filename: {}", backup.display()))?;
        copy(backup, ctx.dist_dir.join(file_name))?;
    }
    Ok(())
}

fn step_set_monospace(ctx: &PipelineContext) -> Result<()> {
    let fonts = ctx.dist_fonts("WarpnineMono-*.ttf")?;
    println!("  Setting monospace flags on {} fonts...", fonts.len());

    let results: Vec<_> = fonts
        .par_iter()
        .map(|path| {
            let data = read_font(path)?;
            let new_data = MonospaceSettings::DEFAULT.apply(&data)?;
            write_font(path, new_data)?;
            Ok(())
        })
        .collect();

    check_results(&results, "set monospace")
}

fn step_create_condensed(ctx: &PipelineContext) -> Result<()> {
    create_condensed(&ctx.recursive_vf, &ctx.dist_dir, 0.90)
}

fn step_create_sans(ctx: &PipelineContext) -> Result<()> {
    create_sans(&ctx.recursive_vf, &ctx.dist_dir)
}

fn step_set_names_sans(ctx: &PipelineContext) -> Result<()> {
    step_set_names_sans_only(ctx)?;
    step_set_names_condensed_only(ctx)?;
    Ok(())
}

fn step_set_names_sans_only(ctx: &PipelineContext) -> Result<()> {
    set_ribbi_names_for_pattern(
        &ctx.dist_dir,
        "WarpnineSans-*.ttf",
        "Warpnine Sans",
        "WarpnineSans",
        "Warpnine Sans is based on Recursive.",
        "WarpnineSans-",
        SANS_STYLES,
    )?;
    Ok(())
}

fn step_set_names_condensed_only(ctx: &PipelineContext) -> Result<()> {
    set_ribbi_names_for_pattern(
        &ctx.dist_dir,
        "WarpnineSansCondensed-*.ttf",
        "Warpnine Sans Condensed",
        "WarpnineSansCondensed",
        "Warpnine Sans Condensed is based on Recursive.",
        "WarpnineSansCondensed-",
        SANS_STYLES,
    )?;
    Ok(())
}

/// Freeze features in fonts matching a pattern.
/// Returns Ok(()) if pattern matches no fonts (silent skip).
fn freeze_matching(
    ctx: &PipelineContext,
    pattern: &str,
    features: &[FeatureTag],
    label: &str,
) -> Result<()> {
    let fonts = ctx.dist_fonts(pattern)?;
    if !fonts.is_empty() {
        println!("  Freezing features in {} {label} fonts...", fonts.len());
        freeze_features(&fonts, features, AutoRvrn::Enabled)?;
    }
    Ok(())
}

fn step_freeze_sans(ctx: &PipelineContext) -> Result<()> {
    freeze_matching(ctx, "WarpnineSans-*.ttf", SANS_FEATURES, "Sans")
}

fn step_freeze_condensed(ctx: &PipelineContext) -> Result<()> {
    freeze_matching(ctx, "WarpnineSansCondensed-*.ttf", SANS_FEATURES, "Condensed")
}

fn step_set_names_vf(ctx: &PipelineContext) -> Result<()> {
    const MONO_COPYRIGHT: &str =
        "Warpnine Mono is based on Recursive Mono Duotone and Noto Sans Mono CJK JP.";

    let vf_path = ctx.vf_output();
    if !vf_path.exists() {
        println!("  VF not found, skipping name setting");
        return Ok(());
    }

    println!(
        "  Setting names for 1 fonts ({})...",
        vf_path.file_name().unwrap_or_default().to_string_lossy()
    );

    // Variable fonts should have:
    // - ID 1 (Family): Just the family name, not "Family VF"
    // - ID 17 (Typographic Subfamily): "Regular" as the default instance
    let naming = FontNaming {
        family: "Warpnine Mono".to_string(),
        style: "Regular".to_string(),
        postscript_family: Some("WarpnineMono".to_string()),
        copyright_extra: Some(MONO_COPYRIGHT.to_string()),
    };

    set_name(&vf_path, &naming)?;

    Ok(())
}

fn step_freeze_vf_and_sans(ctx: &PipelineContext) -> Result<()> {
    let vf = ctx.vf_output();
    if vf.exists() {
        println!("  Freezing features in VF...");
        freeze_features(&[vf], MONO_FEATURES, AutoRvrn::Enabled)?;
    }

    freeze_matching(ctx, "WarpnineSans-*.ttf", SANS_FEATURES, "Sans")?;
    freeze_matching(ctx, "WarpnineSansCondensed-*.ttf", SANS_FEATURES, "Condensed")?;

    Ok(())
}

fn step_build_sans_vf(ctx: &PipelineContext) -> Result<()> {
    build_warpnine_sans_vf(&ctx.dist_dir, &ctx.sans_vf_output())
}

fn step_build_condensed_vf(ctx: &PipelineContext) -> Result<()> {
    build_warpnine_condensed_vf(&ctx.dist_dir, &ctx.condensed_vf_output())
}

/// Set name records on a built variable font. Variable fonts use the family
/// name alone (ID 1/16) with "Regular" as the default instance (ID 17).
fn set_vf_names(
    vf_path: &Path,
    family: &str,
    ps_family: &str,
    copyright_extra: &str,
) -> Result<()> {
    if !vf_path.exists() {
        println!("  VF not found, skipping name setting");
        return Ok(());
    }

    let naming = FontNaming {
        family: family.to_string(),
        style: "Regular".to_string(),
        postscript_family: Some(ps_family.to_string()),
        copyright_extra: Some(copyright_extra.to_string()),
    };

    set_name(vf_path, &naming)
}

fn step_set_names_sans_vf(ctx: &PipelineContext) -> Result<()> {
    set_vf_names(
        &ctx.sans_vf_output(),
        "Warpnine Sans",
        "WarpnineSans",
        "Warpnine Sans is based on Recursive.",
    )
}

fn step_set_names_condensed_vf(ctx: &PipelineContext) -> Result<()> {
    set_vf_names(
        &ctx.condensed_vf_output(),
        "Warpnine Sans Condensed",
        "WarpnineSansCondensed",
        "Warpnine Sans Condensed is based on Recursive.",
    )
}

fn step_generate_woff2_sans(ctx: &PipelineContext) -> Result<()> {
    generate_vf_woff2(&ctx.sans_vf_output())
}

fn step_generate_woff2_condensed(ctx: &PipelineContext) -> Result<()> {
    generate_vf_woff2(&ctx.condensed_vf_output())
}

fn step_set_version(ctx: &PipelineContext) -> Result<()> {
    set_version_matching(ctx, "*.ttf")
}

fn step_set_version_mono(ctx: &PipelineContext) -> Result<()> {
    set_version_matching(ctx, "WarpnineMono-*.ttf")
}

fn step_set_version_sans(ctx: &PipelineContext) -> Result<()> {
    set_version_matching(ctx, "WarpnineSans-*.ttf")
}

fn step_set_version_condensed(ctx: &PipelineContext) -> Result<()> {
    set_version_matching(ctx, "WarpnineSansCondensed-*.ttf")
}

fn set_version_matching(ctx: &PipelineContext, pattern: &str) -> Result<()> {
    let fonts = ctx.dist_fonts(pattern)?;
    println!("  Setting version on {} fonts...", fonts.len());

    let results: Vec<_> = fonts
        .par_iter()
        .map(|path| {
            let data = read_font(path)?;
            let new_data = ctx.version.apply(&data)?;
            write_font(path, new_data)?;
            Ok(())
        })
        .collect();

    check_results(&results, "set version")
}

fn step_generate_woff2(ctx: &PipelineContext) -> Result<()> {
    generate_vf_woff2(&ctx.vf_output())
}

/// Convert a built variable font to WOFF2 alongside the TTF. Silently skips if
/// the source TTF does not exist.
fn generate_vf_woff2(vf: &Path) -> Result<()> {
    if !vf.exists() {
        println!("  VF not found, skipping WOFF2 generation");
        return Ok(());
    }

    println!("  Generating WOFF2 from {}...", vf.display());
    let ttf_data = read_font(vf)?;
    let woff2_data = convert_to_woff2(&ttf_data)?;

    let woff2_path = vf.with_extension("woff2");
    write(&woff2_path, &woff2_data)?;

    let ttf_size = ttf_data.len() as f64 / 1024.0;
    let woff2_size = woff2_data.len() as f64 / 1024.0;
    let ratio = woff2_size / ttf_size * 100.0;
    println!(
        "  Output: {} ({ttf_size:.1} KB -> {woff2_size:.1} KB, {ratio:.1}%)",
        woff2_path.display()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(steps: &[PipelineStep], name: &str) -> usize {
        steps.iter().position(|(step, _)| *step == name).unwrap()
    }

    #[test]
    fn versioning_precedes_every_woff2_conversion() {
        assert!(position(FINAL_STEPS, "set-version") < position(FINAL_STEPS, "generate-woff2"));
        assert!(
            position(FINAL_STEPS, "set-version") < position(FINAL_STEPS, "generate-woff2-sans")
        );
        assert!(
            position(FINAL_STEPS, "set-version")
                < position(FINAL_STEPS, "generate-woff2-condensed")
        );
        assert!(
            position(SANS_ONLY_STEPS, "set-version-sans")
                < position(SANS_ONLY_STEPS, "generate-woff2-sans")
        );
        assert!(
            position(CONDENSED_ONLY_STEPS, "set-version-condensed")
                < position(CONDENSED_ONLY_STEPS, "generate-woff2-condensed")
        );
        assert!(
            position(MONO_FINAL_STEPS, "set-version-mono")
                < position(MONO_FINAL_STEPS, "generate-woff2")
        );
    }
}
