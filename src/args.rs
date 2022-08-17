use clap::{Parser, Subcommand};
use clap_verbosity_flag::{Verbosity, WarnLevel};
use const_format::formatcp;
use std::path::PathBuf;

const LICENSE: &str = "Copyright (C) 2022 Eliza Velasquez
License GPLv3+: GNU GPL version 3 or later <https://gnu.org/licenses/gpl.html>
This is free software: you are free to change and redistribute it.
There is NO WARRANTY, to the extent permitted by law.";

const VERSION: &str = formatcp!("{}\n{}", clap::crate_version!(), LICENSE);

#[derive(Parser, Debug)]
#[clap(author, version, long_version = VERSION, about, long_about = None)]
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

    #[clap(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Push mail without pulling changes.
    Push,
    /// Synchronize mail.
    Sync,
    /// Send mail.
    Send {
        /// Ignored sendmail-compatible flag.
        #[clap(short = 'i')]
        sendmail1: bool,
        /// Ignored sendmail-compatible flag.
        #[clap(short = 'f', name = "NAME")]
        sendmail2: Option<String>,
        /// Ignored sendmail-compatible flag.
        #[clap(short = 'F', name = "FULLNAME")]
        sendmail3: Option<String>,
        /// Read the message to obtain recipients.
        ///
        /// If specified, the recipient arguments are ignored.
        #[clap(short = 't', long)]
        read_recipients: bool,
        /// Email addresses of the recipients of the message.
        recipients: Vec<String>,
    },
}
