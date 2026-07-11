//! Build pipeline logic for Warpnine fonts.

mod clean;
mod download;
mod steps;
mod vf;

use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::Result;
pub use clean::clean;
pub use download::download;
pub use steps::{
    CONDENSED_ONLY_STEPS, FINAL_STEPS, MONO_FINAL_STEPS, MONO_STEPS, PipelineStep, SANS_ONLY_STEPS,
    SANS_STEPS,
};
pub use vf::{
    build_warpnine_condensed_vf, build_warpnine_mono_vf, build_warpnine_sans_vf,
    warpnine_mono_designspace,
};

use crate::{
    FontVersion,
    config::{JETBRAINS_MONO_FILENAME, NOTO_CJK_VF_FILENAME, RECURSIVE_VF_FILENAME},
    io::glob_fonts,
};

pub struct PipelineContext {
    pub build_dir: PathBuf,
    pub dist_dir: PathBuf,
    pub recursive_vf: PathBuf,
    pub noto_vf: PathBuf,
    pub jetbrains_mono: PathBuf,
    pub version: FontVersion,
}

impl PipelineContext {
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(build_dir: PathBuf, dist_dir: PathBuf, version: String) -> Result<Self> {
        let version = FontVersion::parse(Some(&version))?;
        let recursive_vf = build_dir.join(RECURSIVE_VF_FILENAME);
        let noto_vf = build_dir.join(NOTO_CJK_VF_FILENAME);
        let jetbrains_mono = build_dir.join(JETBRAINS_MONO_FILENAME);
        Ok(Self {
            build_dir,
            dist_dir,
            recursive_vf,
            noto_vf,
            jetbrains_mono,
            version,
        })
    }

    pub fn build_fonts(&self, pattern: &str) -> Result<Vec<PathBuf>> {
        glob_fonts(&self.build_dir, pattern)
    }

    pub fn dist_fonts(&self, pattern: &str) -> Result<Vec<PathBuf>> {
        glob_fonts(&self.dist_dir, pattern)
    }

    pub fn static_mono_fonts(&self) -> Result<Vec<PathBuf>> {
        Ok(self
            .dist_fonts("WarpnineMono-*.ttf")?
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| !s.contains("-VF"))
            })
            .collect())
    }

    pub fn vf_output(&self) -> PathBuf {
        self.dist_dir.join("WarpnineMono-VF.ttf")
    }

    pub fn sans_vf_output(&self) -> PathBuf {
        self.dist_dir.join("WarpnineSans-VF.ttf")
    }

    pub fn condensed_vf_output(&self) -> PathBuf {
        self.dist_dir.join("WarpnineSansCondensed-VF.ttf")
    }

    pub fn frozen_backup_dir(&self) -> PathBuf {
        self.build_dir.join("frozen")
    }
}

pub fn run_step(
    name: &str,
    step_num: usize,
    total: usize,
    ctx: &PipelineContext,
    f: impl Fn(&PipelineContext) -> Result<()>,
) -> Result<()> {
    println!("\n[{step_num}/{total}] {name}");
    let start = Instant::now();
    f(ctx)?;
    println!("  ✓ {name} ({:.2}s)", start.elapsed().as_secs_f64());
    Ok(())
}

pub fn run_steps(
    steps: &[PipelineStep],
    ctx: &PipelineContext,
    offset: usize,
    total: usize,
) -> Result<()> {
    for (i, (name, step_fn)) in steps.iter().enumerate() {
        run_step(name, offset + i + 1, total, ctx, step_fn)?;
    }
    Ok(())
}

pub fn build_all(build_dir: &Path, dist_dir: &Path, version: String) -> Result<()> {
    let ctx = PipelineContext::new(build_dir.to_path_buf(), dist_dir.to_path_buf(), version)?;
    let start = Instant::now();

    println!("═══════════════════════════════════════════════════════════════════════════════");
    println!("Warpnine Fonts Build Pipeline (Rust)");
    println!("═══════════════════════════════════════════════════════════════════════════════");

    let total = MONO_STEPS.len() + SANS_STEPS.len() + FINAL_STEPS.len();

    run_steps(MONO_STEPS, &ctx, 0, total)?;
    run_steps(SANS_STEPS, &ctx, MONO_STEPS.len(), total)?;
    run_steps(FINAL_STEPS, &ctx, MONO_STEPS.len() + SANS_STEPS.len(), total)?;

    println!("\n═══════════════════════════════════════════════════════════════════════════════");
    println!("✨ Build complete in {:.2}s", start.elapsed().as_secs_f64());
    println!("   Output: {}", ctx.dist_dir.display());

    let mono_count = ctx.dist_fonts("WarpnineMono-*.ttf")?.len();
    let sans_count = ctx.dist_fonts("WarpnineSans-*.ttf")?.len();
    let condensed_count = ctx.dist_fonts("WarpnineSansCondensed-*.ttf")?.len();

    println!("   Fonts: {mono_count} Mono, {sans_count} Sans, {condensed_count} Condensed");
    println!("═══════════════════════════════════════════════════════════════════════════════");

    Ok(())
}

pub fn build_mono(build_dir: &Path, dist_dir: &Path, version: String) -> Result<()> {
    let ctx = PipelineContext::new(build_dir.to_path_buf(), dist_dir.to_path_buf(), version)?;
    let start = Instant::now();

    println!("═══════════════════════════════════════════════════════════════════════════════");
    println!("Warpnine Mono Build Pipeline (Rust)");
    println!("═══════════════════════════════════════════════════════════════════════════════");

    let total = MONO_STEPS.len() + MONO_FINAL_STEPS.len();

    run_steps(MONO_STEPS, &ctx, 0, total)?;
    run_steps(MONO_FINAL_STEPS, &ctx, MONO_STEPS.len(), total)?;

    println!("\n═══════════════════════════════════════════════════════════════════════════════");
    println!("✨ Mono build complete in {:.2}s", start.elapsed().as_secs_f64());
    println!("   Output: {}", ctx.dist_dir.display());

    let mono_count = ctx.dist_fonts("WarpnineMono-*.ttf")?.len();
    println!("   Fonts: {mono_count} Mono");
    println!("═══════════════════════════════════════════════════════════════════════════════");

    Ok(())
}

pub fn build_sans(build_dir: &Path, dist_dir: &Path, version: String) -> Result<()> {
    let ctx = PipelineContext::new(build_dir.to_path_buf(), dist_dir.to_path_buf(), version)?;
    let start = Instant::now();

    println!("═══════════════════════════════════════════════════════════════════════════════");
    println!("Warpnine Sans Build Pipeline (Rust)");
    println!("═══════════════════════════════════════════════════════════════════════════════");

    let total = SANS_ONLY_STEPS.len();

    run_steps(SANS_ONLY_STEPS, &ctx, 0, total)?;

    println!("\n═══════════════════════════════════════════════════════════════════════════════");
    println!("✨ Sans build complete in {:.2}s", start.elapsed().as_secs_f64());
    println!("   Output: {}", ctx.dist_dir.display());

    let sans_count = ctx.dist_fonts("WarpnineSans-*.ttf")?.len();
    println!("   Fonts: {sans_count} Sans");
    println!("═══════════════════════════════════════════════════════════════════════════════");

    Ok(())
}

pub fn build_condensed(build_dir: &Path, dist_dir: &Path, version: String) -> Result<()> {
    let ctx = PipelineContext::new(build_dir.to_path_buf(), dist_dir.to_path_buf(), version)?;
    let start = Instant::now();

    println!("═══════════════════════════════════════════════════════════════════════════════");
    println!("Warpnine Sans Condensed Build Pipeline (Rust)");
    println!("═══════════════════════════════════════════════════════════════════════════════");

    let total = CONDENSED_ONLY_STEPS.len();

    run_steps(CONDENSED_ONLY_STEPS, &ctx, 0, total)?;

    println!("\n═══════════════════════════════════════════════════════════════════════════════");
    println!("✨ Condensed build complete in {:.2}s", start.elapsed().as_secs_f64());
    println!("   Output: {}", ctx.dist_dir.display());

    let condensed_count = ctx.dist_fonts("WarpnineSansCondensed-*.ttf")?.len();
    println!("   Fonts: {condensed_count} Condensed");
    println!("═══════════════════════════════════════════════════════════════════════════════");

    Ok(())
}
