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
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use symlink::symlink_file;
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
    AddLocalEmail { source: notmuch::Error },

    #[snafu(display("Could not update local email: {}", source))]
    UpdateLocalEmail { source: notmuch::Error },

    #[snafu(display("Could not remove local email: {}", source))]
    RemoveLocalEmail { source: notmuch::Error },

    #[snafu(display(
        "Could not make symlink from cache `{}' to maildir `{}': {}",
        from.to_string_lossy(),
        to.to_string_lossy(),
        source
    ))]
    MakeMaildirSymlink {
        from: PathBuf,
        to: PathBuf,
        source: io::Error,
    },

    #[snafu(display("Could not rename mail file from `{}' to `{}': {}", from.to_string_lossy(), to.to_string_lossy(), source))]
    RenameMailFile {
        from: PathBuf,
        to: PathBuf,
        source: io::Error,
    },

    #[snafu(display("Could not remove mail file `{}': {}", path.to_string_lossy(), source))]
    RemoveMailFile { path: PathBuf, source: io::Error },

    #[snafu(display("Could not begin atomic database operation: {}", source))]
    BeginAtomic { source: notmuch::Error },

    #[snafu(display("Could not end atomic database operation: {}", source))]
    EndAtomic { source: notmuch::Error },
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

/// A new email to be eventually added to the maildir.
pub struct NewEmail<'a> {
    pub remote_email: &'a remote::Email,
    pub cache_path: PathBuf,
    pub maildir_path: PathBuf,
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
    let local_emails = local.all_emails().context(IndexLocalEmailsSnafu {})?;

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
            match remote.changed_email_ids(jmap_state) {
                Ok((state, created, mut updated, destroyed)) => {
                    // If we have something in the updated set that isn't in the
                    // local database, something must have gone wrong somewhere.
                    // Do a full sync instead.
                    if !updated.iter().all(|x| local_emails.contains_key(x)) {
                        warn!(
                            "Server sent an update which references an ID we don't know about, doing a full sync instead");
                        full_sync(&mut remote)
                    } else {
                        updated.extend(created);
                        Ok((state, updated, destroyed))
                    }
                },
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

    let remote_emails = remote
        .get_emails(&updated_ids)
        .context(GetRemoteEmailsSnafu {})?;

    // Before merging, download the new files into the cache.
    let new_emails: Vec<NewEmail> = remote_emails
        .values()
        .filter(|remote_email| match local_emails.get(&remote_email.id) {
            Some(local_email) => local_email.blob_id != remote_email.blob_id,
            None => false,
        })
        .map(|remote_email| NewEmail {
            remote_email,
            cache_path: cache.cache_path(&remote_email.id, &remote_email.blob_id),
            maildir_path: local.new_maildir_path(&remote_email.id, &remote_email.blob_id),
        })
        .collect();

    let new_emails_missing_from_cache: Vec<&NewEmail> = new_emails
        .iter()
        .filter(|x| !x.cache_path.exists())
        .collect();

    stdout.set_color(&info_color_spec).context(LogSnafu {})?;
    writeln!(stdout, "Downloading new mail...").context(LogSnafu {})?;
    stdout.reset().context(LogSnafu {})?;
    stdout.flush().context(LogSnafu {})?;

    let pb = ProgressBar::new(new_emails_missing_from_cache.len() as u64);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.concurrent_downloads)
        .build()
        .context(CreateDownloadThreadPoolSnafu {})?;
    let result: Result<Vec<_>, Error> = pool.install(|| {
        new_emails_missing_from_cache
            .into_par_iter()
            .map(|new_email| {
                let remote_email = new_email.remote_email;
                let reader = remote
                    .read_email_blob(&remote_email.blob_id)
                    .context(DownloadRemoteEmailSnafu {})?;
                cache
                    .download_into_cache(&new_email, reader)
                    .context(CacheNewEmailSnafu {})?;
                pb.inc(1);
                Ok(())
            })
            .collect()
    });
    result?;
    pb.finish_with_message("done");

    // Merge locally.
    //
    // 1. Symlink the cached messages that were previously downloaded into the
    // maildir. We will replace these symlinks with the actual files once the
    // atomic sync is complete.
    //
    // 2. Add new messages to the database by indexing these symlinks. This is
    // also done for existing messages which have new blob IDs.
    //
    // 3. Update the tags of all local messages *except* the ones which had been
    // modified locally since mujmap was last run. Neither JMAP nor notmuch
    // support looking at message history, so if both the local message and the
    // remote message have been flagged as "updated" since the last sync, we
    // prefer to overwrite remote tags with notmuch's tags.
    //
    // 4. Remove messages with destroyed IDs or updated blob IDs.
    //
    // 5. Overwrite the symlinks we made earlier with the actual files from the
    // cache.
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
    let updated_local_emails = local
        .all_emails_since(notmuch_revision)
        .context(IndexLocalUpdatedEmailsSnafu {})?;

    // Update local messages.
    if !args.dry_run {
        // Collect the local messages which will be destroyed. We will add to
        // this list any messages with new blob IDs.
        let mut destroyed_local_emails: Vec<&local::Email> = destroyed_ids
            .into_iter()
            .flat_map(|x| local_emails.get(&x))
            .collect();

        // Symlink the new mail files into the maildir...
        for new_email in &new_emails {
            symlink_file(&new_email.cache_path, &new_email.maildir_path).context(
                MakeMaildirSymlinkSnafu {
                    from: &new_email.cache_path,
                    to: &new_email.maildir_path,
                },
            )?;
        }

        local.begin_atomic().context(BeginAtomicSnafu {})?;

        // ...and add them to the database.
        let new_local_emails = new_emails
            .iter()
            .map(|new_email| {
                let local_email = local
                    .add_new_email(&new_email)
                    .context(AddLocalEmailSnafu {})?;
                if let Some(e) = local_emails.get(&new_email.remote_email.id) {
                    // Move the old message to the destroyed emails set.
                    destroyed_local_emails.push(e);
                }
                Ok((local_email.id.clone(), local_email))
            })
            .collect::<Result<HashMap<_, _>>>()?;

        // Update local emails with remote tags.
        for remote_email in remote_emails.values() {
            // Skip email which has been updated offline.
            if updated_local_emails.contains_key(&remote_email.id) {
                continue;
            }

            // Do it!
            let local_email = new_local_emails
                .get(&remote_email.id)
                .unwrap_or_else(|| &local_emails[&remote_email.id]);
            local_email
                .update(remote_email, &mailboxes)
                .context(UpdateLocalEmailSnafu {})?;
        }

        // Finally, remove the old messages from the database.
        for destroyed_local_email in &destroyed_local_emails {
            local
                .remove_email(*destroyed_local_email)
                .context(RemoveLocalEmailSnafu {})?;
        }

        local.end_atomic().context(EndAtomicSnafu {})?;

        // Now that the atomic database operation has been completed, do the
        // actual file operations.

        // Replace the symlinks with the real files.
        for new_email in &new_emails {
            fs::rename(&new_email.cache_path, &new_email.maildir_path).context(
                RenameMailFileSnafu {
                    from: &new_email.cache_path,
                    to: &new_email.maildir_path,
                },
            )?;
        }

        // Delete the destroyed email files.
        for destroyed_local_email in &destroyed_local_emails {
            fs::remove_file(&destroyed_local_email.path).context(RemoveMailFileSnafu {
                path: &destroyed_local_email.path,
            })?;
        }
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
