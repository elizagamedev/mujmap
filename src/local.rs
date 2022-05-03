use crate::jmap;
use const_format::formatcp;
use lazy_static::lazy_static;
use log::debug;
use notmuch::Database;
use notmuch::Message;
use notmuch::Messages;
use path_absolutize::*;
use regex::Regex;
use snafu::prelude::*;
use snafu::Snafu;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

const ID_PATTERN: &'static str = r"[-A-Za-z0-9_]+";
const MAIL_PATTERN: &'static str = formatcp!(r"^({})\.({})(?:$|:)", ID_PATTERN, ID_PATTERN);

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
    /// Is this a dry run?
    dry_run: bool,
}

impl Local {
    /// Open the local store.
    ///
    /// `mail_dir` *must* be a subdirectory of the notmuch path.
    pub fn open(mail_dir: impl AsRef<Path>, dry_run: bool) -> Result<Self> {
        // Open the notmuch database with default config options.
        let db = Database::open_with_config::<PathBuf, PathBuf>(
            None,
            notmuch::DatabaseMode::ReadOnly,
            None,
            None,
        )
        .context(OpenDatabaseSnafu {})?;

        // Build new absolute path resolving all relative paths. Check to make
        // sure it's actually a subdirectory of the notmuch root path.
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
            dry_run,
        })
    }

    pub fn revision(&self) -> u64 {
        self.db.revision().revision
    }

    /// Return a map of all `Email` IDs to `Email` objects.
    pub fn all_email(&self) -> Result<HashMap<jmap::Id, Email>> {
        Ok(self
            .query(&self.all_mail_query)?
            .into_iter()
            .flat_map(|x| Email::from_message(x))
            .map(|x| (x.id.clone(), x))
            .collect())
    }

    /// Return a list of all `Message`s that mujmap manages which were
    /// modified since the given database revision.
    pub fn all_email_since(&self, last_revision: u64) -> Result<Messages> {
        self.query(&format!(
            "{} and lastmod:{}..{}",
            self.all_mail_query,
            last_revision,
            self.revision()
        ))
    }

    fn query(&self, query_string: &str) -> Result<Messages> {
        debug!("notmuch query: {}", query_string);

        let query =
            self.db
                .create_query(query_string)
                .with_context(|_| CreateNotmuchQuerySnafu {
                    query: query_string.clone(),
                })?;
        let messages = query
            .search_messages()
            .with_context(|_| ExecuteNotmuchQuerySnafu {
                query: query_string.clone(),
            })?;
        Ok(messages)
    }
}

pub struct Email {
    pub id: jmap::Id,
    pub blob_id: jmap::Id,
    pub message: Message,
}

impl Email {
    fn from_message(message: Message) -> Option<Self> {
        lazy_static! {
            static ref MAIL_FILE: Regex = Regex::new(MAIL_PATTERN).unwrap();
        }
        message
            .filename()
            .file_name()
            .and_then(|x| {
                MAIL_FILE.captures(&x.to_string_lossy()).map(|x| {
                    let id = jmap::Id(x.get(0).unwrap().as_str().to_string());
                    let blob_id = jmap::Id(x.get(1).unwrap().as_str().to_string());
                    (id, blob_id)
                })
            })
            .map(|(id, blob_id)| Self {
                id,
                blob_id,
                message,
            })
    }
}
