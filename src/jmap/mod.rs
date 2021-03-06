mod request;
mod response;
mod session;

use core::fmt;

pub use request::*;
pub use response::*;
use serde::{Deserialize, Serialize};
pub use session::*;

#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Debug, Clone)]
pub struct Id(pub String);

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Debug, Clone)]
pub struct State(pub String);

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Keywords that assign meaning to email.
///
/// Note that JMAP mandates that these be lowercase.
///
/// See <https://www.iana.org/assignments/imap-jmap-keywords/imap-jmap-keywords.xhtml>.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug, Serialize, Deserialize)]
pub enum EmailKeyword {
    #[serde(rename = "$draft")]
    Draft,
    #[serde(rename = "$seen")]
    Seen,
    #[serde(rename = "$flagged")]
    Flagged,
    #[serde(rename = "$answered")]
    Answered,
    #[serde(rename = "$forwarded")]
    Forwarded,
    #[serde(rename = "$junk")]
    Junk,
    #[serde(rename = "$notjunk")]
    NotJunk,
    #[serde(rename = "$phishing")]
    Phishing,
    #[serde(rename = "$important")]
    Important,
    #[serde(other)]
    Unknown,
}
