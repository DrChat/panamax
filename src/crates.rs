use crate::download::{download, DownloadError};
use crate::mirror::{ConfigCrates, ConfigMirror};
use crate::progress_bar::{padded_prefix_message, progress_bar, ProgressBarMessage};
use git2::Repository;
use reqwest::header::HeaderValue;
use scoped_threadpool::Pool;
use serde::{Deserialize, Serialize};
use std::fs::read_dir;
use std::path::{Path, PathBuf};
use std::{
    fs,
    io::{self, BufRead, Cursor},
};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SyncError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Download error: {0}")]
    Download(#[from] DownloadError),
    #[error("JSON serialization error: {0}")]
    SerializeError(#[from] serde_json::Error),
    #[error("Git error: {0}")]
    GitError(#[from] git2::Error),
}
/// One entry found in a crates.io-index file.
/// These files are formatted as lines of JSON.
#[derive(Debug, Serialize, Deserialize)]
pub struct CrateEntry {
    name: String,
    vers: String,
    cksum: String,
    yanked: bool,
}

/// Download one single crate file.
pub fn sync_one_crate_entry(
    path: &Path,
    source: Option<&str>,
    retries: usize,
    crate_entry: &CrateEntry,
    user_agent: &HeaderValue,
) -> Result<(), DownloadError> {
    // If source is "https://crates.io/api/v1/crates" (the default, and thus a None here)
    // download straight from the static.crates.io CDN, to avoid bogging down crates.io itself
    // or affecting its statistics, and avoiding an extra redirect for each crate.
    let url = if let Some(source) = source {
        format!(
            "{}/{}/{}/download",
            source, crate_entry.name, crate_entry.vers
        )
    } else {
        format!(
            "https://static.crates.io/crates/{}/{}-{}.crate",
            crate_entry.name, crate_entry.name, crate_entry.vers
        )
    };

    let file_path = get_crate_path(path, &crate_entry.name, &crate_entry.vers)
        .ok_or_else(|| DownloadError::BadCrate(crate_entry.name.clone()))?;

    download(
        &url[..],
        &file_path,
        Some(&crate_entry.cksum),
        retries,
        false,
        user_agent,
    )
}

/// Synchronize the crate files themselves, using the index for a list of files.
// TODO: There are still many unwraps in the foreach sections. This needs to be fixed.
pub fn sync_crates_files(
    path: &Path,
    mirror: &ConfigMirror,
    crates: &ConfigCrates,
    user_agent: &HeaderValue,
) -> Result<(), SyncError> {
    let prefix = if cfg!(feature = "dev_reduced_crates") {
        padded_prefix_message(2, 3, "Syncing 'z' crates files")
    } else {
        padded_prefix_message(2, 3, "Syncing crates files")
    };

    // For now, assume successful crates.io-index download
    let repo_path = path.join("crates.io-index");
    let repo = Repository::open(&repo_path)?;

    // Set the crates.io URL, or None if default
    let crates_source = if crates.source == "https://crates.io/api/v1/crates" {
        None
    } else {
        Some(crates.source.as_ref())
    };

    // Find Reference for origin/master
    let origin_master = repo.find_reference("refs/remotes/origin/master")?;
    let origin_master_tree = origin_master.peel_to_tree()?;

    let master = repo.find_reference("refs/heads/master")?;
    let master_tree = master.peel_to_tree()?;

    // Perform a full scan if master and origin/master match
    let do_full_scan = origin_master.peel_to_commit()?.id() == master.peel_to_commit()?.id();

    // Diff between master and origin/master (i.e. everything since the last fetch)
    let diff = if do_full_scan {
        repo.diff_tree_to_tree(None, Some(&origin_master_tree), None)?
    } else {
        repo.diff_tree_to_tree(Some(&master_tree), Some(&origin_master_tree), None)?
    };

    // Run one pass to figure out a total count
    let mut count = 0;
    diff.foreach(
        &mut |delta, _| {
            let df = delta.new_file();
            let p = df.path().unwrap();
            if p == Path::new("config.json") {
                // Skip config.json, as it's the only file that's not a crate descriptor
                return true;
            }

            // DEV: if dev_reduced_crates is enabled, only download crates that start with z
            #[cfg(feature = "dev_reduced_crates")]
            {
                // Get file name, try-convert to string, check if starts_with z, unwrap, or false if None
                if !p
                    .file_name()
                    .and_then(|x| x.to_str())
                    .map(|x| x.starts_with('z'))
                    .unwrap_or(false)
                {
                    return true;
                }
            }

            let oid = df.id();
            if oid.is_zero() {
                // The crate was removed, continue to next crate
                return true;
            }
            let blob = repo.find_blob(oid).unwrap();
            let data = blob.content();
            count += Cursor::new(data).lines().count();
            true
        },
        None,
        None,
        None,
    )?;

    let (pb_thread, sender) = progress_bar(Some(count), prefix);

    let mut removed_crates = vec![];

    // Download crates multithreaded
    Pool::new(crates.download_threads as u32).scoped(|scoped| {
        diff.foreach(
            &mut |delta, _| {
                let df = delta.new_file();
                let p = df.path().unwrap();
                if p == Path::new("config.json") {
                    return true;
                }

                // DEV: if dev_reduced_crates is enabled, only download crates that start with z
                #[cfg(feature = "dev_reduced_crates")]
                {
                    // Get file name, try-convert to string, check if starts_with z, unwrap, or false if None
                    if !p
                        .file_name()
                        .and_then(|x| x.to_str())
                        .map(|x| x.starts_with('z'))
                        .unwrap_or(false)
                    {
                        return true;
                    }
                }

                // Get the data for this crate file
                let oid = df.id();
                if oid.is_zero() {
                    // The crate was removed, continue to next crate.
                    // Note that this does not include yanked crates.
                    removed_crates.push(p.to_path_buf());
                    return true;
                }
                let blob = repo.find_blob(oid).unwrap();
                let data = blob.content();

                // Download one crate for each of the versions in the crate file
                for line in Cursor::new(data).lines() {
                    let line = line.unwrap();
                    let c: CrateEntry = serde_json::from_str(&line).unwrap();
                    let s = sender.clone();
                    scoped.execute(move || {
                        match sync_one_crate_entry(
                            path,
                            crates_source,
                            mirror.retries,
                            &c,
                            user_agent,
                        ) {
                            Ok(())
                            | Err(DownloadError::NotFound {
                                status: _,
                                url: _,
                                data: _,
                            })
                            | Err(DownloadError::MismatchedHash {
                                expected: _,
                                actual: _,
                            }) => {}
                            Err(e) => {
                                s.send(ProgressBarMessage::Println(format!(
                                    "Downloading {} {} failed: {:?}",
                                    &c.name, &c.vers, e
                                )))
                                .expect("Channel send should not fail");
                            }
                        }
                        s.send(ProgressBarMessage::Increment)
                            .expect("progress bar increment error");
                    });
                }

                true
            },
            None,
            None,
            None,
        )
        .unwrap();
    });

    // Delete any removed crates
    for rc in removed_crates {
        // Try to remove the file, but ignore it if it doesn't exist
        let _ = fs::remove_file(repo_path.join(rc));
    }

    sender
        .send(ProgressBarMessage::Done)
        .expect("Channel send should not fail");
    pb_thread.join().expect("Thread join should not fail");

    Ok(())
}

/// Detect if the crates directory is using the old format.
pub fn is_new_crates_format(path: &Path) -> Result<bool, io::Error> {
    if !path.exists() {
        // Path doesn't exist, so we can start with a clean slate.
        return Ok(true);
    }

    for crate_dir in read_dir(path)? {
        let crate_dir = crate_dir?;
        if !crate_dir.file_type()?.is_dir() {
            // Ignore any files in the directory. Only look at other directories.
            continue;
        }

        let dir_name = crate_dir
            .file_name()
            .into_string()
            .map_err(|_| io::ErrorKind::Other)?;
        match dir_name.as_str() {
            // 1-letter crate names cannot be numbers, so this must be new format.
            "1" | "2" | "3" => continue,
            // 2-letter directories are used for crates longer than 3 characters.
            x if x.len() == 2 => continue,
            // Unrecognized directory found, might be crate in old format.
            _ => {
                return Ok(false);
            }
        };
    }

    Ok(true)
}

pub fn get_crate_path(
    mirror_path: &Path,
    crate_name: &str,
    crate_version: &str,
) -> Option<PathBuf> {
    let crate_path = match crate_name.len() {
        1 => PathBuf::from("1"),
        2 => PathBuf::from("2"),
        3 => PathBuf::from("3"),
        n if n >= 4 => {
            let first_two = crate_name.get(0..2)?;
            let second_two = crate_name.get(2..4)?;
            [first_two, second_two].iter().collect()
        }
        _ => return None,
    };

    Some(
        mirror_path
            .join("crates")
            .join(crate_path)
            .join(crate_name)
            .join(crate_version)
            .join(format!("{}-{}.crate", crate_name, crate_version)),
    )
}
