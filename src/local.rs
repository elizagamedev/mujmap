use crate::jmap;
use crate::sync::NewEmail;
use const_format::formatcp;
use lazy_static::lazy_static;
use log::debug;
use notmuch::Database;
use notmuch::Exclude;
use notmuch::Message;
use notmuch::ConfigKey;
use regex::Regex;
use snafu::prelude::*;
use snafu::Snafu;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::path::StripPrefixError;

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
    #[snafu(display("Could not canonicalize given path: {}", source))]
    Canonicalize { source: io::Error },

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

#[derive(Debug)]
pub struct Email {
    pub id: jmap::Id,
    pub blob_id: jmap::Id,
    pub message_id: String,
    pub path: PathBuf,
    pub tags: HashSet<String>,
}

pub struct Local {
    /// Notmuch database.
    db: Database,
    /// The path to mujmap's maildir/cur.
    pub mail_cur_dir: PathBuf,
    /// Notmuch search query which searches for all mail in mujmap's maildir.
    all_mail_query: String,
    /// Flag, whether or not notmuch should add maildir flags to message filenames.
    pub synchronize_maildir_flags: bool,
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

        // Find the mail dir, either notmuch's idea, or just under the data dir.
        let canonical_mail_dir_path = db
            .config(ConfigKey::MailRoot)
            .map_or(
                db.path().into(),
                |root| PathBuf::from(root)
            )
            .canonicalize()
            .context(CanonicalizeSnafu {})?;


        debug!("mail dir: {}", canonical_mail_dir_path.to_str().unwrap());

        // Build the query to search for all mail in our maildir.
        let all_mail_query = "path:**".to_string();

        // Ensure the maildir contains the standard cur, new, and tmp dirs.
        let mail_cur_dir = canonical_mail_dir_path.join("cur");
        if !dry_run {
            for path in &[
                &mail_cur_dir,
                &canonical_mail_dir_path.join("new"),
                &canonical_mail_dir_path.join("tmp"),
            ] {
                fs::create_dir_all(path).context(CreateMaildirDirSnafu { path })?;
            }
        }

        let synchronize_maildir_flags = db.config_bool(ConfigKey::MaildirFlags).unwrap_or(true);

        Ok(Self {
            db,
            mail_cur_dir,
            all_mail_query,
            synchronize_maildir_flags,
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
        let tags = message
            .tags()
            .into_iter()
            .filter(|tag| !AUTOMATIC_TAGS.contains(tag.as_str()))
            .collect();
        Ok(Email {
            id: new_email.remote_email.id.clone(),
            blob_id: new_email.remote_email.blob_id.clone(),
            message_id: message.id().to_string(),
            path: new_email.maildir_path.clone(),
            tags,
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
            .flat_map(|x| self.emails_from_message(x))
            .map(|x| (x.id.clone(), x))
            .collect())
    }

    /// Get a notmuch Message object for the wanted id.
    pub fn get_message(&self, id: &str) -> Result<Option<Message>, notmuch::Error> {
        let query_string = format!("id:{}", id);
        let query = self.db.create_query(query_string.as_str())?;
        query.set_omit_excluded(Exclude::False);
        let messages = query.search_messages()?;
        Ok(messages.into_iter().next())
    }

    /// Returns a separate `Email` object for each duplicate email file mujmap owns.
    fn emails_from_message(&self, message: Message) -> Vec<Email> {
        lazy_static! {
            static ref MAIL_FILE: Regex = Regex::new(MAIL_PATTERN).unwrap();
        }
        message
            .filenames()
            .into_iter()
            .filter(|x| x.starts_with(&self.mail_cur_dir))
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
            .map(|(id, blob_id, path)| Email {
                id,
                blob_id,
                message_id: message.id().to_string(),
                path,
                tags: message
                    .tags()
                    .into_iter()
                    .filter(|tag| !AUTOMATIC_TAGS.contains(tag.as_str()))
                    .collect(),
            })
            .collect()
    }

    pub fn update_email_tags(
        &self,
        email: &Email,
        tags: HashSet<&str>,
    ) -> Result<(), notmuch::Error> {
        if let Some(message) = self.get_message(&email.message_id)? {
            // Build diffs for tags and apply them.
            message.freeze()?;
            let extant_tags: HashSet<String> = message.tags().into_iter().collect();
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
                "Updating local email: {email:?}, by adding tags: {tags_to_add:?}, removing tags: {tags_to_remove:?}"
            );
            for tag in tags_to_remove {
                message.remove_tag(tag)?;
            }
            for tag in tags_to_add {
                message.add_tag(tag)?;
            }
            message.thaw()?;
            if self.synchronize_maildir_flags {
                message.tags_to_maildir_flags()?;
            }
        }
        Ok(())
    }
}
