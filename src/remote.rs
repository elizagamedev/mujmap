use std::{
    collections::{HashMap, HashSet},
    io::{self, Read},
    time::Duration,
};

use crate::{
    config::{self, Config},
    jmap::{self, EmailKeyword, Id, MailboxRole, State},
    local,
};
use itertools::Itertools;
use lazy_static::lazy_static;
use log::{debug, log_enabled, trace, warn};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use snafu::prelude::*;
use trust_dns_resolver::{error::ResolveError, Resolver};
use uritemplate::UriTemplate;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Could not get password from config: {}", source))]
    GetPassword { source: config::Error },

    #[snafu(display("Couldn't determine domain name from `username`"))]
    NoDomainName {},

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

    #[snafu(display("Could not deserialize API response: {}", source))]
    DeserializeResponse { source: serde_json::Error },

    #[snafu(display("Unexpected response from server"))]
    UnexpectedResponse,

    #[snafu(display("Method-level JMAP error: {:?}", error))]
    MethodError { error: jmap::MethodResponseError },

    #[snafu(display("Could not read Email blob from server: {}", source))]
    ReadEmailBlobError { source: ureq::Error },

    #[snafu(display("Could not find an archive mailbox"))]
    NoArchive {},

    #[snafu(display("Mailbox contained an invalid path"))]
    InvalidMailboxPath {},

    #[snafu(display("Failed to update messages on server: {:?}", not_updated))]
    UpdateEmail {
        not_updated: HashMap<jmap::Id, jmap::MethodResponseError>,
    },

    #[snafu(display("Failed to import email: {}", source))]
    ImportEmail { source: jmap::MethodResponseError },

    #[snafu(display("Failed to destroy email: {}", source))]
    DestroyEmail { source: jmap::MethodResponseError },

    #[snafu(display("Failed to create email submission: {}", source))]
    CreateEmailSubmission { source: jmap::MethodResponseError },

    #[snafu(display("Failed to update submitted email: {}", source))]
    UpdateSubmittedEmail { source: jmap::MethodResponseError },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

struct HttpWrapper {
    /// Value of HTTP Authorization header.
    authorization: Option<String>,
    /// Persistent ureq agent to use for all HTTP requests.
    agent: ureq::Agent,
}

impl HttpWrapper {
    fn new(authorization: Option<String>, timeout: u64) -> Self {
        let agent = ureq::AgentBuilder::new()
            .redirect_auth_headers(ureq::RedirectAuthHeaders::SameHost)
            .timeout(Duration::from_secs(timeout))
            .build();

        Self {
            authorization,
            agent,
        }
    }

    fn apply_authorization(&self, req: ureq::Request) -> ureq::Request {
        match &self.authorization {
            Some(authorization) => req.set("Authorization", authorization),
            _ => req,
        }
    }

    fn get_session(&self, session_url: &str) -> Result<(String, jmap::Session), ureq::Error> {
        let response = self
            .apply_authorization(self.agent.get(session_url))
            .call()?;

        let session_url = response.get_url().to_string();
        let session: jmap::Session = response.into_json()?;
        Ok((session_url, session))
    }

    fn get_reader(&self, url: &str) -> Result<impl Read + Send> {
        Ok(self
            .apply_authorization(self.agent.get(url))
            .call()
            .context(ReadEmailBlobSnafu {})?
            .into_reader()
            // Limiting download size as advised by ureq's documentation:
            // https://docs.rs/ureq/latest/ureq/struct.Response.html#method.into_reader
            .take(10_000_000))
    }

    fn post_string<D: DeserializeOwned>(&self, url: &str, body: &str) -> Result<D> {
        let post = self
            .apply_authorization(self.agent.post(url))
            .send_string(body)
            .context(RequestSnafu {})?;
        if log_enabled!(log::Level::Trace) {
            let json = post.into_string().context(ResponseSnafu {})?;
            trace!("Post response: {json}");
            serde_json::from_str(&json).context(DeserializeResponseSnafu {})
        } else {
            post.into_json().context(ResponseSnafu {})
        }
    }

    fn post_json<S: Serialize, D: DeserializeOwned>(&self, url: &str, body: S) -> Result<D> {
        let post = self
            .apply_authorization(self.agent.post(url))
            .send_json(body)
            .context(RequestSnafu {})?;
        if log_enabled!(log::Level::Trace) {
            let json = post.into_string().context(ResponseSnafu {})?;
            trace!("Post response: {json}");
            serde_json::from_str(&json).context(DeserializeResponseSnafu {})
        } else {
            post.into_json().context(ResponseSnafu {})
        }
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
        match (&config.fqdn, &config.session_url) {
            (Some(fqdn), _) => {
                Self::open_host(&fqdn, config.username.as_str(), &password, config.timeout)
            }
            (_, Some(session_url)) => Remote::open_url(
                &session_url.as_str(),
                config.username.as_str(),
                &password,
                config.timeout,
            ),
            _ => {
                let (_, domain) = config
                    .username
                    .split_once('@')
                    .context(NoDomainNameSnafu {})?;
                Self::open_host(domain, config.username.as_str(), &password, config.timeout)
            }
        }
    }

    fn open_host(fqdn: &str, username: &str, password: &str, timeout: u64) -> Result<Self> {
        let resolver = Resolver::from_system_conf().context(ParseResolvConfSnafu {})?;
        let mut address = format!("_jmap._tcp.{}", fqdn);
        if !address.ends_with(".") {
            address.push('.');
        }
        let resolver_response = resolver
            .srv_lookup(address.as_str())
            .context(SrvLookupSnafu { address })?;

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
            match Self::open_url(url.as_str(), username, password, timeout) {
                Ok(s) => return Ok(s),
                Err(e) => last_err = Some(e),
            };
        }
        // All of them failed! Return the last error.
        Err(last_err.unwrap())
    }

    fn open_url(session_url: &str, username: &str, password: &str, timeout: u64) -> Result<Self> {
        let agent = ureq::AgentBuilder::new()
            .redirect_auth_headers(ureq::RedirectAuthHeaders::SameHost)
            .timeout(Duration::from_secs(timeout))
            .build();

        match agent.get(session_url).call() {
            Ok(r) => {
                // Server returned success without authentication. Surprising, but valid.
                let session_url = r.get_url().to_string();
                let session: jmap::Session = r.into_json().context(ResponseSnafu {})?;
                Ok(Self {
                    http_wrapper: HttpWrapper::new(None, timeout),
                    session_url,
                    session,
                })
            }

            Err(ureq::Error::Status(code, ref r)) if code == 401 => {
                let safe_username = match username.find(':') {
                    Some(idx) => &username[..idx],
                    None => username,
                };
                let authorization = format!(
                    "Basic {}",
                    base64::encode(format!("{}:{}", safe_username, password))
                );

                let r = agent
                    .get(r.get_url())
                    .set("Authorization", &authorization)
                    .call()
                    .context(OpenSessionSnafu { session_url })?;

                let session_url = r.get_url().to_string();
                let session: jmap::Session = r.into_json().context(ResponseSnafu {})?;
                Ok(Self {
                    http_wrapper: HttpWrapper::new(Some(authorization), timeout),
                    session_url,
                    session,
                })
            }

            Err(e) => Err(e).context(OpenSessionSnafu { session_url }),
        }
    }

    /// Return a list of all `Email` IDs that exist on the server and a state `String` returned by
    /// `Email/get`.
    ///
    /// This function calls `Email/get` before `Email/query` in case any new `Email` objects appear
    /// in-between the call to `Email/query` and future calls to `Email/changes`. If done in the
    /// opposite order, an `Email` might slip through the cracks.
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

        // No need to continue processing if we have received fewer than the limit imposed.
        if (query_response.ids.len() as u64) < limit {
            return Ok((get_response.state, query_response.ids.into_iter().collect()));
        }

        // If the server imposed a limit on our query, we must continue to make requests until we
        // have collected all of the IDs.
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

    /// Given an `Email/get` state, return the latest `Email/get` state and a list of new/updated
    /// `Email` IDs and destroyed `Email` IDs.
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

        // It's possible something got put in both created and updated; make it mutually exclusive.
        updated_ids.retain(|x| !created_ids.contains(x));

        Ok((state, created_ids, updated_ids, destroyed_ids))
    }

    /// Given a list of `Email` IDs, return a map of their IDs to their properties.
    pub fn get_emails<'a>(
        &mut self,
        email_ids: impl Iterator<Item = &'a jmap::Id>,
        mailboxes: &Mailboxes,
        tags_config: &config::Tags,
    ) -> Result<HashMap<Id, Email>> {
        const GET_METHOD_ID: &str = "0";

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
                emails.insert(
                    email.id.clone(),
                    Email::from_jmap_email(email, mailboxes, tags_config),
                );
            }
        }
        Ok(emails)
    }

    /// Return the `Mailboxes` of the server.
    pub fn get_mailboxes<'a>(&mut self, tags_config: &config::Tags) -> Result<Mailboxes> {
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

        // The archive is special. All email must belong to at least one mailbox, so if an email has
        // no notmuch tags which correspond to other mailboxes, it must be added to the archive.
        let archive_id = jmap_mailboxes
            .values()
            .filter(|x| x.role == Some(MailboxRole::Archive))
            .map(|x| x.id.clone())
            .next()
            .ok_or(Error::NoArchive {})?;

        // Collect the list of available special mailboxes.
        let mut roles: AvailableMailboxRoles = Default::default();
        for mailbox in jmap_mailboxes.values() {
            if let Some(role) = mailbox.role {
                match role {
                    MailboxRole::Drafts => roles.draft = Some(mailbox.id.clone()),
                    MailboxRole::Flagged => roles.flagged = Some(mailbox.id.clone()),
                    MailboxRole::Important => roles.important = Some(mailbox.id.clone()),
                    MailboxRole::Junk => roles.spam = Some(mailbox.id.clone()),
                    MailboxRole::Trash => roles.deleted = Some(mailbox.id.clone()),
                    MailboxRole::Sent => roles.sent = Some(mailbox.id.clone()),
                    _ => {}
                }
            }
        }

        // Returns true if the mailbox should be ignored if this role appears *any* point in the
        // path heirarchy. Namely, if the user has explicitly disabled tags for this role.
        let should_ignore_mailbox_role = |maybe_role: &Option<MailboxRole>| match maybe_role {
            Some(x) => match x {
                MailboxRole::Important => tags_config.important.is_empty(),
                MailboxRole::Inbox => tags_config.inbox.is_empty(),
                MailboxRole::Junk => tags_config.spam.is_empty(),
                MailboxRole::Sent => tags_config.sent.is_empty(),
                MailboxRole::Trash => tags_config.deleted.is_empty(),
                _ => false,
            },
            None => false,
        };

        let lowercase_names: HashMap<&Id, String> = jmap_mailboxes
            .iter()
            .map(|(id, mailbox)| (id, mailbox.name.to_lowercase()))
            .collect();

        // Gather the mailbox objects.
        let mailboxes_by_id: HashMap<Id, Mailbox> = jmap_mailboxes
            .values()
            .map(|jmap_mailbox| {
                if jmap_mailbox.role == Some(MailboxRole::All)
                    || jmap_mailbox.role == Some(MailboxRole::Archive)
                    || should_ignore_mailbox_role(&jmap_mailbox.role)
                {
                    return Ok(None);
                }
                // Determine full path, e.g. root-label/child-label/etc.
                let mut path_ids = vec![&jmap_mailbox.id];
                let mut maybe_parent_id = &jmap_mailbox.parent_id;
                while let Some(parent_id) = maybe_parent_id {
                    // Make sure there isn't a loop.
                    ensure!(!path_ids.contains(&parent_id), InvalidMailboxPathSnafu {});
                    path_ids.push(&parent_id);
                    let parent = jmap_mailboxes
                        .get(&parent_id)
                        .ok_or(Error::InvalidMailboxPath {})?;
                    if should_ignore_mailbox_role(&parent.role) {
                        return Ok(None);
                    }
                    maybe_parent_id = &parent.parent_id;
                }
                let tag = path_ids
                    .into_iter()
                    .rev()
                    .map(|x| {
                        let mailbox = &jmap_mailboxes[&x];
                        mailbox
                            .role
                            .map(|x| match x {
                                MailboxRole::Drafts => Some("draft"),
                                MailboxRole::Flagged => Some("flagged"),
                                MailboxRole::Important => Some(tags_config.important.as_str()),
                                MailboxRole::Inbox => Some(tags_config.inbox.as_str()),
                                MailboxRole::Junk => Some(tags_config.spam.as_str()),
                                MailboxRole::Sent => Some(tags_config.sent.as_str()),
                                MailboxRole::Trash => Some(tags_config.deleted.as_str()),
                                _ => None,
                            })
                            .flatten()
                            .unwrap_or_else(|| {
                                if tags_config.lowercase {
                                    &lowercase_names[&x]
                                } else {
                                    &mailbox.name
                                }
                            })
                    })
                    .join(&tags_config.directory_separator);
                Ok(Some((
                    jmap_mailbox.id.clone(),
                    Mailbox::new(jmap_mailbox.id.clone(), tag),
                )))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .filter(|(_, mailbox)| {
                // Filter out mailboxes with the same tag as automatic tags and warn the user that
                // they shouldn't do this.
                if local::AUTOMATIC_TAGS.contains(mailbox.tag.as_str()) {
                    warn!(
                        concat!(
                            "The JMAP server contains a mailbox `{}' which has the same name",
                            " as an automatic tag. This mailbox will be ignored."
                        ),
                        mailbox.tag
                    );
                    false
                } else {
                    true
                }
            })
            .collect();
        let ids_by_tag: HashMap<_, _> = mailboxes_by_id
            .iter()
            .map(|(id, mailbox)| (mailbox.tag.clone(), id.clone()))
            .collect();

        let ignored_ids = jmap_mailboxes
            .values()
            .filter(|x| archive_id != x.id && !mailboxes_by_id.contains_key(&x.id))
            .map(|x| x.id.clone())
            .collect();

        Ok(Mailboxes {
            archive_id,
            mailboxes_by_id,
            ids_by_tag,
            ignored_ids,
            roles,
        })
    }

    /// Create mailboxes on the server which correspond to the given list of notmuch tags.
    pub fn create_mailboxes(
        &mut self,
        mailboxes: &mut Mailboxes,
        tags: &[String],
        tags_config: &config::Tags,
    ) -> Result<()> {
        let mut created_tags_by_id = Vec::new();
        let mut created_ids_by_tag = HashMap::new();
        let mut create_calls = Vec::new();

        // Creates ancestors for this tag recursively if they do not exist, then returns the ID of
        // its parent.
        fn get_or_create_mailbox_id<'a>(
            tag: &'a str,
            account_id: &'a Id,
            mailboxes: &Mailboxes,
            tags_config: &'a config::Tags,
            created_tags_by_id: &'a mut Vec<String>,
            created_ids_by_tag: &'a mut HashMap<String, Id>,
            create_calls: &'a mut Vec<(jmap::Id, jmap::MailboxCreate)>,
        ) -> Id {
            let (parent_id, name) = match tag.rfind(&tags_config.directory_separator) {
                Some(index) => {
                    let parent_id = get_or_create_mailbox_id(
                        &tag[..index],
                        account_id,
                        mailboxes,
                        tags_config,
                        created_tags_by_id,
                        created_ids_by_tag,
                        create_calls,
                    );
                    let name = &tag[index + 1..];
                    (Some(parent_id), name)
                }
                None => (None, tag),
            };
            // Return this ID if it already exists.
            if let Some(id) = [created_ids_by_tag.get(tag), mailboxes.ids_by_tag.get(tag)]
                .into_iter()
                .flatten()
                .next()
            {
                return id.clone();
            }
            // Create it!
            let id = create_calls.len();
            let create_id = Id(format!("{}", id));
            let ref_id = Id(format!("#{}", id));
            create_calls.push((
                create_id,
                jmap::MailboxCreate {
                    parent_id,
                    name: name.to_owned(),
                },
            ));
            created_tags_by_id.push(tag.to_string());
            created_ids_by_tag.insert(tag.to_string(), ref_id.clone());
            ref_id
        }
        // Build the requests. This function may create mailboxes which are children of other
        // mailboxes created in the same request. JMAP does support this, but these creation
        // requests must be ordered from parent to child. One way to guarantee this in a
        // not-so-clever way is to sort them by the length of the tag.
        let (calls_len, response) = {
            let account_id = &self.session.primary_accounts.mail;
            for tag in tags.iter().sorted_unstable_by_key(|x| x.len()) {
                get_or_create_mailbox_id(
                    &tag,
                    account_id,
                    mailboxes,
                    tags_config,
                    &mut created_tags_by_id,
                    &mut created_ids_by_tag,
                    &mut create_calls,
                );
            }

            debug!("Built calls for creating mailboxes: {:?}", create_calls);

            let method_calls: Vec<_> = create_calls
                .iter()
                .map(|(id, mailbox_create)| {
                    let mut create = HashMap::new();
                    create.insert(id, mailbox_create);
                    jmap::RequestInvocation {
                        call: jmap::MethodCall::MailboxSet {
                            set: jmap::MethodCallSet {
                                account_id,
                                if_in_state: None,
                                create: Some(create),
                                update: None,
                                destroy: None,
                            },
                        },
                        id: &id.0,
                    }
                })
                .collect();

            let response = self.request(jmap::Request {
                using: &[jmap::CapabilityKind::Mail],
                method_calls: &method_calls,
                created_ids: None,
            })?;
            (method_calls.len(), response)
        };
        self.update_session_state(&response.session_state)?;

        if response.method_responses.len() != calls_len {
            return Err(Error::UnexpectedResponse);
        }

        // Insert the newly created mailboxes into the `Mailboxes`.
        for (create_id, invocation) in response.method_responses.into_iter().enumerate() {
            let invocation_id = format!("{}", create_id);
            let set = expect_mailbox_set(&invocation_id, invocation)?;
            let mut created = set.created.ok_or(Error::UnexpectedResponse)?;
            let tag = created_tags_by_id[create_id].to_owned();
            let mailbox = created
                .remove(&Id(invocation_id))
                .ok_or(Error::UnexpectedResponse)?;
            mailboxes
                .mailboxes_by_id
                .insert(mailbox.id.clone(), Mailbox::new(mailbox.id, tag));
        }

        Ok(())
    }

    /// Return all `jmap::Identity` objects from the server.
    pub fn get_identities<'a>(&mut self) -> Result<Vec<jmap::Identity>> {
        const GET_METHOD_ID: &str = "0";

        let account_id = &self.session.primary_accounts.mail;
        let mut response = self.request(jmap::Request {
            using: &[jmap::CapabilityKind::Submission],
            method_calls: &[jmap::RequestInvocation {
                call: jmap::MethodCall::IdentityGet {
                    get: jmap::MethodCallGet {
                        account_id,
                        ids: None,
                        properties: Some(&["id", "email"]),
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

        let get_response = expect_identity_get(GET_METHOD_ID, response.method_responses.remove(0))?;
        Ok(get_response.list)
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

    /// Update all emails on the server with keywords and mailbox IDs corresponding to the local
    /// notmuch tags.
    pub fn update(
        &mut self,
        local_emails: &HashMap<Id, local::Email>,
        mailboxes: &Mailboxes,
        tags_config: &config::Tags,
    ) -> Result<()> {
        // Get the latest remote email objects for the set of local emails so that we can determine
        // if we should include any ignored mailboxes in the patch.
        let remote_emails = self.get_emails(local_emails.keys(), mailboxes, tags_config)?;

        // Build patches.
        let updates = local_emails
            .iter()
            .flat_map(|(id, local_email)| {
                let mut patch = HashMap::new();
                fn as_value(b: bool) -> Value {
                    if b {
                        Value::Bool(true)
                    } else {
                        Value::Null
                    }
                }

                // The remote email may have been destroyed.
                let remote_email = match remote_emails.get(id) {
                    Some(x) => x,
                    None => return None,
                };

                // Keywords.
                patch.insert(
                    "keywords/$draft",
                    as_value(local_email.tags.contains("draft")),
                );
                patch.insert(
                    "keywords/$seen",
                    as_value(!local_email.tags.contains("unread")),
                );
                patch.insert(
                    "keywords/$flagged",
                    as_value(local_email.tags.contains("flagged")),
                );
                patch.insert(
                    "keywords/$answered",
                    as_value(local_email.tags.contains("replied")),
                );
                patch.insert(
                    "keywords/$forwarded",
                    as_value(local_email.tags.contains("passed")),
                );
                if mailboxes.roles.spam.is_none() && !tags_config.spam.is_empty() {
                    let spam = local_email.tags.contains(&tags_config.spam);
                    patch.insert("keywords/$junk", as_value(spam));
                    patch.insert("keywords/$notjunk", as_value(!spam));
                }
                if !tags_config.phishing.is_empty() {
                    patch.insert(
                        "keywords/$phishing",
                        as_value(local_email.tags.contains(&tags_config.phishing)),
                    );
                }
                // Set mailboxes.
                // TODO: eliminate clone here?
                // Include all ignored mailboxes which the remote email is already included in.
                let mut new_mailboxes: serde_json::Map<String, Value> = remote_email
                    .mailbox_ids
                    .iter()
                    .filter(|x| mailboxes.ignored_ids.contains(x))
                    .map(|x| (x.0.clone(), Value::Bool(true)))
                    .collect();
                // Include all mailboxes which correspond to notmuch tags.
                new_mailboxes.extend(
                    mailboxes
                        .mailboxes_by_id
                        .values()
                        .filter(|x| local_email.tags.contains(&x.tag))
                        .map(|x| (x.id.0.clone(), Value::Bool(true))),
                );
                // If no mailboxes were found, assign to Archive.
                if new_mailboxes.is_empty() {
                    new_mailboxes.insert(mailboxes.archive_id.0.clone(), Value::Bool(true));
                }
                patch.insert("mailboxIds", Value::Object(new_mailboxes));
                Some(Ok((id, patch)))
            })
            .collect::<Result<HashMap<&Id, HashMap<&str, Value>>>>()?;
        debug!("Built patch for remote: {:?}", updates);

        // Send it off into cyberspace~
        const SET_METHOD_ID: &str = "0";

        let chunk_size = self.session.capabilities.core.max_objects_in_set as usize;

        for chunk in &updates.into_iter().chunks(chunk_size) {
            let account_id = &self.session.primary_accounts.mail;
            let mut response = self.request(jmap::Request {
                using: &[jmap::CapabilityKind::Mail],
                method_calls: &[jmap::RequestInvocation {
                    call: jmap::MethodCall::EmailSet {
                        set: jmap::MethodCallSet {
                            account_id,
                            if_in_state: None,
                            create: None,
                            update: Some(chunk.collect::<HashMap<_, _>>()),
                            destroy: None,
                        },
                    },
                    id: SET_METHOD_ID,
                }],
                created_ids: None,
            })?;
            self.update_session_state(&response.session_state)?;

            if response.method_responses.len() != 1 {
                return Err(Error::UnexpectedResponse);
            }

            let set_response =
                expect_email_set(SET_METHOD_ID, response.method_responses.remove(0))?;

            if let Some(not_updated) = set_response.not_updated {
                return Err(Error::UpdateEmail { not_updated });
            }
        }

        Ok(())
    }

    /// Send an email with the given body.
    pub fn send_email(
        &mut self,
        identity_id: jmap::Id,
        mailboxes: &Mailboxes,
        from_address: &str,
        to_addresses: &HashSet<String>,
        email: &str,
    ) -> Result<()> {
        const IMPORT_EMAIL_METHOD_ID: &str = "0";
        const SET_EMAIL_SUBMISSION_METHOD_ID: &str = "1";
        lazy_static! {
            static ref EMAIL_CLIENT_ID: jmap::Id = jmap::Id("0".into());
            static ref EMAIL_CLIENT_ID_REF: jmap::Id = jmap::Id("#0".into());
            static ref EMAIL_SUBMISSION_CLIENT_ID: jmap::Id = jmap::Id("1".into());
            static ref EMAIL_SUBMISSION_CLIENT_ID_REF: jmap::Id = jmap::Id("#1".into());
        }

        let blob_id = self.upload_blob(email)?.blob_id;

        let draft_mailbox_id = mailboxes
            .roles
            .draft
            .as_ref()
            .unwrap_or(&mailboxes.archive_id);
        let sent_mailbox_id = mailboxes
            .roles
            .sent
            .as_ref()
            .unwrap_or(&mailboxes.archive_id);

        let draft_mailbox_patch = format!("mailboxIds/{}", draft_mailbox_id.0);
        let sent_mailbox_patch = format!("mailboxIds/{}", sent_mailbox_id.0);

        // TODO: Set $answered and $forwarded properties here?
        let mut on_success_update_email = HashMap::from([("keywords/$draft", Value::Null)]);
        if draft_mailbox_id != sent_mailbox_id {
            on_success_update_email.insert(&draft_mailbox_patch, Value::Null);
            on_success_update_email.insert(&sent_mailbox_patch, Value::Bool(true));
        }

        let account_id = &self.session.primary_accounts.mail;
        let rcpt_to: Vec<_> = to_addresses
            .iter()
            .map(|x| jmap::Address { email: x.as_str() })
            .collect();
        let mut response = self.request(jmap::Request {
            using: &[jmap::CapabilityKind::Mail, jmap::CapabilityKind::Submission],
            method_calls: &[
                jmap::RequestInvocation {
                    call: jmap::MethodCall::EmailImport {
                        account_id,
                        emails: HashMap::from([(
                            &*EMAIL_CLIENT_ID,
                            jmap::EmailImport {
                                blob_id,
                                mailbox_ids: HashMap::from([(draft_mailbox_id, true)]),
                                keywords: HashMap::from([
                                    (EmailKeyword::Draft, true),
                                    (EmailKeyword::Seen, true),
                                ]),
                            },
                        )]),
                    },
                    id: IMPORT_EMAIL_METHOD_ID,
                },
                jmap::RequestInvocation {
                    call: jmap::MethodCall::EmailSubmissionSet {
                        set: jmap::MethodCallSet {
                            account_id,
                            if_in_state: None,
                            create: Some(HashMap::from([(
                                &*EMAIL_SUBMISSION_CLIENT_ID,
                                &jmap::EmailSubmissionCreate {
                                    identity_id: &identity_id,
                                    email_id: &*EMAIL_CLIENT_ID_REF,
                                    envelope: jmap::Envelope {
                                        mail_from: jmap::Address {
                                            email: from_address,
                                        },
                                        rcpt_to: &rcpt_to,
                                    },
                                },
                            )])),
                            update: None,
                            destroy: None,
                        },
                        on_success_update_email: Some(HashMap::from([(
                            &*EMAIL_SUBMISSION_CLIENT_ID_REF,
                            on_success_update_email,
                        )])),
                    },
                    id: SET_EMAIL_SUBMISSION_METHOD_ID,
                },
            ],
            created_ids: None,
        })?;
        self.update_session_state(&response.session_state)?;

        // Pop the responses off one at a time so that we process errors in order in case of a
        // partial success.

        // Verify that the email was created and get its ID.
        if response.method_responses.is_empty() {
            return Err(Error::UnexpectedResponse);
        }
        let import_response =
            expect_email_import(IMPORT_EMAIL_METHOD_ID, response.method_responses.remove(0))?;
        map_first_method_error_into_result(import_response.not_created)
            .context(ImportEmailSnafu {})?;
        let imported_email_id = import_response
            .created
            .and_then(|x| x.into_iter().map(|(_, object)| object.id).next())
            .context(UnexpectedResponseSnafu {})?;

        // Verify that the rest of the submission succeeded. If it doesn't, we destroy the draft we
        // just uploaded.
        let mut verify_submission = || -> Result<()> {
            if response.method_responses.is_empty() {
                return Err(Error::UnexpectedResponse);
            }
            let set_email_submission_response = expect_email_submission_set(
                SET_EMAIL_SUBMISSION_METHOD_ID,
                response.method_responses.remove(0),
            )?;
            map_first_method_error_into_result(set_email_submission_response.not_created)
                .context(CreateEmailSubmissionSnafu {})?;

            if response.method_responses.is_empty() {
                return Err(Error::UnexpectedResponse);
            }
            let set_email_response = expect_email_set(
                SET_EMAIL_SUBMISSION_METHOD_ID,
                response.method_responses.remove(0),
            )?;
            map_first_method_error_into_result(set_email_response.not_created)
                .context(UpdateSubmittedEmailSnafu {})?;

            Ok(())
        };

        if let Err(e) = verify_submission() {
            // Delete the email we created and fail as normal.
            if let Err(e) = self.destroy_email(&imported_email_id) {
                warn!("Could not destroy draft: {e}");
            }
            return Err(e);
        };
        Ok(())
    }

    fn destroy_email(&mut self, id: &jmap::Id) -> Result<()> {
        const SET_METHOD_ID: &str = "0";

        let account_id = &self.session.primary_accounts.mail;
        let mut response = self.request(jmap::Request {
            using: &[jmap::CapabilityKind::Mail],
            method_calls: &[jmap::RequestInvocation {
                call: jmap::MethodCall::EmailSet {
                    set: jmap::MethodCallSet {
                        account_id,
                        if_in_state: None,
                        create: None,
                        update: None,
                        destroy: Some(&[id]),
                    },
                },
                id: SET_METHOD_ID,
            }],
            created_ids: None,
        })?;
        self.update_session_state(&response.session_state)?;

        if response.method_responses.len() != 1 {
            return Err(Error::UnexpectedResponse);
        }

        let set_response = expect_email_set(SET_METHOD_ID, response.method_responses.remove(0))?;
        map_first_method_error_into_result(set_response.not_destroyed)
            .context(DestroyEmailSnafu {})?;

        Ok(())
    }

    fn upload_blob(&self, body: &str) -> Result<jmap::BlobUploadResponse> {
        let uri = UriTemplate::new(self.session.upload_url.as_str())
            .set("accountId", self.session.primary_accounts.mail.0.as_str())
            .build();

        self.http_wrapper.post_string(&uri, body)
    }

    fn request<'a>(&self, request: jmap::Request<'a>) -> Result<jmap::Response> {
        self.http_wrapper.post_json(&self.session.api_url, request)
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

/// Contains processed mailbox data.
#[derive(Debug)]
pub struct Mailboxes {
    /// The ID of the archive mailbox. Any mail which does not belong to at least one other mailbox
    /// is instead assigned to this mailbox.
    pub archive_id: Id,
    /// A map of IDs to their corresponding mailboxes.
    pub mailboxes_by_id: HashMap<Id, Mailbox>,
    /// A map of tags to their corresponding mailboxes.
    pub ids_by_tag: HashMap<String, Id>,
    /// A list of IDs of mailboxes to ignore. "Ignore" here means that we will not add or remove
    /// messages from these mailboxes, nor will we assign them to any notmuch tags.
    pub ignored_ids: HashSet<Id>,

    /// An enumeration of what mailbox roles this JMAP server supports.
    pub roles: AvailableMailboxRoles,
}

/// Enumerates the special mailboxes that are available for this particular server.
#[derive(Debug, Default)]
pub struct AvailableMailboxRoles {
    pub deleted: Option<Id>,
    pub draft: Option<Id>,
    pub flagged: Option<Id>,
    pub important: Option<Id>,
    pub spam: Option<Id>,
    pub sent: Option<Id>,
}

/// An object which contains only the properties of a remote Email that mujmap cares about.
#[derive(Debug)]
pub struct Email {
    pub id: Id,
    pub blob_id: Id,
    pub keywords: HashSet<jmap::EmailKeyword>,
    pub mailbox_ids: HashSet<Id>,
    pub tags: HashSet<String>,
}

#[derive(Debug)]
pub struct Mailbox {
    pub id: Id,
    pub tag: String,
}

impl Mailbox {
    fn new(id: Id, tag: String) -> Self {
        Mailbox { id, tag }
    }
}

impl Email {
    fn from_jmap_email(
        jmap_email: jmap::Email,
        mailboxes: &Mailboxes,
        tags_config: &config::Tags,
    ) -> Self {
        let keywords: HashSet<jmap::EmailKeyword> = jmap_email
            .keywords
            .into_iter()
            .filter(|(k, v)| *v && *k != jmap::EmailKeyword::Unknown)
            .map(|(k, _)| k)
            .collect();
        let mailbox_ids = jmap_email
            .mailbox_ids
            .into_iter()
            .filter(|(_, v)| *v)
            .map(|(k, _)| k)
            .collect();

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
        for keyword in &keywords {
            if let Some(tag) = match keyword {
                EmailKeyword::Answered => Some("replied"),
                EmailKeyword::Draft => {
                    if mailboxes.roles.draft.is_some() {
                        None
                    } else {
                        Some("draft")
                    }
                }
                EmailKeyword::Flagged => {
                    if mailboxes.roles.flagged.is_some() {
                        None
                    } else {
                        Some("flagged")
                    }
                }
                EmailKeyword::Forwarded => Some("passed"),
                EmailKeyword::Important => {
                    if mailboxes.roles.important.is_some() {
                        None
                    } else {
                        none_if_empty(&tags_config.important)
                    }
                }
                EmailKeyword::Phishing => none_if_empty(&tags_config.phishing),
                _ => None,
            } {
                tags.insert(tag.to_string());
            }
        }
        if !keywords.contains(&EmailKeyword::Seen) {
            tags.insert("unread".to_string());
        }
        if mailboxes.roles.spam.is_none()
            && !tags_config.spam.is_empty()
            && keywords.contains(&EmailKeyword::Junk)
            && !keywords.contains(&EmailKeyword::NotJunk)
        {
            tags.insert(tags_config.spam.clone());
        }

        Self {
            id: jmap_email.id,
            blob_id: jmap_email.blob_id,
            keywords,
            mailbox_ids,
            tags,
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

fn expect_email_set(
    id: &str,
    invocation: jmap::ResponseInvocation,
) -> Result<jmap::MethodResponseSet<jmap::EmptySetUpdated>> {
    if invocation.id != id {
        return Err(Error::UnexpectedResponse);
    }
    match invocation.call {
        jmap::MethodResponse::EmailSet(set) => Ok(set),
        jmap::MethodResponse::Error(error) => Err(Error::MethodError { error }),
        _ => Err(Error::UnexpectedResponse),
    }
}

fn expect_email_import(
    id: &str,
    invocation: jmap::ResponseInvocation,
) -> Result<jmap::MethodResponseEmailImport> {
    if invocation.id != id {
        return Err(Error::UnexpectedResponse);
    }
    match invocation.call {
        jmap::MethodResponse::EmailImport(import) => Ok(import),
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

fn expect_mailbox_set(
    id: &str,
    invocation: jmap::ResponseInvocation,
) -> Result<jmap::MethodResponseSet<jmap::GenericObjectWithId>> {
    if invocation.id != id {
        return Err(Error::UnexpectedResponse);
    }
    match invocation.call {
        jmap::MethodResponse::MailboxSet(set) => Ok(set),
        jmap::MethodResponse::Error(error) => Err(Error::MethodError { error }),
        _ => Err(Error::UnexpectedResponse),
    }
}

fn expect_email_submission_set(
    id: &str,
    invocation: jmap::ResponseInvocation,
) -> Result<jmap::MethodResponseSet<jmap::GenericObjectWithId>> {
    if invocation.id != id {
        return Err(Error::UnexpectedResponse);
    }
    match invocation.call {
        jmap::MethodResponse::EmailSubmissionSet(set) => Ok(set),
        jmap::MethodResponse::Error(error) => Err(Error::MethodError { error }),
        _ => Err(Error::UnexpectedResponse),
    }
}

fn expect_identity_get(
    id: &str,
    invocation: jmap::ResponseInvocation,
) -> Result<jmap::MethodResponseGetIdentity> {
    if invocation.id != id {
        return Err(Error::UnexpectedResponse);
    }
    match invocation.call {
        jmap::MethodResponse::IdentityGet(get) => Ok(get),
        jmap::MethodResponse::Error(error) => Err(Error::MethodError { error }),
        _ => Err(Error::UnexpectedResponse),
    }
}

fn map_first_method_error_into_result(
    errors: Option<HashMap<Id, jmap::MethodResponseError>>,
) -> Result<(), jmap::MethodResponseError> {
    errors
        .and_then(|map| map.into_iter().next())
        .map_or(Ok(()), |(_, e)| Err(e))
}
