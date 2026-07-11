//! Development commands for font manipulation.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use read_fonts::types::Tag;
use warpnine_core::{
    FontVersion, MonospaceSettings, Subsetter, build_warpnine_mono_vf,
    freeze_batch::{AutoRvrn, freeze_features},
    instance::{AxisLocation, InstanceDef, create_instance, create_instances_batch},
    io::{read_font, transform_font_in_place, write_font},
    merge::{merge_batch, merge_fonts},
    parallel::run_parallel,
    warpnine::{
        calt::fix_calt_registration,
        condense::create_condensed,
        ligatures::remove_grave_ligature,
        naming::{FontNaming, set_name},
        sans::create_sans,
    },
};
use warpnine_font_ops::copy_table;

#[derive(Subcommand)]
pub enum DevCommands {
    /// Copy GSUB table from one font to another
    CopyGsub {
        /// Source font to copy GSUB table from
        #[arg(long)]
        from: PathBuf,
        /// Target font to copy GSUB table to
        #[arg(long)]
        to: PathBuf,
    },
    /// Remove triple-backtick ligature from fonts
    RemoveLigatures {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Set monospace flags (post.isFixedPitch, OS/2 panose)
    SetMonospace {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Set font version date in name and head tables
    SetVersion {
        /// Version string (YYYY-MM-DD or YYYY-MM-DD.N format)
        #[arg(short, long)]
        version: Option<String>,
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Subset font to Japanese Unicode ranges
    SubsetJapanese {
        #[arg(required = true)]
        input: PathBuf,
        #[arg(required = true)]
        output: PathBuf,
    },
    /// Freeze OpenType features into fonts permanently
    Freeze {
        /// Comma-separated list of features to freeze (e.g., ss01,ss02)
        #[arg(short, long, value_delimiter = ',')]
        features: Vec<String>,
        /// Automatically enable rvrn feature for variable fonts
        #[arg(long)]
        auto_rvrn: bool,
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Create static instance from variable font
    Instance {
        /// Axis location in TAG=VALUE format (e.g., wght=700)
        #[arg(short, long = "axis", value_parser = parse_axis)]
        axes: Vec<AxisLocation>,
        #[arg(required = true)]
        input: PathBuf,
        #[arg(required = true)]
        output: PathBuf,
    },
    /// Create multiple static instances from variable font
    InstanceBatch {
        /// Input variable font
        #[arg(long)]
        input: PathBuf,
        /// Output directory for generated instances
        #[arg(long, default_value = "dist")]
        output_dir: PathBuf,
        /// Instance definition in NAME:TAG=VAL,TAG=VAL format
        #[arg(short, long = "instance", value_parser = parse_instance_def)]
        instances: Vec<(String, Vec<AxisLocation>)>,
    },
    /// Merge multiple fonts into one
    Merge {
        /// Input fonts (first is base, rest are merged in order)
        #[arg(required = true, num_args = 2..)]
        inputs: Vec<PathBuf>,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Merge multiple base fonts with a fallback font
    MergeBatch {
        /// Base fonts to merge fallback into
        #[arg(required = true)]
        base_fonts: Vec<PathBuf>,
        /// Fallback font to merge into each base font
        #[arg(short, long)]
        fallback: PathBuf,
        /// Output directory for merged fonts
        #[arg(short, long, default_value = "dist")]
        output_dir: PathBuf,
    },
    /// Create WarpnineSans fonts from Recursive VF
    CreateSans {
        /// Input Recursive variable font
        #[arg(long)]
        input: PathBuf,
        /// Output directory for generated fonts
        #[arg(long, default_value = "dist")]
        output_dir: PathBuf,
    },
    /// Create WarpnineSansCondensed fonts from Recursive VF
    CreateCondensed {
        /// Input Recursive variable font
        #[arg(long)]
        input: PathBuf,
        /// Output directory for generated fonts
        #[arg(long, default_value = "dist")]
        output_dir: PathBuf,
        /// Horizontal scale factor (0.90 = 90% width)
        #[arg(long, default_value = "0.90")]
        scale: f32,
    },
    /// Set name table entries (family, style, copyright)
    SetName {
        /// Font family name
        #[arg(long)]
        family: String,
        /// Font style name (e.g., Regular, Bold)
        #[arg(long)]
        style: String,
        /// PostScript family name (defaults to family with spaces removed)
        #[arg(long)]
        postscript_family: Option<String>,
        /// Additional copyright text to append
        #[arg(long)]
        copyright_extra: Option<String>,
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Fix calt/rclt feature registration across all scripts
    FixCalt {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Build WarpnineMono variable font from static masters
    BuildVf {
        /// Directory containing static master fonts
        #[arg(long, default_value = "dist")]
        dist_dir: PathBuf,
        /// Output path for variable font
        #[arg(long, default_value = "dist/WarpnineMono-VF.ttf")]
        output: PathBuf,
    },
    /// Generate sample PDF using typst
    GenerateSample {
        /// Directory containing fonts
        #[arg(long, default_value = "dist")]
        font_dir: PathBuf,
        /// Output PDF path
        #[arg(long, default_value = "docs/sample.pdf")]
        output: PathBuf,
        /// Watch for changes and recompile
        #[arg(long, short)]
        watch: bool,
        /// Also export first page as PNG for README
        #[arg(long)]
        png: bool,
    },
}

fn parse_axis(s: &str) -> Result<AxisLocation, String> {
    let (tag, value_str) = s
        .split_once('=')
        .ok_or_else(|| format!("Invalid axis format '{s}', expected TAG=VALUE"))?;
    let value: f32 = value_str
        .parse()
        .map_err(|_| format!("Invalid value '{value_str}' for axis '{tag}'"))?;
    Ok(AxisLocation::new(tag, value))
}

fn parse_instance_def(s: &str) -> Result<(String, Vec<AxisLocation>), String> {
    let (name, axes_str) = s
        .split_once(':')
        .ok_or_else(|| format!("Expected NAME:TAG=VAL,TAG=VAL format, got '{s}'"))?;
    let axes: Result<Vec<_>, String> = axes_str.split(',').map(parse_axis).collect();
    Ok((name.to_string(), axes?))
}

impl DevCommands {
    pub fn run(self) -> Result<()> {
        match self {
            DevCommands::CopyGsub { from, to } => {
                let source_data = read_font(&from)?;
                let target_data = read_font(&to)?;
                let new_data = copy_table(&source_data, &target_data, Tag::new(b"GSUB"))?;
                write_font(&to, new_data)?;
                println!("Copied GSUB table from {} to {}", from.display(), to.display());
            }
            DevCommands::RemoveLigatures { files } => {
                run_parallel("Remove ligatures", &files, |path| {
                    remove_grave_ligature(path)?;
                    Ok(())
                })?;
            }
            DevCommands::SetMonospace { files } => {
                run_parallel("Set monospace", &files, |path| {
                    transform_font_in_place(path, |data| MonospaceSettings::DEFAULT.apply(data))
                })?;
            }
            DevCommands::SetVersion { version, files } => {
                let font_version = FontVersion::parse(version.as_deref())?;
                let version_tag = font_version.tag.clone();
                run_parallel(&format!("Set version {version_tag}"), &files, |path| {
                    transform_font_in_place(path, |data| font_version.apply(data))
                })?;
            }
            DevCommands::SubsetJapanese { input, output } => {
                let data = read_font(&input)?;
                let subset_data = Subsetter::japanese()
                    .exclude_codepoints([
                        0x25CB, // ○ WHITE CIRCLE
                        0x25CF, // ● BLACK CIRCLE
                    ])
                    .subset(&data)?;
                write_font(&output, subset_data)?;
                println!("Subset {} -> {}", input.display(), output.display());
            }
            DevCommands::Freeze { features, auto_rvrn, files } => {
                let auto_rvrn = if auto_rvrn { AutoRvrn::Enabled } else { AutoRvrn::Disabled };
                freeze_features(&files, &features, auto_rvrn)?;
            }
            DevCommands::Instance { axes, input, output } => {
                create_instance(&input, &output, &axes)?;
            }
            DevCommands::InstanceBatch { input, output_dir, instances } => {
                let defs: Vec<InstanceDef> = instances
                    .into_iter()
                    .map(|(name, axes)| InstanceDef::new(name, axes))
                    .collect();
                create_instances_batch(&input, &output_dir, &defs)?;
            }
            DevCommands::Merge { inputs, output } => {
                merge_fonts(&inputs, &output)?;
            }
            DevCommands::MergeBatch { base_fonts, fallback, output_dir } => {
                merge_batch(&base_fonts, &fallback, &output_dir)?;
            }
            DevCommands::CreateSans { input, output_dir } => {
                create_sans(&input, &output_dir)?;
            }
            DevCommands::CreateCondensed { input, output_dir, scale } => {
                create_condensed(&input, &output_dir, scale)?;
            }
            DevCommands::SetName {
                family,
                style,
                postscript_family,
                copyright_extra,
                files,
            } => {
                let naming = FontNaming { family, style, postscript_family, copyright_extra };
                run_parallel("Set name", &files, |path| set_name(path, &naming))?;
            }
            DevCommands::FixCalt { files } => {
                run_parallel("Fix calt", &files, fix_calt_registration)?;
            }
            DevCommands::BuildVf { dist_dir, output } => {
                build_warpnine_mono_vf(&dist_dir, &output)?;
            }
            DevCommands::GenerateSample { font_dir, output, watch, png } => {
                generate_sample(&font_dir, &output, watch, png)?;
            }
        }
        Ok(())
    }
}

fn generate_sample(font_dir: &PathBuf, output: &PathBuf, watch: bool, png: bool) -> Result<()> {
    use std::process::Command;

    let docs_dir = PathBuf::from("docs");
    let sample_typ = docs_dir.join("sample.typ");
    let subcommand = if watch { "watch" } else { "compile" };

    let mut command = Command::new("typst");
    command
        .arg(subcommand)
        .arg("--ignore-system-fonts")
        .arg("--font-path")
        .arg(font_dir)
        .arg(&sample_typ)
        .arg(output);
    let status = run_typst(&mut command)?;

    if !status.success() {
        bail!("typst {subcommand} failed with exit code: {:?}", status.code());
    }

    if !watch {
        println!("Generated: {}", output.display());
    }

    if png && !watch {
        // Page 1 → sample.png (Latin overview), page 2 → sample-jp.png (Japanese specimen).
        for (page, name) in [("1", "sample.png"), ("2", "sample-jp.png")] {
            let png_output = docs_dir.join(name);
            let mut command = Command::new("typst");
            command
                .arg("compile")
                .arg("--ignore-system-fonts")
                .arg("--font-path")
                .arg(font_dir)
                .arg("--pages")
                .arg(page)
                .arg("--ppi")
                .arg("288")
                .arg(&sample_typ)
                .arg(&png_output);
            let status = run_typst(&mut command)?;

            if !status.success() {
                bail!("typst compile (PNG) failed with exit code: {:?}", status.code());
            }
            println!("Generated: {}", png_output.display());
        }
    }

    Ok(())
}

fn run_typst(command: &mut std::process::Command) -> Result<std::process::ExitStatus> {
    command.status().with_context(
        || "failed to run `typst`; install Typst and ensure the `typst` executable is on PATH",
    )
}

#[cfg(test)]
mod sample_tests {
    use super::*;

    #[test]
    fn missing_typst_command_has_actionable_error() {
        let mut command = std::process::Command::new("warpnine-command-that-does-not-exist");
        let error = run_typst(&mut command).unwrap_err();
        assert!(error.to_string().contains("install Typst"));
        assert!(error.to_string().contains("PATH"));
    }
}
