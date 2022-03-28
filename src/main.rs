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
use ureq::Agent;

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

fn reverse_dependents_for(index: &Index, crate_name: &str) -> Vec<VersionExt> {
    index
        .crates_parallel()
        .filter_map(|maybe_krate| maybe_krate.ok())
        .map(|krate| krate.highest_version().to_owned())
        .map(VersionExt::from)
        .filter(|version| {
            version
                .dependencies()
                .iter()
                .any(|dep| dep.crate_name() == crate_name)
        })
        .collect()
}

fn get_uninstalled_insta_dependents() -> Result<Vec<VersionExt>> {
    let spinner = ProgressBar::new(1).with_style(
        ProgressStyle::default_spinner()
            .template("{elapsed:>3.green.bold} {spinner:.blue.bold} {msg:!.bold}"),
    );
    spinner.set_message("Finding all current crates that use `insta`...");
    spinner.enable_steady_tick(100);
    let index = Index::new_cargo_default()?;
    let uses_insta = reverse_dependents_for(&index, "insta");
    spinner.finish();
    Dialog::raw_with_indent(NonZeroUsize::new(1).unwrap()).info_with(
        "Found {} crates that use `insta`!",
        disps![uses_insta.len()],
    );

    // Check to see which ones we already have installed
    let scan_dialog = Dialog::new("Scanning locally downloaded crates...");
    let local_crates = LocalCrates::new()?;
    let config = index.index_config()?;
    let to_download: Vec<_> = uses_insta
        .into_iter()
        .filter(|version| !local_crates.contains(version))
        .collect();
    if to_download.len() == 0 {
        scan_dialog.info("No crates to download!");
    } else {
        scan_dialog.info_with("{} crates to download", disps![to_download.len()]);
    }

    Ok(to_download)
}

fn download_crate(agent: &Agent, registry: &CargoRegistry, url: &str) -> Result<String> {
    // Download file
    let resp = agent.get(url).call()?;
    let file_name = resp.get_url().rsplit_once('/').unwrap().1.to_owned();
    let dl_path = registry.cache().join(&file_name);
    let mut dl_file = File::create(&dl_path)?;
    let mut reader = BufReader::new(resp.into_reader());
    io::copy(&mut reader, &mut dl_file)?;

    // Extract contents
    let reader = File::open(&dl_path)?;
    let decompressor = GzDecoder::new(BufReader::new(reader));
    let mut archive = Archive::new(decompressor);
    archive.unpack(&registry.src())?;

    Ok(file_name)
}

fn download_crates(urls: &[String]) -> Result<()> {
    let counter = format!(
        "{}{{pos:.cyan.bold}}{}{{len:.cyan.bold}}{}",
        "(".cyan().bold(),
        "/".cyan().bold(),
        ")".cyan().bold(),
    );
    let eta = format!("{} {{eta:<3.green.bold}}", "eta".green().bold());
    let pb = ProgressBar::new(urls.len() as u64).with_style(
        ProgressStyle::default_bar()
            .template(&format!(
                "{} {} {}{{bar:60.blue}}{} {}",
                "Downloading:".bold(),
                counter,
                "|".bold(),
                "|".bold(),
                eta,
            ))
            .progress_chars("█▉▊▋▌▍▎▏ "),
    );
    let full_dl_dialog = Dialog::new("Downloading crates...");
    let cargo_registry = CargoRegistry::new()?;
    let agent = ureq::builder()
        // Setting a description user agent per crates.io crawling policy
        .user_agent("dumpsta (github.com/LovecraftianHorror/dumpsta)")
        .build();
    let mut num_install_errors = 0;
    for url in pb.wrap_iter(urls.iter()) {
        // Performing at most one request per second per crates.io crawling policy
        sleep(Duration::from_secs(1));
        let (crate_dl_dialog, msg) = full_dl_dialog.info_str_with("Downloading {}...", disps![url]);
        pb.println(msg);

        match download_crate(&agent, &cargo_registry, &url) {
            Ok(file_name) => {
                let (_, msg) = crate_dl_dialog.msg_str_with(
                    Color::Green,
                    "Downloaded and extracted {}",
                    disps![file_name],
                );
                pb.println(msg);
            }
            Err(e) => {
                crate_dl_dialog.warn_with("Failed download for {}, Err: {}", disps![url, e]);
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

// TODO: check cached and extracted files
// TODO: Add a flag for force updating the index
// - Have this store a flag and limit. We don't need people to force updates all the time
// TODO: display the error with our `Dialog` stuff
// TODO: Add a check to avoid scanning the full index
// - A simple timestamp on the last check should be enough
// TODO: Have a default out dir and an option to override
// TODO: Check if installed, then cached, then download if needed
fn main() -> Result<()> {
    let cli::Args { dry_run, threads } = cli::Args::parse();

    ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()?;

    let to_download = get_uninstalled_insta_dependents()?;
    let index = Index::new_cargo_default()?;
    let config = index.index_config()?;
    let download_urls: Vec<_> = to_download
        .iter()
        .filter_map(|version| version.download_url(&config))
        .collect();

    if dry_run {
        Dialog::new("Finished dry run!");
        return Ok(());
    }

    download_crates(&download_urls)?;

    Ok(())
}
