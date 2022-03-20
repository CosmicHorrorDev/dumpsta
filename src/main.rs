use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs::File,
    hash::{Hash, Hasher},
    io::{self, BufReader},
    ops::Deref,
    path::{Path, PathBuf},
    thread::sleep,
    time::Duration,
};

use anyhow::{Context, Result};
use crates_index::{Index, Version};
use flate2::bufread::GzDecoder;
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

// TODO: store a persistent hashset that filters out any versions we've already seen. This should
// drastically speed up finding versions that use insta
fn main() -> Result<()> {
    env_logger::init();

    log::info!("Getting listing of crates to download...");
    let index = Index::new_cargo_default()?;
    // Get a list of all the crates that use `insta`
    let uses_insta = index
        .crates()
        .map(|krate| krate.highest_version().to_owned())
        .filter(|version| {
            version
                .dependencies()
                .iter()
                .any(|dep| dep.crate_name() == "insta")
        });

    // See if the crate is already downloaded in
    // $CARGO_HOME/registry/src/github.com-1ecc6299db9ec823
    // If it is then search that, otherwise download it in memory and extract it while filtering
    // for any `.snap` files
    let local_crates = LocalCrates::new()?;
    let config = index.index_config()?;
    let download_urls: Vec<_> = uses_insta
        .map(VersionExt::from)
        .filter(|version| !local_crates.contains(version))
        .filter_map(|version| version.download_url(&config))
        .collect();

    log::info!("    There are {} crates to download", download_urls.len());
    let cargo_registry = CargoRegistry::new()?;
    let cache_path = cargo_registry.cache();
    let src_path = cargo_registry.src();
    for download_url in &download_urls {
        sleep(Duration::from_millis(500));
        log::info!("Downloading {download_url}...");
        let resp = match ureq::get(download_url).call() {
            Ok(resp) => resp,
            Err(err) => {
                log::warn!("Error downloading file: {download_url}, Err: {err:?}");
                continue;
            }
        };

        let file_name = resp.get_url().rsplit_once('/').unwrap().1.to_owned();
        let download_path = cache_path.join(&file_name);
        let mut download_file = match File::create(&download_path) {
            Ok(file) => file,
            Err(err) => {
                log::warn!("Failed creating file: {download_path:?}, Err: {err:?}");
                continue;
            }
        };

        let mut reader = BufReader::new(resp.into_reader());
        match io::copy(&mut reader, &mut download_file) {
            Ok(_) => log::info!("    Downloaded: {file_name:?}"),
            Err(err) => {
                log::warn!("Failed downloading file: {download_path:?}, Err: {err:?}");
                continue;
            }
        }

        let reader = match File::open(&download_path) {
            Ok(file) => file,
            Err(err) => {
                log::warn!("Failed opening file: {download_path:?}, Err: {err:?}");
                continue;
            }
        };

        let decompressor = GzDecoder::new(BufReader::new(reader));
        let mut archive = Archive::new(decompressor);
        match archive.unpack(&src_path) {
            Ok(_) => log::info!("    Extracted: {file_name:?}"),
            Err(err) => {
                log::warn!("Failed extracting file: {download_path:?}, Err: {err:?}");
                continue;
            }
        };
    }

    Ok(())
}
