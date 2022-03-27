use std::{
    collections::{BTreeSet, HashSet},
    env,
    ffi::OsString,
    fs::{self, File},
    hash::{Hash, Hasher},
    io::{self, BufReader},
    iter,
    ops::Deref,
    path::{Path, PathBuf},
    thread::sleep,
    time::Duration,
};

use anyhow::{Context, Result};
use colored::Colorize;
use crates_index::{Dependency, Index, Version};
use flate2::bufread::GzDecoder;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use tar::Archive;

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

#[derive(Clone, Copy, Default)]
#[must_use]
struct Opts {
    level: Level,
    indent: usize,
}

impl Opts {
    fn level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }

    fn indent(mut self, indent: usize) -> Self {
        self.indent = indent;
        self
    }

    fn inc_indent(mut self) -> Self {
        self.indent += 1;
        self
    }
}

#[derive(Clone, Copy)]
enum Level {
    Info,
    Warn,
    Error,
}

impl Default for Level {
    fn default() -> Self {
        Self::Info
    }
}

// TODO: Stuff using this gets pretty verbose. Work on:
// - Making nesting more natural
//   - use start actions and continuing actions
// - Take a template string to avoid `format`ing every time
// - Extend the color trait to apply to specific colors to specific types
fn eprintln_action_cont(msg: &str, opts: Opts) {
    let Opts { level, indent } = opts;

    let arrow = match level {
        Level::Info => "->".blue(),
        Level::Warn => "->".magenta(),
        Level::Error => "->".red(),
    }
    .bold();

    let indent: String = iter::repeat("  ").take(indent).collect();
    eprintln!("{}{} {}", indent, arrow, msg.bold());
}

fn main() -> Result<()> {
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
    let msg = format!(
        "{} Found {} crates using `insta`!",
        "->".blue(),
        uses_insta.len().to_string().blue()
    );
    eprintln!("{}", msg.bold());

    eprintln!("{}", "Scanning locally downloaded crates...".bold());
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
        eprintln_action_cont("No crates to download!", Opts::default());
    } else {
        let msg = format!(
            "{} crates to download",
            download_urls.len().to_string().blue()
        );
        eprintln_action_cont(&msg, Opts::default());
    }

    eprintln!("{}", "Downloading crates...".bold());
    let cargo_registry = CargoRegistry::new()?;
    let cache_path = cargo_registry.cache();
    let src_path = cargo_registry.src();
    let info_opts = Opts::default().inc_indent();
    let warn_opts = info_opts.level(Level::Warn);
    for url in &download_urls {
        sleep(Duration::from_millis(200));
        eprintln_action_cont(&format!("Downloading {}...", url.cyan()), info_opts);

        let info_opts = info_opts.inc_indent();
        let warn_opts = warn_opts.inc_indent();

        let resp = match ureq::get(url).call() {
            Ok(resp) => resp,
            Err(err) => {
                eprintln_action_cont(
                    &format!(
                        "Error downloading file: {}, Err: {}",
                        url.cyan(),
                        err.to_string().red(),
                    ),
                    warn_opts,
                );
                continue;
            }
        };

        let file_name = resp.get_url().rsplit_once('/').unwrap().1.to_owned();
        let download_path = cache_path.join(&file_name);
        let mut download_file = match File::create(&download_path) {
            Ok(file) => file,
            Err(err) => {
                eprintln_action_cont(
                    &format!(
                        "Failed creating file: {}, Err: {}",
                        download_path.to_string_lossy().cyan(),
                        err.to_string().red()
                    ),
                    warn_opts,
                );
                continue;
            }
        };

        let mut reader = BufReader::new(resp.into_reader());
        match io::copy(&mut reader, &mut download_file) {
            Ok(_) => eprintln_action_cont(&format!("Downloaded {}", file_name.cyan()), info_opts),
            Err(err) => {
                eprintln_action_cont(
                    &format!(
                        "Failed downloading file: {}, Err: {}",
                        file_name.cyan(),
                        err.to_string().red()
                    ),
                    warn_opts,
                );
                continue;
            }
        }

        let reader = match File::open(&download_path) {
            Ok(file) => file,
            Err(err) => {
                eprintln_action_cont(
                    &format!(
                        "Failed opening file: {}, Err: {}",
                        download_path.to_string_lossy().cyan(),
                        err.to_string().red()
                    ),
                    warn_opts,
                );
                continue;
            }
        };

        let decompressor = GzDecoder::new(BufReader::new(reader));
        let mut archive = Archive::new(decompressor);
        match archive.unpack(&src_path) {
            Ok(_) => eprintln_action_cont(&format!("Extracted {}", file_name.cyan()), info_opts),
            Err(err) => {
                eprintln_action_cont(
                    &format!(
                        "Failed extracting file: {}, Err: {}",
                        download_path.to_string_lossy().cyan(),
                        err.to_string().red()
                    ),
                    warn_opts,
                );
                continue;
            }
        };
    }

    Ok(())
}
