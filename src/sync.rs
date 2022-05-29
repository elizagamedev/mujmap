use crate::args::Args;
use crate::cache::{self, Cache};
use crate::remote::{self, Remote};
use crate::{config::Config, local::Local};
use crate::{jmap, local};
use atty::Stream;
use fslock::LockFile;
use indicatif::ProgressBar;
use log::{debug, error, warn};
use rayon::{prelude::*, ThreadPoolBuildError};
use serde::{Deserialize, Serialize};
use snafu::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use symlink::symlink_file;
use termcolor::{ColorSpec, StandardStream, WriteColor};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Could not open lock file `{}': {}", path.to_string_lossy(), source))]
    OpenLockFile { path: PathBuf, source: io::Error },

    #[snafu(display("Could not lock: {}", source))]
    Lock { source: io::Error },

    #[snafu(display("Could not log string: {}", source))]
    Log { source: io::Error },

    #[snafu(display("Could not read mujmap state file `{}': {}", filename.to_string_lossy(), source))]
    ReadStateFile {
        filename: PathBuf,
        source: io::Error,
    },

    #[snafu(display("Could not parse mujmap state file `{}': {}", filename.to_string_lossy(), source))]
    ParseStateFile {
        filename: PathBuf,
        source: serde_json::Error,
    },

    #[snafu(display("Could not create mujmap state file `{}': {}", filename.to_string_lossy(), source))]
    CreateStateFile {
        filename: PathBuf,
        source: io::Error,
    },

    #[snafu(display("Could not write to mujmap state file `{}': {}", filename.to_string_lossy(), source))]
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

    #[snafu(display("JMAP server is missing mailboxes for these tags: {:?}", tags))]
    MissingMailboxes { tags: Vec<String> },

    #[snafu(display("Could not create missing mailboxes for tags `{:?}': {}", tags, source))]
    CreateMailboxes {
        tags: Vec<String>,
        source: remote::Error,
    },

    #[snafu(display("Could not index notmuch tags: {}", source))]
    IndexTags { source: notmuch::Error },

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

    #[snafu(display("Could not get local message from notmuch: {}", source))]
    GetNotmuchMessage { source: notmuch::Error },

    #[snafu(display(
        "Could not remove unindexed mail file `{}': {}",
        path.to_string_lossy(),
        source
    ))]
    RemoveUnindexedMailFile { path: PathBuf, source: io::Error },

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

    #[snafu(display("Could not push changes to JMAP server: {}", source))]
    PushChanges { source: remote::Error },

    #[snafu(display("Programmer error!"))]
    ProgrammerError {},
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A new email to be eventually added to the maildir.
#[derive(Debug)]
pub struct NewEmail<'a> {
    pub remote_email: &'a remote::Email,
    pub cache_path: PathBuf,
    pub maildir_path: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub struct LatestState {
    /// Latest revision of the notmuch database since the last time mujmap was run.
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

pub fn sync(
    stdout: &mut StandardStream,
    info_color_spec: ColorSpec,
    mail_dir: PathBuf,
    args: Args,
    config: Config,
) -> Result<(), Error> {
    // Grab lock.
    let lock_file_path = mail_dir.join("mujmap.lock");
    let mut lock = LockFile::open(&lock_file_path).context(OpenLockFileSnafu {
        path: lock_file_path,
    })?;
    let is_locked = lock.try_lock().context(LockSnafu {})?;
    if !is_locked {
        println!("Lock file owned by another process. Waiting...");
        lock.lock().context(LockSnafu {})?;
    }

    // Load the intermediary state.
    let latest_state_filename = mail_dir.join("mujmap.state.json");
    let latest_state = LatestState::open(&latest_state_filename).unwrap_or_else(|e| {
        warn!("{e}");
        LatestState::empty()
    });

    // Open the local notmuch database.
    let local = Local::open(mail_dir, args.dry_run).context(OpenLocalSnafu {})?;

    // Open the local cache.
    let cache = Cache::open(&local.mail_cur_dir, &config).context(OpenCacheSnafu {})?;

    // Open the remote session.
    let mut remote = Remote::open(&config).context(OpenRemoteSnafu {})?;

    // List all remote mailboxes and convert them to notmuch tags.
    let mut mailboxes = remote
        .get_mailboxes(&config.tags)
        .context(IndexMailboxesSnafu {})?;
    debug!("Got mailboxes: {:?}", mailboxes);

    // Query local database for all email.
    let local_emails = local.all_emails().context(IndexLocalEmailsSnafu {})?;

    // Function which performs a full sync, i.e. a sync which considers all remote IDs as updated,
    // and determines destroyed IDs by finding the difference of all remote IDs from all local IDs.
    let full_sync =
        |remote: &mut Remote| -> Result<(jmap::State, HashSet<jmap::Id>, HashSet<jmap::Id>)> {
            let (state, updated_ids) = remote.all_email_ids().context(IndexRemoteEmailsSnafu {})?;
            // TODO can we optimize these two lines?
            let local_ids: HashSet<jmap::Id> =
                local_emails.iter().map(|(id, _)| id).cloned().collect();
            let destroyed_ids = local_ids.difference(&updated_ids).cloned().collect();
            Ok((state, updated_ids, destroyed_ids))
        };

    // Create lists of updated and destroyed `Email` IDs. This is done in one of two ways, depending
    // on if we have a working JMAP `Email` state.
    let (state, updated_ids, destroyed_ids) = latest_state
        .jmap_state
        .map(|jmap_state| {
            match remote.changed_email_ids(jmap_state) {
                Ok((state, created, mut updated, destroyed)) => {
                    debug!("Remote changes: state={state}, created={created:?}, updated={updated:?}, destroyed={destroyed:?}");
                    // If we have something in the updated set that isn't in the local database,
                    // something must have gone wrong somewhere. Do a full sync instead.
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
                        "Error while attempting to resolve changes, attempting full sync: {e}"
                    );
                    full_sync(&mut remote)
                }
            }
        })
        .unwrap_or_else(|| full_sync(&mut remote))?;

    // Retrieve the updated `Email` objects from the server.
    stdout.set_color(&info_color_spec).context(LogSnafu {})?;
    write!(stdout, "Retrieving metadata...").context(LogSnafu {})?;
    stdout.reset().context(LogSnafu {})?;
    writeln!(stdout, " ({} possibly changed)", updated_ids.len()).context(LogSnafu {})?;
    stdout.flush().context(LogSnafu {})?;

    let remote_emails = remote
        .get_emails(updated_ids.iter(), &mailboxes, &config.tags)
        .context(GetRemoteEmailsSnafu {})?;

    // Before merging, download the new files into the cache.
    let mut new_emails: HashMap<jmap::Id, NewEmail> = remote_emails
        .values()
        .filter(|remote_email| match local_emails.get(&remote_email.id) {
            Some(local_email) => local_email.blob_id != remote_email.blob_id,
            None => true,
        })
        .map(|remote_email| (remote_email.id.clone(), NewEmail {
            remote_email,
            cache_path: cache.cache_path(&remote_email.id, &remote_email.blob_id),
            maildir_path: local.new_maildir_path(&remote_email.id, &remote_email.blob_id),
        }))
        .collect();

    let new_emails_missing_from_cache: Vec<&NewEmail> = new_emails
        .values()
        .filter(|x| !x.cache_path.exists() && !local_emails.contains_key(&x.remote_email.id))
        .collect();

    if !new_emails_missing_from_cache.is_empty() {
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
                    let mut retry_count = 0;
                    loop {
                        match download(new_email, &remote, &cache, config.convert_dos_to_unix) {
                            Ok(_) => {
                                pb.inc(1);
                                return Ok(());
                            }
                            Err(e) => {
                                // Try again.
                                retry_count += 1;
                                if config.retries > 0 && retry_count >= config.retries {
                                    return Err(e);
                                }
                                warn!("Download error on try {}, retrying: {}", retry_count, e);
                            }
                        };
                    }
                })
                .collect()
        });
        result?;
        pb.finish_with_message("done");
    }

    // Merge locally.
    //
    // 1. Symlink the cached messages that were previously downloaded into the maildir. We will
    // replace these symlinks with the actual files once the atomic sync is complete.
    //
    // 2. Add new messages to the database by indexing these symlinks. This is also done for
    // existing messages which have new blob IDs.
    //
    // 3. Update the tags of all local messages *except* the ones which had been modified locally
    // since mujmap was last run. Neither JMAP nor notmuch support looking at message history, so if
    // both the local message and the remote message have been flagged as "updated" since the last
    // sync, we prefer to overwrite remote tags with notmuch's tags.
    //
    // 4. Remove messages with destroyed IDs or updated blob IDs.
    //
    // 5. Overwrite the symlinks we made earlier with the actual files from the cache.
    let notmuch_revision = get_notmuch_revision(
        local_emails.is_empty(),
        &local,
        latest_state.notmuch_revision,
        args.dry_run,
    )?;
    let updated_local_emails: HashMap<jmap::Id, local::Email> = local
        .all_emails_since(notmuch_revision)
        .context(IndexLocalUpdatedEmailsSnafu {})?
        .into_iter()
        // Filter out emails that were destroyed on the server.
        .filter(|(id, _)| !destroyed_ids.contains(&id))
        .collect();

    stdout.set_color(&info_color_spec).context(LogSnafu {})?;
    write!(stdout, "Applying changes to notmuch database...").context(LogSnafu {})?;
    stdout.reset().context(LogSnafu {})?;
    writeln!(
        stdout,
        " ({} new, {} changed, {} destroyed)",
        new_emails.len(),
        remote_emails.len(),
        destroyed_ids.len()
    )
    .context(LogSnafu {})?;
    stdout.flush().context(LogSnafu {})?;

    // Update local messages.
    if !args.dry_run {
        // Collect the local messages which will be destroyed. We will add to this list any messages
        // with new blob IDs.
        let mut destroyed_local_emails: Vec<&local::Email> = destroyed_ids
            .into_iter()
            .flat_map(|x| local_emails.get(&x))
            .collect();

        // Symlink the new mail files into the maildir...
        for new_email in new_emails.values() {
            debug!(
                "Making symlink from `{}' to `{}'",
                &new_email.cache_path.to_string_lossy(),
                &new_email.maildir_path.to_string_lossy(),
            );
            if new_email.maildir_path.exists() {
                warn!(
                    "File `{}' already existed in maildir but was not indexed. Replacing...",
                    &new_email.maildir_path.to_string_lossy(),
                );
                fs::remove_file(&new_email.maildir_path).context(RemoveUnindexedMailFileSnafu {
                    path: &new_email.maildir_path,
                })?;
            }
            symlink_file(&new_email.cache_path, &new_email.maildir_path).context(
                MakeMaildirSymlinkSnafu {
                    from: &new_email.cache_path,
                    to: &new_email.maildir_path,
                },
            )?;
        }

        let mut commit_changes = || -> Result<()> {
            local.begin_atomic().context(BeginAtomicSnafu {})?;

            // ...and add them to the database.
            let new_local_emails = new_emails
                .values()
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
            //
            // XXX: If the server contains two or more of a message which notmuch considers a
            // duplicate, it will be updated *for each duplicate* in a non-deterministic order. This
            // may cause surprises.
            for remote_email in remote_emails.values() {
                // Skip email which has been updated offline.
                if updated_local_emails.contains_key(&remote_email.id) {
                    continue;
                }

                // Do it!
                let local_email = [
                    new_local_emails.get(&remote_email.id),
                    local_emails.get(&remote_email.id),
                ]
                .into_iter()
                .flatten()
                .next()
                .ok_or_else(|| {
                    error!(
                        "Could not find local email for updated remote ID {}",
                        remote_email.id
                    );
                    Error::ProgrammerError {}
                })?;

                // Add mailbox tags
                let mut tags: HashSet<&str> = remote_email.tags.iter().map(|s| s.as_str()).collect();
                for id in &remote_email.mailbox_ids {
                    if let Some(mailbox) = mailboxes.mailboxes_by_id.get(id) {
                        tags.insert(&mailbox.tag);
                    }
                }

                local
                    .update_email_tags(local_email, tags)
                    .context(UpdateLocalEmailSnafu {})?;

                // In `update' notmuch may have renamed the file on disk when setting maildir
                // flags, so we need to update our idea of the filename to match so that, for new
                // messages, we can reliably replace the symlink later.
                //
                // The `Message' might have multiple paths though (if more than one message has the
                // same id) so we have to get all the filenames and then find the one that matches
                // ours. Fortunately, our generated name (the raw JMAP mailbox.message id) will
                // always be a substring of notmuch's version (same name with flags attached), so a
                // starts-with test is enough.
                if let Some(mut new_email) = new_emails.get_mut(&remote_email.id) {
                    if let Some(our_filename) = new_email.maildir_path.file_name().map(|p| p.to_string_lossy()) {
                        if let Some(message) =
                            local
                                .get_message(&local_email.message_id)
                                .context(GetNotmuchMessageSnafu {})? {

                            if let Some(new_maildir_path) = message
                                .filenames()
                                .into_iter()
                                .filter(|f| f.file_name().map_or(false, |p| p.to_string_lossy().starts_with(&*our_filename)))
                                .next() {

                                new_email.maildir_path = new_maildir_path;
                            }
                        }
                    }
                }

            }

            // Finally, remove the old messages from the database.
            for destroyed_local_email in &destroyed_local_emails {
                local
                    .remove_email(*destroyed_local_email)
                    .context(RemoveLocalEmailSnafu {})?;
            }

            local.end_atomic().context(EndAtomicSnafu {})?;
            Ok(())
        };

        if let Err(e) = commit_changes() {
            // Remove all the symlinks.
            for new_email in new_emails.values() {
                debug!(
                    "Removing symlink `{}'",
                    &new_email.maildir_path.to_string_lossy(),
                );
                if let Err(e) = fs::remove_file(&new_email.maildir_path) {
                    warn!(
                        "Could not remove symlink `{}': {e}",
                        &new_email.maildir_path.to_string_lossy(),
                    );
                }
            }
            // Fail as normal.
            return Err(e);
        }

        // Now that the atomic database operation has been completed, do the actual file operations.

        // Replace the symlinks with the real files.
        for new_email in new_emails.values() {
            debug!(
                "Moving mail from `{}' to `{}'",
                &new_email.cache_path.to_string_lossy(),
                &new_email.maildir_path.to_string_lossy(),
            );
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

    // Ensure that for every tag, there exists a corresponding mailbox.
    if !args.dry_run {
        let tags_with_missing_mailboxes: Vec<String> = local
            .all_tags()
            .context(IndexTagsSnafu {})?
            .filter(|tag| {
                let tag = tag.as_str();
                // Any tags which *can* be mapped to a keyword do not require a mailbox.
                // Additionally, automatic tags are never mapped to mailboxes.
                if [
                    "draft",
                    "flagged",
                    "passed",
                    "replied",
                    "unread",
                    &config.tags.spam,
                    &config.tags.important,
                    &config.tags.phishing,
                ]
                .contains(&tag)
                    || local::AUTOMATIC_TAGS.contains(tag)
                {
                    false
                } else {
                    !mailboxes.ids_by_tag.contains_key(tag)
                }
            })
            .collect();
        if !tags_with_missing_mailboxes.is_empty() {
            if !config.auto_create_new_mailboxes {
                return Err(Error::MissingMailboxes {
                    tags: tags_with_missing_mailboxes,
                });
            }
            remote
                .create_mailboxes(&mut mailboxes, &tags_with_missing_mailboxes, &config.tags)
                .context(CreateMailboxesSnafu {
                    tags: tags_with_missing_mailboxes,
                })?;
        }

        // Update remote messages.
        stdout.set_color(&info_color_spec).context(LogSnafu {})?;
        write!(stdout, "Applying changes to JMAP server...").context(LogSnafu {})?;
        stdout.reset().context(LogSnafu {})?;
        writeln!(stdout, " ({} changed)", updated_local_emails.len()).context(LogSnafu {})?;
        stdout.flush().context(LogSnafu {})?;

        remote
            .update(&updated_local_emails, &mailboxes, &config.tags)
            .context(PushChangesSnafu {})?;
    }

    if !args.dry_run {
        // Record the final state for the next invocation.
        LatestState {
            notmuch_revision: Some(local.revision() + 1),
            jmap_state: Some(state),
        }
        .save(latest_state_filename)?;
    }

    Ok(())
}

fn download(
    new_email: &NewEmail,
    remote: &Remote,
    cache: &Cache,
    convert_dos_to_unix: bool,
) -> Result<()> {
    let remote_email = new_email.remote_email;
    let reader = remote
        .read_email_blob(&remote_email.blob_id)
        .context(DownloadRemoteEmailSnafu {})?;
    cache
        .download_into_cache(&new_email, reader, convert_dos_to_unix)
        .context(CacheNewEmailSnafu {})?;
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
