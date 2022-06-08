#![doc = include_str!("../README.md")]

/// Command line arguments.
mod args;
/// Local cache interface.
mod cache;
/// Configuration file options.
mod config;
/// Miniature JMAP API.
mod jmap;
/// Local notmuch database interface.
mod local;
/// Remote JMAP interface.
mod remote;
/// Send command.
mod send;
/// Sync command.
mod sync;

use args::Args;
use atty::Stream;
use clap::Parser;
use config::Config;
use log::debug;
use send::send;
use snafu::prelude::*;
use std::path::PathBuf;
use std::{env, io::Write};
use sync::sync;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Could not open config file: {}", source))]
    OpenConfigFile { source: config::Error },

    #[snafu(display("Could not sync mail: {}", source))]
    Sync { source: sync::Error },

    #[snafu(display("Could not send mail: {}", source))]
    Send { source: send::Error },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

fn try_main(stdout: &mut StandardStream) -> Result<(), Error> {
    // HACK: Remove -oi from the command-line arguments. If someone is weird enough to have named
    // their maildir "-oi", or something like that, this would cause mujmap to fail unnecessarily.
    // However, clap does not yet support "long" arguments with more than one character, so this is
    // our best option. See: https://github.com/clap-rs/clap/issues/1210
    let args = Args::parse_from(env::args().into_iter().filter(|a| a != "-oi"));

    env_logger::Builder::new()
        .filter_level(args.verbose.log_level_filter())
        .parse_default_env()
        .init();

    let info_color_spec = ColorSpec::new()
        .set_fg(Some(Color::Green))
        .set_bold(true)
        .to_owned();

    // Determine working directory and load all data files.
    let config_dir = args.path.clone().unwrap_or_else(|| PathBuf::from("."));

    let config = Config::from_dir(&config_dir).context(OpenConfigFileSnafu {})?;
    debug!("Using config: {:?}", config);

    match args.command {
        args::Command::Sync => sync(stdout, info_color_spec, args, config).context(SyncSnafu {}),
        args::Command::Send {
            read_recipients,
            recipients,
            ..
        } => send(read_recipients, recipients, config).context(SendSnafu {}),
    }
}

fn main() {
    let mut stdout = StandardStream::stdout(if atty::is(Stream::Stdout) {
        ColorChoice::Auto
    } else {
        ColorChoice::Never
    });
    let mut stderr = StandardStream::stderr(if atty::is(Stream::Stderr) {
        ColorChoice::Auto
    } else {
        ColorChoice::Never
    });

    std::process::exit(match try_main(&mut stdout) {
        Ok(_) => 0,
        Err(err) => {
            stderr
                .set_color(ColorSpec::new().set_fg(Some(Color::Red)))
                .ok();
            writeln!(&mut stderr, "error: {err}").ok();
            1
        }
    });
}
