use std::{fs, path::Path, process::Command};

use serde::Deserialize;
use snafu::{prelude::*, Whatever};

fn default_email_get_chunk_size() -> usize {
    200
}

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
    /// How many `Email`s to query for properties at a time.
    #[serde(default = "default_email_get_chunk_size")]
    pub email_get_chunk_size: usize,
}

impl Config {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, Whatever> {
        let contents =
            fs::read_to_string(path.as_ref().to_path_buf()).with_whatever_context(|source| {
                format!(
                    "Could not read config file `{}': {}",
                    path.as_ref().to_string_lossy(),
                    source
                )
            })?;
        let config: Self = toml::from_str(contents.as_str()).with_whatever_context(|source| {
            format!(
                "Could not parse config file `{}': {}",
                path.as_ref().to_string_lossy(),
                source
            )
        })?;

        // Perform final validation.
        if config.fqdn.is_some() && config.session_url.is_some() {
            whatever!("Must not specify both `fqdn' and `session_url' in the same config.");
        }
        Ok(config)
    }

    pub fn password(&self) -> Result<String, Whatever> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(self.password_command.as_str())
            .output()
            .with_whatever_context(|source| {
                format!(
                    "Could not execute password command `{}': {}",
                    self.password_command.as_str(),
                    source,
                )
            })?;
        let stdout = String::from_utf8(output.stdout).with_whatever_context(|source| {
            format!(
                "Could not interpret password command `{}' output as utf-8: {}",
                self.password_command.as_str(),
                source
            )
        })?;
        Ok(stdout)
    }
}
