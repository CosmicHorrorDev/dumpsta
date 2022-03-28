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
use colored::{Color, Colorize};
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
        let registry = CargoRegistry::new()?;

        // Read over entries ignoring any errors
        let listing = registry
            .src()
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

// TODO: check cached and extracted files
// TODO: Add a flag for force updating the index
// - Have this store a flag and limit. We don't need people to force updates all the time
// TODO: display the error with our `Dialog` stuff
// TODO: spinner on potentially pulling the index?
// TODO: Add a check to avoid scanning the full index
// - A simple timestamp on the last check should be enough
// TODO: Have a default out dir and an option to override
// TODO: Check if installed, then cached, then download if needed
fn main() -> Result<()> {
    let cli::Args { dry_run, threads } = cli::Args::parse();

    ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()?;

    let spinner = ProgressBar::new(1).with_style(
        ProgressStyle::default_spinner()
            .template("{elapsed:>3.green.bold} {spinner:.blue.bold} {msg:!.bold}"),
    );
    spinner.set_message("Finding all current crates that use `insta`...");
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
    // $CARGO_HOME/registry/src/github.com-<hash>
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

    // let urls_iter = download_urls.iter();
    let pb = ProgressBar::new(download_urls.len() as u64).with_style(
        ProgressStyle::default_bar()
            .template(&format!(
                "{} {}{{pos:.cyan.bold}}{}{{len:.cyan.bold}}{} {}{{bar:60.blue}}{} {} {{eta:<3.green.bold}}",
                "Downloading:".bold(),
                "(".cyan().bold(),
                "/".cyan().bold(),
                ")".cyan().bold(),
                "|".bold(),
                "|".bold(),
                "eta".green().bold(),
            ))
            .progress_chars("█▉▊▋▌▍▎▏ "),
    );
    let full_dl_dialog = Dialog::new("Downloading crates...");
    let cargo_registry = CargoRegistry::new()?;
    let cache_path = cargo_registry.cache();
    let src_path = cargo_registry.src();
    let agent = ureq::builder()
        // Setting a description user agent per crates.io crawling policy
        .user_agent("dumpsta (github.com/LovecraftianHorror/dumpsta)")
        .build();
    let mut num_install_errors = 0;
    for url in pb.wrap_iter(download_urls.iter()) {
        // Performing at most one request per second per crates.io crawling policy
        sleep(Duration::from_secs(1));
        let (crate_dl_dialog, msg) = full_dl_dialog.info_str_with("Downloading {}...", disps![url]);
        pb.println(msg);

        let resp = match agent.get(url).call() {
            Ok(resp) => resp,
            Err(e) => {
                crate_dl_dialog
                    .warn_with("Error downloading file: {}, Err: {}", disps![url, err(e)]);
                num_install_errors += 1;
                continue;
            }
        };

        // TODO: combine together creating and downloading the file
        let file_name = resp.get_url().rsplit_once('/').unwrap().1.to_owned();
        let dl_path = cache_path.join(&file_name);
        let mut dl_file = match File::create(&dl_path) {
            Ok(file) => file,
            Err(e) => {
                crate_dl_dialog
                    .warn_with("Failed creating file: {}, Err: {}", disps![dl_path, err(e)]);
                num_install_errors += 1;
                continue;
            }
        };

        let mut reader = BufReader::new(resp.into_reader());
        match io::copy(&mut reader, &mut dl_file) {
            Ok(_) => {}
            Err(e) => {
                crate_dl_dialog.warn_with(
                    "Failed downloading file: {}, Err: {}",
                    disps![file_name, err(e)],
                );
                num_install_errors += 1;
                continue;
            }
        }

        // TODO: combine together opening and extracting the file
        let reader = match File::open(&dl_path) {
            Ok(file) => file,
            Err(e) => {
                crate_dl_dialog
                    .warn_with("Failed opening file: {}, Err: {}", disps![dl_path, err(e)]);
                num_install_errors += 1;
                continue;
            }
        };

        let decompressor = GzDecoder::new(BufReader::new(reader));
        let mut archive = Archive::new(decompressor);
        match archive.unpack(&src_path) {
            Ok(_) => {
                let (_, msg) = crate_dl_dialog.msg_str_with(
                    Color::Green,
                    "Downloaded and extracted {}",
                    disps![file_name],
                );
                pb.println(msg);
            }
            Err(e) => {
                crate_dl_dialog.warn_with(
                    "Failed extracting file: {}, Err: {}",
                    disps![dl_path, err(e)],
                );
                num_install_errors += 1;
                continue;
            }
        }
    }

    if num_install_errors != 0 {
        full_dl_dialog.warn_with("Failed pulling {} crates", disps![num_install_errors]);
    }

    Ok(())
}
