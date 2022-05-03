use serde::Deserialize;
use snafu::prelude::*;
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    string::FromUtf8Error,
};

use snafu::Snafu;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Could not read config file `{}': {}", filename.to_string_lossy(), source))]
    ReadConfigFile {
        filename: PathBuf,
        source: io::Error,
    },

    #[snafu(display("Could not parse config file `{}': {}", filename.to_string_lossy(), source))]
    ParseConfigFile {
        filename: PathBuf,
        source: toml::de::Error,
    },

    #[snafu(display("Can only specify one of `fqdn' or `session_url' in the same config"))]
    FqdnOrSessionUrl {},

    #[snafu(display("Must specify at least 1 for `concurrent_downloads'"))]
    ConcurrentDownloadsIsZero {},

    #[snafu(display("Could not execute password command `{}': {}", command, source))]
    ExecutePasswordCommand { command: String, source: io::Error },

    #[snafu(display(
        "Could not decode password command `{}' output as utf-8: {}",
        command,
        source
    ))]
    DecodePasswordCommand {
        command: String,
        source: FromUtf8Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Deserialize)]
pub struct Config {
    /// Username.
    pub username: String,

    /// Shell command which will print a password to stdout.
    pub password_command: String,

    /// Hostname to connect to.
    ///
    /// `mujmap` looks up the JMAP SRV record for this host to determine the
    /// JMAP session URL. Mutually exclusive with `session_url`.
    pub fqdn: Option<String>,

    /// Session URL to connect to.
    ///
    /// Mutually exclusive with `fqdn`.
    pub session_url: Option<String>,

    /// Number of email files to download in parallel.
    ///
    /// This corresponds to the number of blocking OS threads that will be
    /// created for HTTP download requests.
    #[serde(default = "default_concurrent_downloads")]
    pub concurrent_downloads: usize,
}

fn default_concurrent_downloads() -> usize {
    16
}

impl Config {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let contents = fs::read_to_string(path.as_ref()).context(ReadConfigFileSnafu {
            filename: path.as_ref(),
        })?;
        let config: Self = toml::from_str(contents.as_str()).context(ParseConfigFileSnafu {
            filename: path.as_ref(),
        })?;

        // Perform final validation.
        ensure!(
            config.fqdn.is_some() != config.session_url.is_some(),
            FqdnOrSessionUrlSnafu {}
        );
        ensure!(
            config.concurrent_downloads > 0,
            ConcurrentDownloadsIsZeroSnafu {}
        );
        Ok(config)
    }

    pub fn password(&self) -> Result<String> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(self.password_command.as_str())
            .output()
            .context(ExecutePasswordCommandSnafu {
                command: &self.password_command,
            })?;
        let stdout = String::from_utf8(output.stdout).context(DecodePasswordCommandSnafu {
            command: &self.password_command,
        })?;
        Ok(stdout)
    }
}
