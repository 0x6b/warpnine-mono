use std::{
    fs::{create_dir_all, remove_file, rename, write},
    io::{Cursor, Read},
    iter::once,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use reqwest::blocking::get;
use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::config::{
    JETBRAINS_MONO_FILENAME, JETBRAINS_MONO_LICENSE_SHA256, JETBRAINS_MONO_LICENSE_URL,
    JETBRAINS_MONO_SHA256, JETBRAINS_MONO_ZIP_PATH, JETBRAINS_MONO_ZIP_URL,
    NOTO_CJK_LICENSE_SHA256, NOTO_CJK_LICENSE_URL, NOTO_CJK_VF_FILENAME, NOTO_CJK_VF_SHA256,
    NOTO_CJK_VF_URL, RECURSIVE_LICENSE_SHA256, RECURSIVE_LICENSE_URL, RECURSIVE_VF_FILENAME,
    RECURSIVE_VF_SHA256, RECURSIVE_ZIP_PATH, RECURSIVE_ZIP_URL,
};

struct DownloadItem {
    url: &'static str,
    output_name: &'static str,
    description: &'static str,
    sha256: &'static str,
}

const DOWNLOADS: &[DownloadItem] = &[
    DownloadItem {
        url: NOTO_CJK_VF_URL,
        output_name: NOTO_CJK_VF_FILENAME,
        description: "Noto Sans Mono CJK JP (Variable)",
        sha256: NOTO_CJK_VF_SHA256,
    },
    DownloadItem {
        url: NOTO_CJK_LICENSE_URL,
        output_name: "LICENSE-NotoSansCJK.txt",
        description: "Noto CJK License",
        sha256: NOTO_CJK_LICENSE_SHA256,
    },
    DownloadItem {
        url: RECURSIVE_LICENSE_URL,
        output_name: "LICENSE-Recursive.txt",
        description: "Recursive License (OFL)",
        sha256: RECURSIVE_LICENSE_SHA256,
    },
    DownloadItem {
        url: JETBRAINS_MONO_LICENSE_URL,
        output_name: "LICENSE-JetBrainsMono.txt",
        description: "JetBrains Mono License (OFL)",
        sha256: JETBRAINS_MONO_LICENSE_SHA256,
    },
];

fn download_file(item: &DownloadItem, output_dir: &Path) -> Result<()> {
    let target = output_dir.join(item.output_name);
    println!("Downloading {}", item.description);
    println!("  {}", item.output_name);

    let response = get(item.url).with_context(|| format!("Failed to fetch {}", item.url))?;
    let status = response.status();
    if !status.is_success() {
        bail!("HTTP {status} for {}", item.url);
    }

    let bytes = response.bytes()?;
    write_verified(&target, &bytes, item.sha256)?;

    let size_mb = bytes.len() as f64 / 1024.0 / 1024.0;
    println!("  Downloaded ({size_mb:.2} MB)");
    Ok(())
}

fn download_recursive_vf(output_dir: &Path) -> Result<()> {
    let target = output_dir.join(RECURSIVE_VF_FILENAME);
    println!("Downloading Recursive VF");
    println!("  {RECURSIVE_VF_FILENAME}");

    let response =
        get(RECURSIVE_ZIP_URL).with_context(|| format!("Failed to fetch {RECURSIVE_ZIP_URL}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("HTTP {status} for {RECURSIVE_ZIP_URL}");
    }

    let bytes = response.bytes()?;
    let cursor = Cursor::new(bytes.as_ref());
    let mut archive = ZipArchive::new(cursor).context("Failed to open zip archive")?;

    let mut file = archive
        .by_name(RECURSIVE_ZIP_PATH)
        .with_context(|| format!("File {RECURSIVE_ZIP_PATH} not found in zip"))?;

    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    write_verified(&target, &buffer, RECURSIVE_VF_SHA256)?;

    let size_mb = buffer.len() as f64 / 1024.0 / 1024.0;
    println!("  Downloaded ({size_mb:.2} MB)");
    Ok(())
}

fn download_jetbrains_mono(output_dir: &Path) -> Result<()> {
    let target = output_dir.join(JETBRAINS_MONO_FILENAME);
    println!("Downloading JetBrains Mono");
    println!("  {JETBRAINS_MONO_FILENAME}");

    let response = get(JETBRAINS_MONO_ZIP_URL)
        .with_context(|| format!("Failed to fetch {JETBRAINS_MONO_ZIP_URL}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("HTTP {status} for {JETBRAINS_MONO_ZIP_URL}");
    }

    let bytes = response.bytes()?;
    let cursor = Cursor::new(bytes.as_ref());
    let mut archive = ZipArchive::new(cursor).context("Failed to open zip archive")?;

    let mut file = archive
        .by_name(JETBRAINS_MONO_ZIP_PATH)
        .with_context(|| format!("File {JETBRAINS_MONO_ZIP_PATH} not found in zip"))?;

    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    write_verified(&target, &buffer, JETBRAINS_MONO_SHA256)?;

    let size_mb = buffer.len() as f64 / 1024.0 / 1024.0;
    println!("  Downloaded ({size_mb:.2} MB)");
    Ok(())
}

fn write_verified(target: &Path, bytes: &[u8], expected_sha256: &str) -> Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        bail!(
            "SHA-256 mismatch for {}: expected {expected_sha256}, got {actual}",
            target.display()
        );
    }

    let filename = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    let temporary = target.with_file_name(format!(".{filename}.{}.tmp", std::process::id()));
    write(&temporary, bytes).with_context(|| format!("Failed to write {}", temporary.display()))?;
    if let Err(error) = rename(&temporary, target) {
        let _ = remove_file(&temporary);
        return Err(error).with_context(|| {
            format!("Failed to rename {} to {}", temporary.display(), target.display())
        });
    }
    Ok(())
}

/// Download task type for parallel processing.
enum DownloadTask<'a> {
    File(&'a DownloadItem),
    RecursiveVf,
    JetBrainsMono,
}

pub fn download(build_dir: &Path) -> Result<()> {
    create_dir_all(build_dir)?;
    println!("Downloading fonts to {}", build_dir.display());

    let failure_count = AtomicUsize::new(0);

    // Build list of all download tasks
    let tasks: Vec<DownloadTask> = DOWNLOADS
        .iter()
        .map(DownloadTask::File)
        .chain(once(DownloadTask::RecursiveVf))
        .chain(once(DownloadTask::JetBrainsMono))
        .collect();

    let total_count = tasks.len();

    tasks.par_iter().for_each(|task| {
        let result = match task {
            DownloadTask::File(dl) => download_file(dl, build_dir),
            DownloadTask::RecursiveVf => download_recursive_vf(build_dir),
            DownloadTask::JetBrainsMono => download_jetbrains_mono(build_dir),
        };
        if let Err(e) = result {
            let name = match task {
                DownloadTask::File(dl) => dl.description,
                DownloadTask::RecursiveVf => "Recursive VF",
                DownloadTask::JetBrainsMono => "JetBrains Mono",
            };
            eprintln!("Error downloading {name}: {e:?}");
            failure_count.fetch_add(1, Ordering::Relaxed);
        }
    });

    let failures = failure_count.load(Ordering::Relaxed);
    let success_count = total_count - failures;

    println!("\nDownload Summary");
    println!("  Success: {success_count}");
    if failures > 0 {
        println!("  Failed:  {failures}");
        bail!("Some downloads failed");
    }

    println!("All files ready in {}/", build_dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{create_dir_all, read, remove_dir_all};

    #[test]
    fn verified_write_is_atomic_and_rejects_mismatches() {
        let dir =
            std::env::temp_dir().join(format!("warpnine-download-test-{}", std::process::id()));
        create_dir_all(&dir).unwrap();
        let target = dir.join("source.ttf");
        let bytes = b"font data";
        let digest = format!("{:x}", Sha256::digest(bytes));

        write_verified(&target, bytes, &digest).unwrap();
        assert_eq!(read(&target).unwrap(), bytes);

        let error = write_verified(&target, b"unexpected", &digest).unwrap_err();
        assert!(error.to_string().contains("SHA-256 mismatch"));
        assert_eq!(read(&target).unwrap(), bytes);
        assert!(!dir.join(format!(".source.ttf.{}.tmp", std::process::id())).exists());

        remove_dir_all(dir).unwrap();
    }
}
