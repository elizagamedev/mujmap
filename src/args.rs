use clap::Parser;
use clap_verbosity_flag::{Verbosity, WarnLevel};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
pub struct Args {
    /// Path to config file.
    ///
    /// Defaults to the current working directory.
    #[clap(short = 'C', long)]
    pub path: Option<PathBuf>,

    /// Test a sync without committing any changes.
    #[clap(short, long)]
    pub dry_run: bool,

    #[clap(flatten)]
    pub verbose: Verbosity<WarnLevel>,
}
