use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs::File,
    hash::{Hash, Hasher},
    io::{self, BufReader},
    num::NonZeroUsize,
    ops::Deref,
    path::{Path, PathBuf},
    thread::sleep,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use crates_index::{Index, Version};
use flate2::bufread::GzDecoder;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::{prelude::*, ThreadPoolBuilder};
use tar::Archive;

mod cli;
mod dialog;

use dialog::{disps, Dialog};

#[derive(Debug, Clone)]
struct CargoRegistry {
    base: PathBuf,
    index_name: OsString,
}

impl CargoRegistry {
    pub fn new() -> Result<Self> {
        let base = match env::var_os("CARGO_HOME") {
            Some(home) => PathBuf::from(home),
            None => {
                let home_dir = dirs::home_dir().context("Failed to get home dir")?;
                PathBuf::from(home_dir).join(".cargo")
            }
        }
        .join("registry");

        let index_name = base
            .join("cache")
            .read_dir()
            .ok()
            .and_then(|mut dirs| match dirs.next() {
                Some(Ok(entry)) => Some(entry.file_name()),
                _ => None,
            })
            .context("Cargo home doesn't seem to exist :(")?;

        Ok(CargoRegistry { base, index_name })
    }

    pub fn cache(&self) -> PathBuf {
        self.sub_dir("cache")
    }

    pub fn index(&self) -> PathBuf {
        self.sub_dir("index")
    }

    pub fn src(&self) -> PathBuf {
        self.sub_dir("src")
    }

    fn sub_dir(&self, dir: &'static str) -> PathBuf {
        self.base.join(dir).join(&self.index_name)
    }
}

#[derive(Debug, Clone)]
struct LocalCrates {
    listing: BTreeSet<String>,
}

impl LocalCrates {
    fn new() -> Result<Self> {
        // TODO: don't hardcode this anymore
        let path =
            Path::new("/home/wintermute/.data/cargo/registry/src/github.com-1ecc6299db9ec823/");

        // Read over entries ignoring any errors
        let listing = path
            .read_dir()?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().map(ToOwned::to_owned))
            .collect();
        Ok(Self { listing })
    }

    fn contains(&self, version: &VersionExt) -> bool {
        let key = format!("{}-{}", version.name(), version.version());
        self.listing.contains(&key)
    }
}

#[derive(Debug, Clone)]
struct VersionExt(Version);

impl VersionExt {
    fn new(version: Version) -> Self {
        Self(version)
    }

    fn inner(&self) -> &Version {
        &self.0
    }
}

impl From<Version> for VersionExt {
    fn from(version: Version) -> Self {
        Self::new(version)
    }
}

impl Deref for VersionExt {
    type Target = Version;

    fn deref(&self) -> &Self::Target {
        self.inner()
    }
}

impl PartialEq for VersionExt {
    fn eq(&self, other: &Self) -> bool {
        self.name() == other.name() && self.version() == other.version()
    }
}

impl Hash for VersionExt {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name().hash(state);
        self.version().hash(state);
    }
}

fn err(err: impl std::error::Error + Send + Sync + 'static) -> anyhow::Error {
    err.into()
}

fn main() -> Result<()> {
    let cli::Args { dry_run, threads } = cli::Args::parse();

    ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()?;

    let spinner = ProgressBar::new(1).with_style(
        ProgressStyle::default_spinner()
            .template("{elapsed:>3.green.bold} {spinner:.blue.bold} {msg:!.bold}"),
    );
    spinner.set_message("Finding all current crates using `insta`...");
    spinner.enable_steady_tick(100);

    // Get any new versions we haven't seen before
    let index = Index::new_cargo_default()?;
    let new_versions = index
        .crates_parallel()
        .filter_map(|maybe_krate| maybe_krate.ok())
        .map(|krate| krate.highest_version().to_owned())
        .map(VersionExt::from);

    let uses_insta: Vec<_> = new_versions
        .filter(|version| {
            version
                .dependencies()
                .iter()
                .any(|dep| dep.crate_name() == "insta")
        })
        .collect();

    spinner.finish();
    Dialog::raw_with_indent(NonZeroUsize::new(1).unwrap())
        .info_with("Found {} crates using `insta`!", disps![uses_insta.len()]);

    let scan_dialog = Dialog::new("Scanning locally downloaded crates...");
    // See if the crate is already downloaded in
    // $CARGO_HOME/registry/src/github.com-1ecc6299db9ec823
    // If it is then search that, otherwise download it in memory and extract it while filtering
    // for any `.snap` files
    let local_crates = LocalCrates::new()?;
    let config = index.index_config()?;
    let download_urls: Vec<_> = uses_insta
        .into_iter()
        .map(VersionExt::from)
        .filter(|version| !local_crates.contains(version))
        .filter_map(|version| version.download_url(&config))
        .collect();
    if download_urls.len() == 0 {
        scan_dialog.info("No crates to download!");
    } else {
        scan_dialog.info_with("{} crates to download", disps![download_urls.len()]);
    }

    if dry_run {
        Dialog::new("Finished dry run!");
        return Ok(());
    }

    let full_dl_dialog = Dialog::new("Downloading crates...");
    let cargo_registry = CargoRegistry::new()?;
    let cache_path = cargo_registry.cache();
    let src_path = cargo_registry.src();
    for url in &download_urls {
        sleep(Duration::from_millis(200));
        let crate_dl_dialog = full_dl_dialog.info_with("Downloading {}...", disps![url]);

        let resp = match ureq::get(url).call() {
            Ok(resp) => resp,
            Err(e) => {
                crate_dl_dialog
                    .warn_with("Error downloading file: {}, Err: {}", disps![url, err(e)]);
                continue;
            }
        };

        let file_name = resp.get_url().rsplit_once('/').unwrap().1.to_owned();
        let dl_path = cache_path.join(&file_name);
        let mut dl_file = match File::create(&dl_path) {
            Ok(file) => file,
            Err(e) => {
                crate_dl_dialog
                    .warn_with("Failed creating file: {}, Err: {}", disps![dl_path, err(e)]);
                continue;
            }
        };

        let mut reader = BufReader::new(resp.into_reader());
        match io::copy(&mut reader, &mut dl_file) {
            Ok(_) => crate_dl_dialog.info_with("Downloaded {}", disps![file_name.clone()]),
            Err(e) => {
                crate_dl_dialog.warn_with(
                    "Failed downloading file: {}, Err: {}",
                    disps![file_name, err(e)],
                );
                continue;
            }
        };

        let reader = match File::open(&dl_path) {
            Ok(file) => file,
            Err(e) => {
                crate_dl_dialog
                    .warn_with("Failed opening file: {}, Err: {}", disps![dl_path, err(e)]);
                continue;
            }
        };

        let decompressor = GzDecoder::new(BufReader::new(reader));
        let mut archive = Archive::new(decompressor);
        match archive.unpack(&src_path) {
            Ok(_) => crate_dl_dialog.info_with("Extracted {}", disps![file_name]),
            Err(e) => {
                crate_dl_dialog.warn_with(
                    "Failed extracting file: {}, Err: {}",
                    disps![dl_path, err(e)],
                );
                continue;
            }
        };
    }

    Ok(())
}
