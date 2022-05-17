use crate::config;
use crate::jmap;
use crate::jmap::EmailKeyword;
use crate::remote;
use crate::sync::NewEmail;
use const_format::formatcp;
use lazy_static::lazy_static;
use log::debug;
use notmuch::Database;
use notmuch::Exclude;
use notmuch::Message;
use path_absolutize::*;
use regex::Regex;
use snafu::prelude::*;
use snafu::Snafu;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

const ID_PATTERN: &'static str = r"[-A-Za-z0-9_]+";
const MAIL_PATTERN: &'static str = formatcp!(r"^({})\.({})(?:$|:)", ID_PATTERN, ID_PATTERN);

lazy_static! {
    /// mujmap *must not* touch automatic tags, and should warn if the JMAP server contains
    /// mailboxes that match these tags.
    ///
    /// These values taken from: https://notmuchmail.org/special-tags/
    pub static ref AUTOMATIC_TAGS: HashSet<&'static str> =
        HashSet::from(["attachment", "signed", "encrypted"]);
}

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Could not absolutize given path: {}", source))]
    Absolutize { source: io::Error },

    #[snafu(display(
        "Given maildir path `{}' is not a subdirectory of the notmuch root `{}'",
        mail_dir.to_string_lossy(),
        notmuch_root.to_string_lossy(),
    ))]
    MailDirNotASubdirOfNotmuchRoot {
        mail_dir: PathBuf,
        notmuch_root: PathBuf,
    },

    #[snafu(display("Could not open notmuch database: {}", source))]
    OpenDatabase { source: notmuch::Error },

    #[snafu(display("Could not create Maildir dir `{}': {}", path.to_string_lossy(), source))]
    CreateMaildirDir { path: PathBuf, source: io::Error },

    #[snafu(display("Could not create notmuch query `{}': {}", query, source))]
    CreateNotmuchQuery {
        query: String,
        source: notmuch::Error,
    },

    #[snafu(display("Could not execute notmuch query `{}': {}", query, source))]
    ExecuteNotmuchQuery {
        query: String,
        source: notmuch::Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub struct Local {
    /// Notmuch database.
    db: Database,
    /// The path to mujmap's maildir/cur.
    pub mail_cur_dir: PathBuf,
    /// Notmuch search query which searches for all mail in mujmap's maildir.
    all_mail_query: String,
}

impl Local {
    /// Open the local store.
    ///
    /// `mail_dir` *must* be a subdirectory of the notmuch path.
    pub fn open(mail_dir: impl AsRef<Path>, dry_run: bool) -> Result<Self> {
        // Open the notmuch database with default config options.
        let db = Database::open_with_config::<PathBuf, PathBuf>(
            None,
            if dry_run {
                notmuch::DatabaseMode::ReadOnly
            } else {
                notmuch::DatabaseMode::ReadWrite
            },
            None,
            None,
        )
        .context(OpenDatabaseSnafu {})?;

        // Build new absolute path resolving all relative paths. Check to make sure it's actually a
        // subdirectory of the notmuch root path.
        let mail_dir = mail_dir.as_ref().absolutize().context(AbsolutizeSnafu {})?;

        if !mail_dir.starts_with(db.path()) {
            return Err(Error::MailDirNotASubdirOfNotmuchRoot {
                mail_dir: mail_dir.into(),
                notmuch_root: db.path().into(),
            });
        }

        // Build the query to search for all mail in our maildir.
        let all_mail_query = format!(
            "path:\"{}/**\"",
            mail_dir.strip_prefix(db.path()).unwrap().to_str().unwrap()
        );

        // Ensure the maildir contains the standard cur, new, and tmp dirs.
        let mail_cur_dir = mail_dir.join("cur");
        if !dry_run {
            for path in &[&mail_cur_dir, &mail_dir.join("new"), &mail_dir.join("tmp")] {
                fs::create_dir_all(path).context(CreateMaildirDirSnafu { path })?;
            }
        }

        Ok(Self {
            db,
            mail_cur_dir,
            all_mail_query,
        })
    }

    pub fn revision(&self) -> u64 {
        self.db.revision().revision
    }

    /// Create a path for a newly added file to the maildir.
    pub fn new_maildir_path(&self, id: &jmap::Id, blob_id: &jmap::Id) -> PathBuf {
        self.mail_cur_dir.join(format!("{}.{}", id, blob_id))
    }

    /// Return all `Email`s that mujmap owns for this maildir.
    pub fn all_emails(&self) -> Result<HashMap<jmap::Id, Email>> {
        self.query(&self.all_mail_query)
    }

    /// Return all `Email`s that mujmap owns which were modified since the given database revision.
    pub fn all_emails_since(&self, last_revision: u64) -> Result<HashMap<jmap::Id, Email>> {
        self.query(&format!(
            "{} and lastmod:{}..{}",
            self.all_mail_query,
            last_revision,
            self.revision()
        ))
    }

    /// Return all tags in the database.
    pub fn all_tags(&self) -> Result<notmuch::Tags, notmuch::Error> {
        self.db.all_tags()
    }

    /// Begin atomic database operation.
    pub fn begin_atomic(&self) -> Result<(), notmuch::Error> {
        self.db.begin_atomic()
    }

    /// End atomic database operation.
    pub fn end_atomic(&self) -> Result<(), notmuch::Error> {
        self.db.end_atomic()
    }

    /// Add the given email into the database.
    pub fn add_new_email(&self, new_email: &NewEmail) -> Result<Email, notmuch::Error> {
        debug!("Adding new email: {:?}", new_email);
        let message = self.db.index_file(&new_email.maildir_path, None)?;
        Ok(Email {
            id: new_email.remote_email.id.clone(),
            blob_id: new_email.remote_email.blob_id.clone(),
            message,
            path: new_email.maildir_path.clone(),
        })
    }

    /// Remove the given email file from notmuch's database and the disk.
    pub fn remove_email(&self, email: &Email) -> Result<(), notmuch::Error> {
        debug!("Removing email: {:?}", email);
        self.db.remove_message(&email.path)
    }

    fn query(&self, query_string: &str) -> Result<HashMap<jmap::Id, Email>> {
        debug!("notmuch query: {}", query_string);

        let query =
            self.db
                .create_query(query_string)
                .with_context(|_| CreateNotmuchQuerySnafu {
                    query: query_string.clone(),
                })?;
        query.set_omit_excluded(Exclude::False);
        let messages = query
            .search_messages()
            .with_context(|_| ExecuteNotmuchQuerySnafu {
                query: query_string.clone(),
            })?;
        Ok(messages
            .into_iter()
            .flat_map(|x| Email::from_message(x, &self.mail_cur_dir))
            .map(|x| (x.id.clone(), x))
            .collect())
    }
}

#[derive(Debug)]
pub struct Email {
    pub id: jmap::Id,
    pub blob_id: jmap::Id,
    pub message: Message,
    pub path: PathBuf,
}

impl Email {
    /// Returns a separate `Email` object for each duplicate email file mujmap owns.
    fn from_message(message: Message, mail_cur_dir: &Path) -> Vec<Self> {
        lazy_static! {
            static ref MAIL_FILE: Regex = Regex::new(MAIL_PATTERN).unwrap();
        }
        message
            .filenames()
            .into_iter()
            .filter(|x| x.starts_with(mail_cur_dir))
            .flat_map(|path| {
                MAIL_FILE
                    .captures(&path.file_name().unwrap().to_string_lossy())
                    .map(|x| {
                        let id = jmap::Id(x.get(1).unwrap().as_str().to_string());
                        let blob_id = jmap::Id(x.get(2).unwrap().as_str().to_string());
                        (id, blob_id)
                    })
                    .map(|(id, blob_id)| (id, blob_id, path))
            })
            .map(|(id, blob_id, path)| Self {
                id,
                blob_id,
                message: message.clone(),
                path,
            })
            .collect()
    }

    pub fn update(
        &self,
        remote_email: &remote::Email,
        mailboxes: &remote::Mailboxes,
        tags_config: &config::Tags,
    ) -> Result<(), notmuch::Error> {
        // Keywords. Consider *only* keywords which are not explicitly disabled by the config and
        // are not already covered by a mailbox.
        fn none_if_empty(s: &str) -> Option<&str> {
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        let mut tags = HashSet::new();
        for keyword in &remote_email.keywords {
            if let Some(tag) = match keyword {
                EmailKeyword::Answered => Some("replied"),
                EmailKeyword::Draft => {
                    if mailboxes.roles.draft {
                        None
                    } else {
                        Some("draft")
                    }
                }
                EmailKeyword::Flagged => {
                    if mailboxes.roles.flagged {
                        None
                    } else {
                        Some("flagged")
                    }
                }
                EmailKeyword::Forwarded => Some("passed"),
                EmailKeyword::Important => {
                    if mailboxes.roles.important {
                        None
                    } else {
                        none_if_empty(&tags_config.important)
                    }
                }
                EmailKeyword::Phishing => none_if_empty(&tags_config.phishing),
                _ => None,
            } {
                tags.insert(tag);
            }
        }
        if !mailboxes.roles.spam
            && !tags_config.spam.is_empty()
            && remote_email.keywords.contains(&EmailKeyword::Junk)
            && !remote_email.keywords.contains(&EmailKeyword::NotJunk)
        {
            tags.insert(&tags_config.spam);
        }
        if !remote_email.keywords.contains(&EmailKeyword::Seen) {
            tags.insert("unread");
        }
        // Mailboxes.
        for id in &remote_email.mailbox_ids {
            if let Some(mailbox) = mailboxes.mailboxes_by_id.get(id) {
                tags.insert(&mailbox.tag);
            }
        }
        // Build diffs for tags and apply them.
        self.message.freeze()?;
        let extant_tags: HashSet<String> = self.message.tags().into_iter().collect();
        let tags_to_remove: Vec<&str> = extant_tags
            .iter()
            .map(|tag| tag.as_str())
            .filter(|tag| !tags.contains(tag) && !AUTOMATIC_TAGS.contains(tag))
            .collect();
        let tags_to_add: Vec<&str> = tags
            .iter()
            .cloned()
            .filter(|&tag| !extant_tags.contains(tag))
            .collect();
        debug!(
            "Updating local email: {:?}, {:?}, by adding tags: {tags_to_add:?}, removing tags: {tags_to_remove:?}",
            self, remote_email
        );
        for tag in tags_to_remove {
            self.message.remove_tag(tag)?;
        }
        for tag in tags_to_add {
            self.message.add_tag(tag)?;
        }
        self.message.thaw()?;
        Ok(())
    }
}
