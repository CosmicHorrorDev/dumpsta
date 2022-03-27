use clap::Parser;

#[derive(Parser)]
pub struct Args {
    /// Check how many crates would be downloaded without downloading
    #[clap(short, long)]
    pub dry_run: bool,
    /// Number of threads used to scan the index [Default: NUM_CPUS]
    #[clap(short, long, default_value_t = 0, hide_default_value = true)]
    pub threads: usize,
}
