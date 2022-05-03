mod args;
mod cache;
mod config;
mod jmap;
mod local;
mod remote;

use args::Args;
use cache::Cache;
use clap::Parser;
use config::Config;
use indicatif::ProgressBar;
use local::Local;
use log::{info, warn};
use rayon::{prelude::*, ThreadPoolBuildError};
use remote::Remote;
use serde::{Deserialize, Serialize};
use snafu::prelude::*;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Could not open config file: {}", source))]
    OpenConfigFile { source: config::Error },

    #[snafu(display("Could not get password from config: {}", source))]
    GetPassword { source: config::Error },

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

    #[snafu(display("Could not open local database: {}", source))]
    OpenLocal { source: local::Error },

    #[snafu(display("Could not open local cache: {}", source))]
    OpenCache { source: cache::Error },

    #[snafu(display("Could not open remote session: {}", source))]
    OpenRemote { source: remote::Error },

    #[snafu(display("Could not index local email: {}", source))]
    IndexLocalEmail { source: local::Error },

    #[snafu(display("Could not index all remote email IDs for a full sync: {}", source))]
    IndexRemoteEmail { source: remote::Error },

    #[snafu(display("Could not retrieve email properties from remote: {}", source))]
    GetRemoteEmails { source: remote::Error },

    #[snafu(display("Could not create download thread pool: {}", source))]
    CreateDownloadThreadPool { source: ThreadPoolBuildError },

    #[snafu(display("Could not download email from remote: {}", source))]
    DownloadRemoteEmail { source: remote::Error },

    #[snafu(display("Could not save email to cache: {}", source))]
    CacheNewEmail { source: cache::Error },
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

fn try_main() -> Result<(), Error> {
    let args = Args::parse();

    env_logger::Builder::new()
        .filter_level(args.verbose.log_level_filter())
        .init();

    // Determine working directory and load all data files.
    let mail_dir = args.path.unwrap_or_else(|| PathBuf::from("."));

    let config = Config::from_file(mail_dir.join("mujmap.toml")).context(OpenConfigFileSnafu {})?;

    // Load the intermediary state.
    let state_filename = mail_dir.join("mujmap.state.json");
    let state = State::open(state_filename).unwrap_or_else(|e| {
        warn!("{}", e);
        State::empty()
    });

    // Open the local notmuch database.
    let local = Local::open(mail_dir, args.dry_run).context(OpenLocalSnafu {})?;

    // Open the local cache.
    let cache = Cache::open(&local.mail_cur_dir).context(OpenCacheSnafu {})?;

    // Open the remote session.
    let mut remote = match &config.fqdn {
        Some(fqdn) => Remote::open_host(
            &fqdn,
            config.username.as_str(),
            &config.password().context(GetPasswordSnafu {})?,
        ),
        None => Remote::open_url(
            &config.session_url.as_ref().unwrap(),
            config.username.as_str(),
            &config.password().context(GetPasswordSnafu {})?,
        ),
    }
    .context(OpenRemoteSnafu {})?;

    // Query local database for all email.
    let local_emails = local.all_email().context(IndexLocalEmailSnafu {})?;

    // Function which performs a full sync, i.e. a sync which considers all
    // remote IDs as updated, and determines destroyed IDs by finding the
    // difference of all remote IDs from all local IDs.
    let full_sync =
        |remote: &mut Remote| -> Result<(jmap::State, HashSet<jmap::Id>, HashSet<jmap::Id>)> {
            let (state, updated_ids) = remote.all_email_ids().context(IndexRemoteEmailSnafu {})?;
            // TODO can we optimize these two lines?
            let local_ids: HashSet<jmap::Id> =
                local_emails.iter().map(|(id, _)| id).cloned().collect();
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
    let (_, remote_emails) = remote
        .get_emails(state.clone(), &updated_ids)
        .context(GetRemoteEmailsSnafu {})?;

    // Before merging, download the new files into the cache.
    // let cached_blobs = local.all_cached_blobs().with_whatever_context(|source| {
    //     format!("Could not discover extant files in cache: {}", source)
    // })?;
    let missing_emails: Vec<&remote::Email> = remote_emails
        .values()
        .filter(|remote_email| {
            // If we have a local mail file with the same `Email` and blob IDs,
            // skip it.
            match local_emails.get(&remote_email.id) {
                Some(local_email) => {
                    if local_email.blob_id == remote_email.blob_id {
                        return false;
                    }
                }
                None => {}
            }

            // If the cache already has this file, skip it.
            !cache.is_in_cache(&remote_email.id, &remote_email.blob_id)
        })
        .collect();

    info!("Downloading new mail...");
    let pb = ProgressBar::new(missing_emails.len() as u64);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.concurrent_downloads)
        .build()
        .context(CreateDownloadThreadPoolSnafu {})?;
    let result: Result<Vec<_>, Error> = pool.install(|| {
        missing_emails
            .into_par_iter()
            .map(|remote_email| {
                let reader = remote
                    .read_email_blob(&remote_email.blob_id)
                    .context(DownloadRemoteEmailSnafu {})?;
                cache
                    .download_into_cache(&remote_email.id, &remote_email.blob_id, reader)
                    .context(CacheNewEmailSnafu {})?;
                pb.inc(1);
                Ok(())
            })
            .collect()
    });
    result?;

    pb.finish_with_message("done");

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
