mod args;
mod config;
mod jmap;
mod local;
mod remote;

use args::Args;
use clap::Parser;
use config::Config;
use directories::ProjectDirs;
use local::Local;
use log::{info, warn};
use remote::Remote;
use serde::{Deserialize, Serialize};
use snafu::{prelude::*, Whatever};
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Could not read mujmore state file `{}': {}", filename.to_string_lossy(), source))]
    ReadStateFile {
        filename: PathBuf,
        source: io::Error,
    },

    #[snafu(display("Could not parse mujmore state file `{}': {}", filename.to_string_lossy(), source))]
    ParseStateFile {
        filename: PathBuf,
        source: serde_json::Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Serialize, Deserialize)]
pub struct State {
    /// Latest revision of the notmuch database since the last time mujmap was
    /// run.
    pub notmuch_revision: Option<u64>,
    /// Latest JMAP Email state returned by `Email/get`.
    pub jmap_state: Option<jmap::State>,
}

impl State {
    fn open(filename: impl AsRef<Path>) -> Result<Self> {
        let filename = filename.as_ref();
        let file = File::open(filename).context(ReadStateFileSnafu { filename })?;
        let reader = BufReader::new(file);
        serde_json::from_reader(reader).context(ParseStateFileSnafu { filename })
    }

    fn empty() -> Self {
        State {
            notmuch_revision: None,
            jmap_state: None,
        }
    }
}

fn try_main() -> Result<(), Whatever> {
    let args = Args::parse();

    env_logger::Builder::new()
        .filter_level(args.verbose.log_level_filter())
        .init();

    // Determine working directory and load all data files.
    let mail_dir = args.path.unwrap_or_else(|| PathBuf::from("."));

    let config = Config::from_file(mail_dir.join("mujmap.toml"))?;

    // Load the intermediary state.
    let state_filename = mail_dir.join("mujmap.state.json");
    let state = State::open(state_filename).unwrap_or_else(|e| {
        warn!("{}", e);
        State::empty()
    });

    // Open the local notmuch database.
    let project_dirs = ProjectDirs::from("sh.eliza", "", "mujmap").unwrap();
    let local = Local::open(mail_dir, project_dirs.cache_dir(), args.dry_run)
        .with_whatever_context(|source| format!("Could not open local state: {}", source))?;

    // Open the remote session.
    let mut remote = match &config.fqdn {
        Some(fqdn) => Remote::open_host(&fqdn, config.username.as_str(), &config.password()?),
        None => Remote::open_url(
            &config.session_url.as_ref().unwrap(),
            config.username.as_str(),
            &config.password()?,
        ),
    }
    .with_whatever_context(|source| format!("Could not open remote state: {}", source))?;

    // Query local database for all email.
    let local_email = local
        .all_email()
        .with_whatever_context(|source| format!("Could not query lcoal email: {}", source))?;

    // Function which performs a full sync, i.e. a sync which considers all
    // remote IDs as updated, and determines destroyed IDs by finding the
    // difference of all remote IDs from all local IDs.
    let full_sync = |remote: &mut Remote| -> Result<
        (jmap::State, HashSet<jmap::Id>, HashSet<jmap::Id>),
        Whatever,
    > {
        let (state, updated_ids) = remote.all_email_ids().with_whatever_context(|source| {
            format!(
                "Could not query all remote email IDs for a full sync: {}",
                source
            )
        })?;
        // TODO can we optimize these two lines?
        let local_ids: HashSet<jmap::Id> = local_email.iter().map(|(id, _)| id).cloned().collect();
        let destroyed_ids = local_ids.difference(&updated_ids).cloned().collect();
        Ok((state, updated_ids, destroyed_ids))
    };

    // Create lists of updated and destroyed `Email` IDs. This is done in one of
    // two ways, depending on if we have a working JMAP `Email` state.
    let (state, updated_ids, destroyed_ids) = state
        .jmap_state
        .map(|jmap_state| {
            match remote.updated_email_ids(jmap_state) {
                Ok(x) => Ok(x),
                Err(e) => {
                    // `Email/changes` failed, so fall back to `Email/query`.
                    warn!(
                        "Error while attempting to resolve changes, attempting full sync: {}",
                        e
                    );
                    full_sync(&mut remote)
                }
            }
        })
        .unwrap_or_else(|| full_sync(&mut remote))?;

    info!(
        "Discovered {} possible remote email updates, {} destroyed",
        updated_ids.len(),
        destroyed_ids.len()
    );

    // Retrieve the updated `Email` objects from the server.
    let (_, emails) = remote
        .get_emails(state.clone(), &updated_ids, config.email_get_chunk_size)
        .with_whatever_context(|source| {
            format!(
                "Could not retrieve email properties from remote: {}",
                source
            )
        })?;

    // Before merging, download the new files into the cache.

    Ok(())
}

fn main() {
    std::process::exit(match try_main() {
        Ok(_) => 0,
        Err(err) => {
            eprintln!("error: {}", err);
            1
        }
    });
}
