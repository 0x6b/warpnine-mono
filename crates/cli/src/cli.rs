//! CLI definitions and command dispatch.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use warpnine_core::{
    build_all, build_condensed, build_mono, build_sans,
    pipeline::{clean, download},
};

use crate::dev::DevCommands;

#[derive(Parser)]
#[command(about, version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Clone, clap::Args)]
pub struct BuildArgs {
    /// Directory for intermediate build files
    #[arg(long, default_value = "build")]
    pub build_dir: PathBuf,
    /// Directory for output font files
    #[arg(long, default_value = "dist")]
    pub dist_dir: PathBuf,
    /// Version string (YYYY-MM-DD or YYYY-MM-DD.N format)
    #[arg(short, long)]
    pub version: String,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Build all fonts (Mono, Sans, and Condensed variants)
    Build {
        #[command(flatten)]
        args: BuildArgs,
    },
    /// Build WarpnineMono fonts only (static + variable)
    BuildMono {
        #[command(flatten)]
        args: BuildArgs,
    },
    /// Build WarpnineSans fonts only (proportional sans-serif, static + variable)
    BuildSans {
        #[command(flatten)]
        args: BuildArgs,
    },
    /// Build WarpnineSansCondensed fonts only (90% width, static + variable)
    BuildCondensed {
        #[command(flatten)]
        args: BuildArgs,
    },
    /// Download source fonts (Recursive VF and Noto CJK)
    Download {
        /// Directory for intermediate build files
        #[arg(long, default_value = "build")]
        build_dir: PathBuf,
    },
    /// Remove build artifacts (build/ and dist/ directories)
    Clean {
        /// Directory for intermediate build files
        #[arg(long, default_value = "build")]
        build_dir: PathBuf,
        /// Directory for output font files
        #[arg(long, default_value = "dist")]
        dist_dir: PathBuf,
    },
    #[command(subcommand, hide = true)]
    Dev(DevCommands),
}

impl Commands {
    pub fn run(self) -> Result<()> {
        match self {
            Commands::Build { args } => build_all(&args.build_dir, &args.dist_dir, args.version),
            Commands::BuildMono { args } => {
                build_mono(&args.build_dir, &args.dist_dir, args.version)
            }
            Commands::BuildSans { args } => {
                build_sans(&args.build_dir, &args.dist_dir, args.version)
            }
            Commands::BuildCondensed { args } => {
                build_condensed(&args.build_dir, &args.dist_dir, args.version)
            }
            Commands::Download { build_dir } => download(&build_dir),
            Commands::Clean { build_dir, dist_dir } => clean(&build_dir, &dist_dir),
            Commands::Dev(dev) => dev.run(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn build_requires_explicit_version() {
        let error = match Cli::try_parse_from(["warpnine-fonts", "build"]) {
            Ok(_) => panic!("build without --version should fail"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
        assert!(
            Cli::try_parse_from(["warpnine-fonts", "build", "--version", "2026-07-11",]).is_ok()
        );
    }
}
