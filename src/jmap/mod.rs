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
