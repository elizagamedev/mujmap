mod args;
mod cache;
mod config;
mod jmap;
mod local;
mod remote;

use args::Args;
use atty::Stream;
use cache::Cache;
use clap::Parser;
use config::Config;
use indicatif::ProgressBar;
use local::Local;
use log::warn;
use rayon::{prelude::*, ThreadPoolBuildError};
use remote::Remote;
use serde::{Deserialize, Serialize};
use snafu::prelude::*;
use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Could not log string: {}", source))]
    Log { source: io::Error },

    #[snafu(display("Could not open config file: {}", source))]
    OpenConfigFile { source: config::Error },

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

    #[snafu(display("Could not create mujmore state file `{}': {}", filename.to_string_lossy(), source))]
    CreateStateFile {
        filename: PathBuf,
        source: io::Error,
    },

    #[snafu(display("Could not write to mujmore state file `{}': {}", filename.to_string_lossy(), source))]
    WriteStateFile {
        filename: PathBuf,
        source: serde_json::Error,
    },

    #[snafu(display("Could not open local database: {}", source))]
    OpenLocal { source: local::Error },

    #[snafu(display("Could not open local cache: {}", source))]
    OpenCache { source: cache::Error },

    #[snafu(display("Could not open remote session: {}", source))]
    OpenRemote { source: remote::Error },

    #[snafu(display("Could not index mailboxes: {}", source))]
    IndexMailboxes { source: remote::Error },

    #[snafu(display("Could not index local emails: {}", source))]
    IndexLocalEmails { source: local::Error },

    #[snafu(display("Could not index all remote email IDs for a full sync: {}", source))]
    IndexRemoteEmails { source: remote::Error },

    #[snafu(display("Could not retrieve email properties from remote: {}", source))]
    GetRemoteEmails { source: remote::Error },

    #[snafu(display("Could not create download thread pool: {}", source))]
    CreateDownloadThreadPool { source: ThreadPoolBuildError },

    #[snafu(display("Could not download email from remote: {}", source))]
    DownloadRemoteEmail { source: remote::Error },

    #[snafu(display("Could not save email to cache: {}", source))]
    CacheNewEmail { source: cache::Error },

    #[snafu(display("Missing last notmuch database revision"))]
    MissingNotmuchDatabaseRevision {},

    #[snafu(display("Could not index local updated emails: {}", source))]
    IndexLocalUpdatedEmails { source: local::Error },

    #[snafu(display("Could not add new local email: {}", source))]
    AddLocalEmail { source: local::Error },

    #[snafu(display("Could not update local email: {}", source))]
    UpdateLocalEmail { source: notmuch::Error },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Serialize, Deserialize)]
pub struct LatestState {
    /// Latest revision of the notmuch database since the last time mujmap was
    /// run.
    pub notmuch_revision: Option<u64>,
    /// Latest JMAP Email state returned by `Email/get`.
    pub jmap_state: Option<jmap::State>,
}

impl LatestState {
    fn open(filename: impl AsRef<Path>) -> Result<Self> {
        let filename = filename.as_ref();
        let file = File::open(filename).context(ReadStateFileSnafu { filename })?;
        let reader = BufReader::new(file);
        serde_json::from_reader(reader).context(ParseStateFileSnafu { filename })
    }

    fn save(&self, filename: impl AsRef<Path>) -> Result<()> {
        let filename = filename.as_ref();
        let file = File::create(filename).context(CreateStateFileSnafu { filename })?;
        let writer = BufWriter::new(file);
        serde_json::to_writer(writer, self).context(WriteStateFileSnafu { filename })
    }

    fn empty() -> Self {
        Self {
            notmuch_revision: None,
            jmap_state: None,
        }
    }
}

fn try_main(stdout: &mut StandardStream) -> Result<(), Error> {
    let args = Args::parse();

    env_logger::Builder::new()
        .filter_level(args.verbose.log_level_filter())
        .parse_default_env()
        .init();

    let info_color_spec = ColorSpec::new()
        .set_fg(Some(Color::Green))
        .set_bold(true)
        .to_owned();

    // Determine working directory and load all data files.
    let mail_dir = args.path.unwrap_or_else(|| PathBuf::from("."));

    let config = Config::from_file(mail_dir.join("mujmap.toml")).context(OpenConfigFileSnafu {})?;

    // Load the intermediary state.
    let latest_state_filename = mail_dir.join("mujmap.state.json");
    let latest_state = LatestState::open(&latest_state_filename).unwrap_or_else(|e| {
        warn!("{}", e);
        LatestState::empty()
    });

    // Open the local notmuch database.
    let local = Local::open(mail_dir, args.dry_run).context(OpenLocalSnafu {})?;

    // Open the local cache.
    let cache = Cache::open(&local.mail_cur_dir).context(OpenCacheSnafu {})?;

    // Open the remote session.
    let mut remote = Remote::open(&config).context(OpenRemoteSnafu {})?;

    // List all remote mailboxes and convert them to notmuch tags.
    let mailboxes = remote.get_mailboxes().context(IndexMailboxesSnafu {})?;

    // Query local database for all email.
    let mut local_emails = local.all_emails().context(IndexLocalEmailsSnafu {})?;

    // Function which performs a full sync, i.e. a sync which considers all
    // remote IDs as updated, and determines destroyed IDs by finding the
    // difference of all remote IDs from all local IDs.
    let full_sync =
        |remote: &mut Remote| -> Result<(jmap::State, HashSet<jmap::Id>, HashSet<jmap::Id>)> {
            let (state, updated_ids) = remote.all_email_ids().context(IndexRemoteEmailsSnafu {})?;
            // TODO can we optimize these two lines?
            let local_ids: HashSet<jmap::Id> =
                local_emails.iter().map(|(id, _)| id).cloned().collect();
            let destroyed_ids = local_ids.difference(&updated_ids).cloned().collect();
            Ok((state, updated_ids, destroyed_ids))
        };

    // Create lists of updated and destroyed `Email` IDs. This is done in one of
    // two ways, depending on if we have a working JMAP `Email` state.
    let (state, updated_ids, destroyed_ids) = latest_state
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

    // Retrieve the updated `Email` objects from the server.
    stdout.set_color(&info_color_spec).context(LogSnafu {})?;
    writeln!(stdout, "Retrieving metadata...").context(LogSnafu {})?;
    stdout.reset().context(LogSnafu {})?;
    stdout.flush().context(LogSnafu {})?;

    let (_, remote_emails) = remote
        .get_emails(state.clone(), &updated_ids)
        .context(GetRemoteEmailsSnafu {})?;

    // Before merging, download the new files into the cache.
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

    stdout.set_color(&info_color_spec).context(LogSnafu {})?;
    writeln!(stdout, "Downloading new mail...").context(LogSnafu {})?;
    stdout.reset().context(LogSnafu {})?;
    stdout.flush().context(LogSnafu {})?;

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

    // Now merge.
    //
    // 1. Gather all local messages which were modified since we last ran `mujmap`.
    //
    // 2. Update all local messages *except* the above messages with the remote
    // changes. Neither JMAP nor notmuch support looking at message history, so
    // if both the local message and the remote message have been "updated"
    // since the last sync, especially since JMAP messages will be considered
    // "updated" for a lot less than what we care about, especially considering
    // full syncs, we prefer to overwrite remote tags with notmuch's tags.
    //
    // 3. Update the remote tags.
    stdout.set_color(&info_color_spec).context(LogSnafu {})?;
    writeln!(stdout, "Applying changes to notmuch database...").context(LogSnafu {})?;
    stdout.reset().context(LogSnafu {})?;
    stdout.flush().context(LogSnafu {})?;

    let notmuch_revision = get_notmuch_revision(
        local_emails.is_empty(),
        &local,
        latest_state.notmuch_revision,
        args.dry_run,
    )?;
    let updated_local = local
        .all_emails_since(notmuch_revision)
        .context(IndexLocalUpdatedEmailsSnafu {})?;

    // Update local messages.
    if !args.dry_run {
        let pb = ProgressBar::new(updated_local.len() as u64);
        for remote_email in remote_emails.values() {
            if updated_local.contains_key(&remote_email.id) {
                // Skip.
                continue;
            }
            // Commit the remote changes!
            let local_email = match local_emails.remove(&remote_email.id) {
                Some(x) => x,
                None => local
                    .add_new_email(
                        &cache,
                        remote_email.id.clone(),
                        remote_email.blob_id.clone(),
                    )
                    .context(AddLocalEmailSnafu {})?,
            };
            // TODO Make sure to update the blob if it's changed.
            local_email
                .update(remote_email, &mailboxes)
                .context(UpdateLocalEmailSnafu {})?;

            pb.inc(1);
        }
        pb.finish_with_message("done");
    }

    // Record the final state for the next invocation.
    LatestState {
        notmuch_revision: Some(local.revision() + 1),
        jmap_state: Some(state),
    }
    .save(latest_state_filename)?;

    Ok(())
}

fn get_notmuch_revision(
    has_no_local_emails: bool,
    local: &Local,
    notmuch_revision: Option<u64>,
    dry_run: bool,
) -> Result<u64> {
    match notmuch_revision {
        Some(x) => Ok(x),
        None => {
            if has_no_local_emails {
                Ok(local.revision())
            } else {
                if dry_run {
                    println!(
                        "\
THIS IS A DRY RUN, SO NO CHANGES WILL BE MADE NO MATTER THE CHOICE. HOWEVER,
HEED THE WARNING FOR THE REAL DEAL.
"
                    );
                }
                println!(
                    "\
mujmap was unable to read the notmuch database revision (stored in
mujmap.state.json) since the last time it was run. As a result, it cannot
determine the changes made in the local database since the last time a
synchronization was performed.
"
                );
                if atty::is(Stream::Stdout) {
                    println!(
                        "\
Would you like to potentially discard edits to the notmuch database and replace
them with the JMAP server's changes? This will not affect any notmuch messages
not managed by mujmap or other maildirs managed by mujmap.

Alternatively, you may cancel and attempt to resolve this manually by adding a
`notmuch_revision' to a specific notmuch database revision number in the
`mujmap.state.json' file.

Continue? (y/N)
"
                    );
                    let mut response = String::new();
                    io::stdin().read_line(&mut response).ok();
                    let trimmed = response.trim();
                    ensure!(
                        trimmed == "y" || trimmed == "Y",
                        MissingNotmuchDatabaseRevisionSnafu {}
                    );
                    Ok(local.revision())
                } else {
                    println!(
                        "\
Please run notmuj again in an interactive terminal to resolve.
"
                    );
                    return Err(Error::MissingNotmuchDatabaseRevision {});
                }
            }
        }
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
            writeln!(&mut stderr, "error: {}", err).ok();
            1
        }
    });
}
