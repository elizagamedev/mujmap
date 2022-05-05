use std::{
    collections::{HashMap, HashSet},
    io::{self, Read},
    time::Duration,
};

use crate::{
    config::{self, Config},
    jmap::{self, Id, State},
};
use indicatif::ProgressBar;
use itertools::Itertools;
use log::{trace, warn};
use serde::{de::DeserializeOwned, Serialize};
use snafu::prelude::*;
use trust_dns_resolver::{error::ResolveError, Resolver};
use uritemplate::UriTemplate;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Could not get password from config: {}", source))]
    GetPassword { source: config::Error },

    #[snafu(display("Could not determine DNS settings from resolv.conf: {}", source))]
    ParseResolvConf { source: io::Error },

    #[snafu(display("Could not lookup SRV address `{}': {}", address, source))]
    SrvLookup {
        address: String,
        source: ResolveError,
    },

    #[snafu(display("Could not resolve JMAP SRV record for {}: {}", hostname, source))]
    ResolveJmapSrvRecord {
        hostname: String,
        source: ureq::Error,
    },

    #[snafu(display("Could not open session at {}: {}", session_url, source))]
    OpenSession {
        session_url: String,
        source: ureq::Error,
    },

    #[snafu(display("Could not update session at {}: {}", session_url, source))]
    UpdateSession {
        session_url: String,
        source: ureq::Error,
    },

    #[snafu(display("Could not complete API request: {}", source))]
    Request { source: ureq::Error },

    #[snafu(display("Could not interpret API response: {}", source))]
    Response { source: io::Error },

    #[snafu(display("Could not serialize JSON request: {}", source))]
    SerializeRequest { source: serde_json::Error },

    #[snafu(display("Could not deserialize JSON response: {}", source))]
    DeserializeResponse { source: serde_json::Error },

    #[snafu(display("Unexpected response from server"))]
    UnexpectedResponse,

    #[snafu(display("Method-level JMAP error: {:?}", error))]
    MethodError { error: jmap::MethodResponseError },

    #[snafu(display("Could not read Email blob from server: {}", source))]
    ReadEmailBlobError { source: ureq::Error },

    #[snafu(display("Mailbox contained an invalid path"))]
    InvalidMailboxPath {},
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

struct HttpWrapper {
    /// Value of HTTP Authorization header.
    authorization: String,
    /// Persistent ureq agent to use for all HTTP requests.
    agent: ureq::Agent,
    /// Number of times to retry a request.
    retries: usize,
}

impl HttpWrapper {
    fn new(username: &str, password: &str, timeout: u64, retries: usize) -> Self {
        let safe_username = match username.find(':') {
            Some(idx) => &username[..idx],
            None => username,
        };
        let authorization = format!(
            "Basic {}",
            base64::encode(format!("{}:{}", safe_username, password))
        );
        let agent = ureq::AgentBuilder::new()
            .redirect_auth_headers(ureq::RedirectAuthHeaders::SameHost)
            .timeout(Duration::from_secs(timeout))
            .build();

        Self {
            agent,
            authorization,
            retries,
        }
    }

    fn get_session(&self, session_url: &str) -> Result<(String, jmap::Session), ureq::Error> {
        let response = self
            .agent
            .get(session_url)
            .set("Authorization", &self.authorization)
            .call()?;

        let session_url = response.get_url().to_string();
        let session: jmap::Session = response.into_json()?;
        Ok((session_url, session))
    }

    fn get_reader(&self, url: &str) -> Result<impl Read + Send> {
        let mut retry_count = 0;
        loop {
            match self
                .agent
                .get(url)
                .set("Authorization", &self.authorization)
                .call()
            {
                Ok(call) => {
                    return Ok(call
                        .into_reader()
                        // Limiting download size as advised by ureq's documentation:
                        // https://docs.rs/ureq/latest/ureq/struct.Response.html#method.into_reader
                        .take(10_000_000));
                }
                Err(e) => match e {
                    ureq::Error::Transport(_) => {
                        // Try again.
                        retry_count += 1;
                        if retry_count >= self.retries {
                            return Err(e).context(ReadEmailBlobSnafu {});
                        }
                        warn!(
                            "Transport error in GET on try {}, retrying: {}",
                            retry_count, e
                        );
                    }
                    _ => return Err(e).context(ReadEmailBlobSnafu {}),
                },
            };
        }
    }

    fn post<S: Serialize, D: DeserializeOwned>(&self, url: &str, body: S) -> Result<D> {
        self.agent
            .post(url)
            .set("Authorization", &self.authorization)
            .send_json(body)
            .context(RequestSnafu {})?
            .into_json()
            .context(ResponseSnafu {})
    }
}

pub struct Remote {
    http_wrapper: HttpWrapper,
    /// URL which points to the session endpoint after following all redirects.
    session_url: String,
    /// The latest session object returned by the server.
    pub session: jmap::Session,
}

impl Remote {
    pub fn open(config: &Config) -> Result<Self> {
        let password = config.password().context(GetPasswordSnafu {})?;
        match &config.fqdn {
            Some(fqdn) => Self::open_host(
                &fqdn,
                config.username.as_str(),
                &password,
                config.timeout,
                config.retries,
            ),
            None => Remote::open_url(
                &config.session_url.as_ref().unwrap(),
                config.username.as_str(),
                &password,
                config.timeout,
                config.retries,
            ),
        }
    }

    pub fn open_host(
        fqdn: &str,
        username: &str,
        password: &str,
        timeout: u64,
        retries: usize,
    ) -> Result<Self> {
        let resolver = Resolver::from_system_conf().context(ParseResolvConfSnafu {})?;
        let mut address = format!("_jmap._tcp.{}", fqdn);
        if !address.ends_with(".") {
            address.push('.');
        }
        let resolver_response = resolver
            .srv_lookup(address.as_str())
            .context(SrvLookupSnafu { address })?;

        let http_wrapper = HttpWrapper::new(username, password, timeout, retries);

        // Try all SRV names in order of priority.
        let mut last_err = None;
        for name in resolver_response
            .into_iter()
            .sorted_by_key(|x| x.priority())
        {
            let mut target = name.target().to_utf8();
            // Remove the final ".".
            assert!(target.ends_with("."));
            target.pop();

            let url = format!("https://{}:{}/.well-known/jmap", target, name.port());
            match http_wrapper.get_session(url.as_str()) {
                Ok((session_url, session)) => {
                    return Ok(Remote {
                        http_wrapper,
                        session_url,
                        session,
                    })
                }

                Err(e) => last_err = Some((url, e)),
            };
        }
        // All of them failed! Return the last error.
        let (session_url, error) = last_err.unwrap();
        Err(error).context(OpenSessionSnafu { session_url })
    }

    pub fn open_url(
        session_url: &str,
        username: &str,
        password: &str,
        timeout: u64,
        retries: usize,
    ) -> Result<Self> {
        let http_wrapper = HttpWrapper::new(username, password, timeout, retries);
        let (session_url, session) = http_wrapper
            .get_session(session_url)
            .context(OpenSessionSnafu { session_url })?;
        Ok(Remote {
            http_wrapper,
            session_url,
            session,
        })
    }

    /// Return a list of all `Email` IDs that exist on the server and a state
    /// `String` returned by `Email/get`.
    ///
    /// This function calls `Email/get` before `Email/query` in case any new
    /// `Email` objects appear in-between the call to `Email/query` and future
    /// calls to `Email/changes`. If done in the opposite order, an `Email`
    /// might slip through the cracks.
    pub fn all_email_ids(&mut self) -> Result<(State, HashSet<Id>)> {
        const GET_METHOD_ID: &str = "0";
        const QUERY_METHOD_ID: &str = "1";

        let account_id = &self.session.primary_accounts.mail;
        let mut response = self.request(jmap::Request {
            using: &[jmap::CapabilityKind::Mail],
            method_calls: &[
                jmap::RequestInvocation {
                    call: jmap::MethodCall::EmailGet {
                        get: jmap::MethodCallGet {
                            account_id,
                            ids: Some(&[]),
                            properties: Some(&[]),
                        },
                    },
                    id: GET_METHOD_ID,
                },
                jmap::RequestInvocation {
                    call: jmap::MethodCall::EmailQuery {
                        query: jmap::MethodCallQuery {
                            account_id,
                            position: 0,
                            anchor: None,
                            anchor_offset: 0,
                            limit: None,
                            calculate_total: false,
                        },
                    },
                    id: QUERY_METHOD_ID,
                },
            ],
            created_ids: None,
        })?;
        self.update_session_state(&response.session_state)?;

        if response.method_responses.len() != 2 {
            return Err(Error::UnexpectedResponse);
        }

        let query_response =
            expect_email_query(QUERY_METHOD_ID, response.method_responses.remove(1))?;

        let get_response = expect_email_get(GET_METHOD_ID, response.method_responses.remove(0))?;

        // If the server doesn't impose a limit, we're done.
        let limit = match query_response.limit {
            Some(limit) => limit,
            None => return Ok((get_response.state, query_response.ids.into_iter().collect())),
        };

        // Nonsense!
        if limit == 0 {
            return Err(Error::UnexpectedResponse);
        }

        // No need to continue processing if we have received fewer than the
        // limit imposed.
        if (query_response.ids.len() as u64) < limit {
            return Ok((get_response.state, query_response.ids.into_iter().collect()));
        }

        // If the server imposed a limit on our query, we must continue to make
        // requests until we have collected all of the IDs.
        let mut email_ids = query_response.ids;

        loop {
            let account_id = &self.session.primary_accounts.mail;
            let mut response = self.request(jmap::Request {
                using: &[jmap::CapabilityKind::Mail],
                method_calls: &[jmap::RequestInvocation {
                    call: jmap::MethodCall::EmailQuery {
                        query: jmap::MethodCallQuery {
                            account_id,
                            anchor: Some(&email_ids.last().unwrap()),
                            anchor_offset: 1,
                            position: 0,
                            limit: None,
                            calculate_total: false,
                        },
                    },
                    id: QUERY_METHOD_ID,
                }],
                created_ids: None,
            })?;
            self.update_session_state(&response.session_state)?;

            if response.method_responses.len() != 1 {
                return Err(Error::UnexpectedResponse);
            }

            let mut query_response =
                expect_email_query(QUERY_METHOD_ID, response.method_responses.remove(0))?;

            // We're done if we don't get any more IDs.
            if query_response.ids.is_empty() {
                break;
            }

            let len = query_response.ids.len();
            email_ids.append(&mut query_response.ids);

            let limit = match query_response.limit {
                Some(limit) => limit,
                // If we suddenly don't have a limit anymore, we must be done.
                None => break,
            };

            // Nonsense!
            if limit == 0 {
                return Err(Error::UnexpectedResponse);
            }

            // We're done if we get less email than the limit suggests.
            if (len as u64) < limit {
                break;
            }
        }
        Ok((get_response.state, email_ids.into_iter().collect()))
    }

    /// Given an `Email/get` state, return the latest `Email/get` state and a
    /// list of new/updated `Email` IDs and destroyed `Email` IDs.
    pub fn changed_email_ids(
        &mut self,
        state: State,
    ) -> Result<(State, HashSet<Id>, HashSet<Id>, HashSet<Id>)> {
        const CHANGES_METHOD_ID: &str = "0";

        let mut state = state;

        let mut created_ids = HashSet::new();
        let mut updated_ids = HashSet::new();
        let mut destroyed_ids = HashSet::new();

        loop {
            let account_id = &self.session.primary_accounts.mail;
            let mut response = self.request(jmap::Request {
                using: &[jmap::CapabilityKind::Mail],
                method_calls: &[jmap::RequestInvocation {
                    call: jmap::MethodCall::EmailChanges {
                        changes: jmap::MethodCallChanges {
                            account_id,
                            since_state: &state,
                            max_changes: None,
                        },
                    },
                    id: CHANGES_METHOD_ID,
                }],
                created_ids: None,
            })?;
            self.update_session_state(&response.session_state)?;

            if response.method_responses.len() != 1 {
                return Err(Error::UnexpectedResponse);
            }

            let changes_response =
                expect_email_changes(CHANGES_METHOD_ID, response.method_responses.remove(0))?;

            created_ids.extend(changes_response.created);
            updated_ids.extend(changes_response.updated);
            destroyed_ids.extend(changes_response.destroyed);

            state = changes_response.new_state;
            if !changes_response.has_more_changes {
                break;
            }
        }

        // It's possible something got put in both created and updated; make it
        // mutually exclusive.
        updated_ids.retain(|x| !created_ids.contains(x));

        Ok((state, created_ids, updated_ids, destroyed_ids))
    }

    /// Given a list of `Email` IDs, return a map of their IDs to their
    /// properties.
    pub fn get_emails<'a>(&mut self, email_ids: &HashSet<jmap::Id>) -> Result<HashMap<Id, Email>> {
        const GET_METHOD_ID: &str = "0";

        let pb = ProgressBar::new(email_ids.len() as u64);
        let chunk_size = self.session.capabilities.core.max_objects_in_get as usize;

        let mut emails: HashMap<Id, Email> = HashMap::new();

        for chunk in &email_ids.into_iter().chunks(chunk_size) {
            let account_id = &self.session.primary_accounts.mail;
            let ids = chunk.collect::<Vec<&Id>>();
            let mut response = self.request(jmap::Request {
                using: &[jmap::CapabilityKind::Mail],
                method_calls: &[jmap::RequestInvocation {
                    call: jmap::MethodCall::EmailGet {
                        get: jmap::MethodCallGet {
                            account_id,
                            ids: Some(&ids),
                            properties: Some(&["id", "blobId", "keywords", "mailboxIds"]),
                        },
                    },
                    id: GET_METHOD_ID,
                }],
                created_ids: None,
            })?;
            self.update_session_state(&response.session_state)?;

            if response.method_responses.len() != 1 {
                return Err(Error::UnexpectedResponse);
            }

            let get_response =
                expect_email_get(GET_METHOD_ID, response.method_responses.remove(0))?;

            for email in get_response.list {
                emails.insert(email.id.clone(), Email::from_jmap_email(email));
            }

            pb.inc(ids.len() as u64);
        }
        pb.finish_with_message("done");
        Ok(emails)
    }

    /// Return all `Mailbox`es.
    pub fn get_mailboxes<'a>(&mut self) -> Result<HashMap<jmap::Id, Mailbox>> {
        const GET_METHOD_ID: &str = "0";

        let account_id = &self.session.primary_accounts.mail;
        let mut response = self.request(jmap::Request {
            using: &[jmap::CapabilityKind::Mail],
            method_calls: &[jmap::RequestInvocation {
                call: jmap::MethodCall::MailboxGet {
                    get: jmap::MethodCallGet {
                        account_id,
                        ids: None,
                        properties: Some(&["id", "parentId", "name", "role"]),
                    },
                },
                id: GET_METHOD_ID,
            }],
            created_ids: None,
        })?;
        self.update_session_state(&response.session_state)?;

        if response.method_responses.len() != 1 {
            return Err(Error::UnexpectedResponse);
        }

        let get_response = expect_mailbox_get(GET_METHOD_ID, response.method_responses.remove(0))?;

        // Reinterpret the mailbox data.
        let jmap_mailboxes: HashMap<jmap::Id, jmap::Mailbox> = get_response
            .list
            .into_iter()
            .map(|x| (x.id.clone(), x))
            .collect();
        Ok(jmap_mailboxes
            .values()
            .map(|jmap_mailbox| {
                Ok(match jmap_mailbox.role {
                    // Ignore these mailboxes. They don't translate to useful
                    // tags, and we can safely ignore them when patching
                    // remotely.
                    Some(jmap::MailboxRole::All)
                    | Some(jmap::MailboxRole::Archive)
                    | Some(jmap::MailboxRole::Subscribed)
                    | Some(jmap::MailboxRole::Unknown) => None,
                    _ => {
                        // Determine full path, e.g. root-label/child-label.
                        let mut path_ids = vec![&jmap_mailbox.id];
                        let mut maybe_parent_id = &jmap_mailbox.parent_id;
                        while let Some(parent_id) = maybe_parent_id {
                            ensure!(!path_ids.contains(&parent_id), InvalidMailboxPathSnafu {});
                            path_ids.push(&parent_id);

                            let parent = jmap_mailboxes
                                .get(&parent_id)
                                .ok_or(Error::InvalidMailboxPath {})?;
                            maybe_parent_id = &parent.parent_id;
                        }
                        Some(
                            path_ids
                                .into_iter()
                                .rev()
                                .map(|x| {
                                    let mailbox = &jmap_mailboxes[&x];
                                    mailbox
                                        .role
                                        .map(|x| {
                                            if x == jmap::MailboxRole::Unknown {
                                                &mailbox.name
                                            } else {
                                                x.as_str()
                                            }
                                        })
                                        .unwrap_or(&mailbox.name)
                                })
                                .join("/"),
                        )
                    }
                }
                .map(|name| Mailbox {
                    id: jmap_mailbox.id.clone(),
                    name,
                }))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .map(|x| (x.id.clone(), x))
            .collect())
    }

    pub fn read_email_blob(&self, id: &Id) -> Result<impl Read + Send> {
        let uri = UriTemplate::new(self.session.download_url.as_str())
            .set("accountId", self.session.primary_accounts.mail.0.as_str())
            .set("blobId", id.0.as_str())
            .set("type", "text/plain")
            .set("name", id.0.as_str())
            .build();

        self.http_wrapper.get_reader(uri.as_str())
    }

    fn request<'a>(&self, request: jmap::Request<'a>) -> Result<jmap::Response> {
        self.http_wrapper.post(&self.session.api_url, request)
    }

    fn update_session_state(&mut self, session_state: &State) -> Result<()> {
        if *session_state != self.session.state {
            trace!(
                "updating session state from {} to {}",
                self.session.state,
                session_state
            );
            let (_, session) =
                self.http_wrapper
                    .get_session(&self.session_url)
                    .context(UpdateSessionSnafu {
                        session_url: &self.session_url,
                    })?;
            self.session = session;
            trace!("new session state is {}", self.session.state);
        }
        Ok(())
    }
}

/// An object which contains only the properties of a remote Email that mujmap
/// cares about.
#[derive(Debug)]
pub struct Email {
    pub id: Id,
    pub blob_id: Id,
    pub keywords: HashSet<jmap::EmailKeyword>,
    pub mailbox_ids: HashSet<Id>,
}

#[derive(Debug)]
pub struct Mailbox {
    pub id: Id,
    pub name: String,
}

impl Email {
    fn from_jmap_email(jmap_email: jmap::Email) -> Self {
        Self {
            id: jmap_email.id,
            blob_id: jmap_email.blob_id,
            keywords: jmap_email
                .keywords
                .into_iter()
                .filter(|(k, v)| *v && *k != jmap::EmailKeyword::Unknown)
                .map(|(k, _)| k)
                .collect(),
            mailbox_ids: jmap_email
                .mailbox_ids
                .into_iter()
                .filter(|(_, v)| *v)
                .map(|(k, _)| k)
                .collect(),
        }
    }
}

fn expect_email_get(
    id: &str,
    invocation: jmap::ResponseInvocation,
) -> Result<jmap::MethodResponseGet<jmap::Email>> {
    if invocation.id != id {
        return Err(Error::UnexpectedResponse);
    }
    match invocation.call {
        jmap::MethodResponse::EmailGet(get) => Ok(get),
        jmap::MethodResponse::Error(error) => Err(Error::MethodError { error }),
        _ => Err(Error::UnexpectedResponse),
    }
}

fn expect_email_query(
    id: &str,
    invocation: jmap::ResponseInvocation,
) -> Result<jmap::MethodResponseQuery> {
    if invocation.id != id {
        return Err(Error::UnexpectedResponse);
    }
    match invocation.call {
        jmap::MethodResponse::EmailQuery(query) => Ok(query),
        jmap::MethodResponse::Error(error) => Err(Error::MethodError { error }),
        _ => Err(Error::UnexpectedResponse),
    }
}

fn expect_email_changes(
    id: &str,
    invocation: jmap::ResponseInvocation,
) -> Result<jmap::MethodResponseChanges> {
    if invocation.id != id {
        return Err(Error::UnexpectedResponse);
    }
    match invocation.call {
        jmap::MethodResponse::EmailChanges(changes) => Ok(changes),
        jmap::MethodResponse::Error(error) => Err(Error::MethodError { error }),
        _ => Err(Error::UnexpectedResponse),
    }
}

fn expect_mailbox_get(
    id: &str,
    invocation: jmap::ResponseInvocation,
) -> Result<jmap::MethodResponseGet<jmap::Mailbox>> {
    if invocation.id != id {
        return Err(Error::UnexpectedResponse);
    }
    match invocation.call {
        jmap::MethodResponse::MailboxGet(get) => Ok(get),
        jmap::MethodResponse::Error(error) => Err(Error::MethodError { error }),
        _ => Err(Error::UnexpectedResponse),
    }
}
