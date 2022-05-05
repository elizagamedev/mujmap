use crate::jmap;
use crate::NewEmail;
use directories::ProjectDirs;
use snafu::prelude::*;
use snafu::Snafu;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Could not create cache dir `{}': {}", path.to_string_lossy(), source))]
    CreateCacheDir { path: PathBuf, source: io::Error },

    #[snafu(display("Could not create mail file `{}': {}", path.to_string_lossy(), source))]
    CreateMailFile { path: PathBuf, source: io::Error },

    #[snafu(display("Could not rename mail file from `{}' to `{}': {}", from.to_string_lossy(), to.to_string_lossy(), source))]
    RenameMailFile {
        from: PathBuf,
        to: PathBuf,
        source: io::Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub struct Cache {
    /// The path to mujmap's cache, where emails are downloaded before being
    /// placed in the maildir.
    cache_dir: PathBuf,
    /// The prefix to prepend to files in the cache.
    ///
    /// Cached blobs are stored as full paths in the same format that Emacs uses
    /// to store backup files, i.e. the path of the filename with each ! doubled
    /// and each directory separator replaced with a !. This is done because the
    /// JMAP spec does not specify that IDs should be globally unique across
    /// accounts, and regardless, the user might configure multiple instances of
    /// mujmap to manage multiple accounts on different services. As a result,
    /// the cached files look something like this:
    ///
    /// `!home!username!Maildir!username@example.com!cur!XxXxXx.YyYyYy`
    cached_file_prefix: String,
}

impl Cache {
    /// Open the local store.
    ///
    /// `mail_dir` *must* be a subdirectory of the notmuch path.
    pub fn open(mail_cur_dir: impl AsRef<Path>) -> Result<Self> {
        let project_dirs = ProjectDirs::from("sh.eliza", "", "mujmap").unwrap();
        let cache_dir = project_dirs.cache_dir();

        // Ensure the cache dir exists.
        fs::create_dir_all(&cache_dir).context(CreateCacheDirSnafu { path: cache_dir })?;

        // Create the cache filename prefix for this particular maildir. More
        // information about this is found in the documentation for
        // `Local::cached_file_prefix`.
        let mut cached_file_prefix = mail_cur_dir
            .as_ref()
            .to_string_lossy()
            .as_ref()
            .replace("!", "!!")
            .replace("/", "!");
        cached_file_prefix.push('!');

        Ok(Self {
            cache_dir: cache_dir.into(),
            cached_file_prefix,
        })
    }

    /// Return the path in the cache for the given IDs.
    pub fn cache_path(&self, email_id: &jmap::Id, blob_id: &jmap::Id) -> PathBuf {
        self.cache_dir.join(format!(
            "{}{}.{}",
            self.cached_file_prefix, email_id.0, blob_id.0
        ))
    }

    /// Save the data from the given reader into the cache.
    ///
    /// This is done first by downloading to a temporary file so that in the
    /// event of a catastrophic failure, e.g. sudden power outage, there will
    /// (hopefully less likely) be half-downloaded mail files. JMAP doesn't
    /// expose any means of checking data integrity other than comparing blob
    /// IDs, so it's important we take every precaution.
    pub fn download_into_cache(&self, new_email: &NewEmail, mut reader: impl Read) -> Result<()> {
        // Download to temporary file...
        let temporary_file_path = self.cache_dir.join(format!(
            "{}in_progress_download.{}",
            self.cached_file_prefix,
            rayon::current_thread_index().unwrap_or(0)
        ));
        let mut writer = File::create(&temporary_file_path).context(CreateMailFileSnafu {
            path: &temporary_file_path,
        })?;
        io::copy(&mut reader, &mut writer).context(CreateMailFileSnafu {
            path: &temporary_file_path,
        })?;
        // ...and move to its proper location.
        fs::rename(&temporary_file_path, &new_email.cache_path).context(RenameMailFileSnafu {
            from: &temporary_file_path,
            to: &new_email.cache_path,
        })?;
        Ok(())
    }
}
