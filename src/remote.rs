use std::{
    collections::{HashMap, HashSet},
    io::{self, Read},
    time::Duration,
};

use crate::{
    config::{self, Config},
    jmap::{self, Id, MailboxRole, State, EmailKeyword},
    local,
};
use itertools::Itertools;
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
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

struct HttpWrapper {
    /// Value of HTTP Authorization header.
    authorization: String,
    /// Persistent ureq agent to use for all HTTP requests.
    agent: ureq::Agent,
}

impl HttpWrapper {
    fn new(username: &str, password: &str, timeout: u64) -> Self {
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
        Ok(self
            .agent
            .get(url)
            .set("Authorization", &self.authorization)
            .call()
            .context(ReadEmailBlobSnafu {})?
            .into_reader()
            // Limiting download size as advised by ureq's documentation:
            // https://docs.rs/ureq/latest/ureq/struct.Response.html#method.into_reader
            .take(10_000_000))
    }

    fn post<S: Serialize, D: DeserializeOwned>(&self, url: &str, body: S) -> Result<D> {
        let post = self
            .agent
            .post(url)
            .set("Authorization", &self.authorization)
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
        match &config.fqdn {
            Some(fqdn) => {
                Self::open_host(&fqdn, config.username.as_str(), &password, config.timeout)
            }
            None => Remote::open_url(
                &config.session_url.as_ref().unwrap(),
                config.username.as_str(),
                &password,
                config.timeout,
            ),
        }
    }

    pub fn open_host(fqdn: &str, username: &str, password: &str, timeout: u64) -> Result<Self> {
        let resolver = Resolver::from_system_conf().context(ParseResolvConfSnafu {})?;
        let mut address = format!("_jmap._tcp.{}", fqdn);
        if !address.ends_with(".") {
            address.push('.');
        }
        let resolver_response = resolver
            .srv_lookup(address.as_str())
            .context(SrvLookupSnafu { address })?;

        let http_wrapper = HttpWrapper::new(username, password, timeout);

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
    ) -> Result<Self> {
        let http_wrapper = HttpWrapper::new(username, password, timeout);
        let (session_url, session) = http_wrapper
            .get_session(session_url)
            .context(OpenSessionSnafu { session_url })?;
        Ok(Remote {
            http_wrapper,
            session_url,
            session,
        })
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
                emails.insert(email.id.clone(), Email::from_jmap_email(email, mailboxes, tags_config));
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
                    MailboxRole::Drafts => roles.draft = true,
                    MailboxRole::Flagged => roles.flagged = true,
                    MailboxRole::Important => roles.important = true,
                    MailboxRole::Junk => roles.spam = true,
                    MailboxRole::Trash => roles.deleted = true,
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
                patch.insert("keywords/$draft", as_value(local_email.tags.contains("draft")));
                patch.insert("keywords/$seen", as_value(!local_email.tags.contains("unread")));
                patch.insert("keywords/$flagged", as_value(local_email.tags.contains("flagged")));
                patch.insert("keywords/$answered", as_value(local_email.tags.contains("replied")));
                patch.insert("keywords/$forwarded", as_value(local_email.tags.contains("passed")));
                if !mailboxes.roles.spam && !tags_config.spam.is_empty() {
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
    pub deleted: bool,
    pub draft: bool,
    pub flagged: bool,
    pub important: bool,
    pub spam: bool,
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
                tags.insert(tag.to_string());
            }
        }
        if !keywords.contains(&EmailKeyword::Seen) {
            tags.insert("unread".to_string());
        }
        if !mailboxes.roles.spam
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
