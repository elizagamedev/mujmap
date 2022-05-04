use serde::{
    de::{Error, SeqAccess, Visitor},
    Deserialize, Deserializer,
};
use std::{collections::HashMap, fmt};

use super::{Id, State};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    /// An array of responses. The output of the methods MUST be added to the
    /// `method_responses` array in the same order that the methods are
    /// processed.
    pub method_responses: Vec<ResponseInvocation>,
    /// (optional; only returned if given in the request) A map of a
    /// (client-specified) creation id to the id the server assigned when a
    /// record was successfully created. This MUST include all creation ids
    /// passed in the original createdIds parameter of the Request object, as
    /// well as any additional ones added for newly created records.
    pub created_ids: Option<HashMap<String, Id>>,
    /// The current value of the “state” string on the `Session` object. Clients
    /// may use this to detect if this object has changed and needs to be
    /// refetched.
    pub session_state: State,
}

#[derive(Debug)]
pub struct ResponseInvocation {
    pub call: MethodResponse,
    /// An arbitrary string from the client to be echoed back with the responses
    /// emitted by that method call (a method may return 1 or more responses, as
    /// it may make implicit calls to other methods; all responses initiated by
    /// this method call get the same method call id in the response).
    pub id: String,
}

impl<'de> Deserialize<'de> for ResponseInvocation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct MethodResponseVisitor;

        impl<'de> Visitor<'de> for MethodResponseVisitor {
            type Value = ResponseInvocation;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a sequence of [string, map, string]")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let name: String = seq.next_element()?.ok_or(Error::invalid_length(0, &"3"))?;

                let length_err = Error::invalid_length(1, &"3");
                let call = match name.as_str() {
                    "Email/get" => Ok(MethodResponse::EmailGet(
                        seq.next_element::<MethodResponseGet<Email>>()?
                            .ok_or(length_err)?,
                    )),
                    "Email/query" => Ok(MethodResponse::EmailQuery(
                        seq.next_element::<MethodResponseQuery>()?
                            .ok_or(length_err)?,
                    )),
                    "Email/changes" => Ok(MethodResponse::EmailChanges(
                        seq.next_element::<MethodResponseChanges>()?
                            .ok_or(length_err)?,
                    )),
                    "Mailbox/get" => Ok(MethodResponse::MailboxGet(
                        seq.next_element::<MethodResponseGet<Mailbox>>()?
                            .ok_or(length_err)?,
                    )),
                    "error" => Ok(MethodResponse::Error(
                        seq.next_element::<MethodResponseError>()?
                            .ok_or(length_err)?,
                    )),
                    _ => Err(Error::unknown_field(
                        name.as_str(),
                        &[
                            "Email/get",
                            "Email/query",
                            "Email/changes",
                            "Mailbox/get",
                            "error",
                        ],
                    )),
                }?;

                let id: String = seq.next_element()?.ok_or(Error::invalid_length(2, &"3"))?;

                Ok(ResponseInvocation { call, id })
            }
        }
        deserializer.deserialize_seq(MethodResponseVisitor)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodResponseGet<T> {
    /// The id of the account used for the call.
    pub account_id: Id,
    /// A (preferably short) string representing the state on the server for all
    /// the data of this type in the account (not just the objects returned in
    /// this call). If the data changes, this string MUST change. If the Foo
    /// data is unchanged, servers SHOULD return the same state string on
    /// subsequent requests for this data type.
    ///
    /// When a client receives a response with a different state string to a
    /// previous call, it MUST either throw away all currently cached objects
    /// for the type or call Foo/changes to get the exact changes.
    pub state: State,
    /// An array of the Foo objects requested. This is the empty array if no
    /// objects were found or if the ids argument passed in was also an empty
    /// array. The results MAY be in a different order to the ids in the request
    /// arguments. If an identical id is included more than once in the request,
    /// the server MUST only include it once in either the list or the notFound
    /// argument of the response.
    pub list: Vec<T>,
    /// This array contains the ids passed to the method for records that do not
    /// exist. The array is empty if all requested ids were found or if the ids
    /// argument passed in was either null or an empty array.
    pub not_found: Vec<Id>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodResponseQuery {
    /// The id of the account used for the call.
    pub account_id: Id,
    /// A string encoding the current state of the query on the server. This
    /// string MUST change if the results of the query (i.e., the matching ids
    /// and their sort order) have changed. The `query_state` string MAY change
    /// if something has changed on the server, which means the results may have
    /// changed but the server doesn’t know for sure.
    ///
    /// The `query_state` string only represents the ordered list of ids that
    /// match the particular query (including its sort/filter). There is no
    /// requirement for it to change if a property on an object matching the
    /// query changes but the query results are unaffected (indeed, it is more
    /// efficient if the `query_state` string does not change in this case). The
    /// queryState string only has meaning when compared to future responses to
    /// a query with the same type/sort/filter or when used with /queryChanges
    /// to fetch changes.
    ///
    /// Should a client receive back a response with a different `query_state`
    /// string to a previous call, it MUST either throw away the currently
    /// cached query and fetch it again (note, this does not require fetching
    /// the records again, just the list of ids) or call `Foo/queryChanges` to
    /// get the difference.
    pub query_state: State,
    /// This is true if the server supports calling Foo/queryChanges with these
    /// filter/sort parameters. Note, this does not guarantee that the
    /// Foo/queryChanges call will succeed, as it may only be possible for a
    /// limited time afterwards due to server internal implementation details.
    pub can_calculate_changes: bool,
    /// The zero-based index of the first result in the ids array within the
    /// complete list of query results.
    pub position: u64,
    /// The list of ids for each Foo in the query results, starting at the index
    /// given by the position argument of this response and continuing until it
    /// hits the end of the results or reaches the limit number of ids. If
    /// position is >= total, this MUST be the empty list.
    pub ids: Vec<Id>,
    /// (only if requested) The total number of Foos in the results (given the
    /// filter). This argument MUST be omitted if the `calculate_total` request
    /// argument is not true.
    pub total: Option<u64>,
    /// The limit enforced by the server on the maximum number of results to
    /// return. This is only returned if the server set a limit or used a
    /// different limit than that given in the request.
    pub limit: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodResponseChanges {
    /// The id of the account used for the call.
    pub account_id: Id,
    /// This is the sinceState argument echoed back; it’s the state from which
    /// the server is returning changes.
    pub old_state: State,
    /// This is the state the client will be in after applying the set of
    /// changes to the old state.
    pub new_state: State,
    /// If true, the client may call Foo/changes again with the newState
    /// returned to get further updates. If false, newState is the current
    /// server state.
    pub has_more_changes: bool,
    /// An array of ids for records that have been created since the old state.
    pub created: Vec<Id>,
    /// An array of ids for records that have been updated since the old state.
    pub updated: Vec<Id>,
    /// An array of ids for records that have been destroyed since the old
    /// state.
    pub destroyed: Vec<Id>,
}

/// If a method encounters an error, the appropriate error response MUST be
/// inserted at the current point in the methodResponses array and, unless
/// otherwise specified, further processing MUST NOT happen within that
/// method call.
///
/// Any further method calls in the request MUST then be processed as
/// normal. Errors at the method level MUST NOT generate an HTTP-level
/// error.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodResponseError {
    #[serde(rename = "type")]
    pub kind: MethodResponseErrorKind,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Email {
    pub id: Id,
    pub blob_id: Id,
    pub keywords: HashMap<EmailKeyword, bool>,
    pub mailbox_ids: HashMap<Id, bool>,
}

#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug, Deserialize)]
pub enum EmailKeyword {
    #[serde(rename = "$draft")]
    Draft,
    #[serde(rename = "$seen")]
    Seen,
    #[serde(rename = "$flagged")]
    Flagged,
    #[serde(rename = "$answered")]
    Answered,
    #[serde(rename = "$Forwarded")]
    Forwarded,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mailbox {
    /// The id of the Mailbox.
    pub id: Id,
    /// The Mailbox id for the parent of this Mailbox, or null if this Mailbox
    /// is at the top level. Mailboxes form acyclic graphs (forests) directed by
    /// the child-to-parent relationship. There MUST NOT be a loop.
    pub parent_id: Option<Id>,
    /// User-visible name for the Mailbox, e.g., “Inbox”. This MUST be a
    /// Net-Unicode string [@!RFC5198] of at least 1 character in length,
    /// subject to the maximum size given in the capability object. There MUST
    /// NOT be two sibling Mailboxes with both the same parent and the same
    /// name. Servers MAY reject names that violate server policy (e.g., names
    /// containing a slash (/) or control characters).
    pub name: String,
    /// Identifies Mailboxes that have a particular common purpose (e.g., the
    /// “inbox”), regardless of the name property (which may be localised).
    pub role: Option<MailboxRole>,
}

/// https://www.iana.org/assignments/imap-mailbox-name-attributes/imap-mailbox-name-attributes.xhtml
#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MailboxRole {
    /// All messages.
    All,
    /// Archived messages.
    Archive,
    /// Messages that are working drafts.
    Drafts,
    /// Messages with the \Flagged flag.
    Flagged,
    /// Messages deemed important to user.
    Important,
    /// Messages New mail is delivered here by default.
    Inbox,
    /// Messages identified as Spam/Junk.
    Junk,
    /// Sent mail.
    Sent,
    /// The mailbox is subscribed to.
    Subscribed,
    /// Messages the user has discarded.
    Trash,
    /// As-of-yet defined roles.
    #[serde(other)]
    Unknown,
}

impl MailboxRole {
    pub fn as_str(&self) -> &'static str {
        match *self {
            MailboxRole::All => "all",
            MailboxRole::Archive => "archive",
            MailboxRole::Drafts => "draft",
            MailboxRole::Flagged => "flagged",
            MailboxRole::Important => "important",
            MailboxRole::Inbox => "inbox",
            MailboxRole::Junk => "spam",
            MailboxRole::Sent => "sent",
            MailboxRole::Subscribed => "subscribed",
            MailboxRole::Trash => "deleted",
            MailboxRole::Unknown => "YOU_SHOULD_NOT_SEE_THIS",
        }
    }
}

#[derive(Debug)]
pub enum MethodResponse {
    EmailGet(MethodResponseGet<Email>),
    EmailQuery(MethodResponseQuery),
    EmailChanges(MethodResponseChanges),

    MailboxGet(MethodResponseGet<Mailbox>),

    Error(MethodResponseError),
}

#[derive(Debug, Deserialize, Copy, Clone)]
#[serde(rename_all = "camelCase")]
pub enum MethodResponseErrorKind {
    /// The accountId does not correspond to a valid account.
    AccountNotFound,
    /// The accountId given corresponds to a valid account, but the account does
    /// not support this method or data type.
    AccountNotSupportedByMethod,
    /// This method modifies state, but the account is read-only (as returned on
    /// the corresponding Account in the Session object).
    AccountReadOnly,
    /// An anchor argument was supplied, but it cannot be found in the results
    /// of the query.
    AnchorNotFound,
    /// The server forbids duplicates, and the record already exists in the
    /// target account. An existingId property of type Id MUST be included on
    /// the SetError object with the id of the existing record.
    AlreadyExists,
    /// The server cannot calculate the changes from the state string given by
    /// the client.
    CannotCalculateChanges,
    /// The action would violate an ACL or other permissions policy.
    Forbidden,
    /// The fromAccountId does not correspond to a valid account.
    FromAccountNotFound,
    /// The fromAccountId given corresponds to a valid account, but the account
    /// does not support this data type.
    FromAccountNotSupportedByMethod,
    /// One of the arguments is of the wrong type or otherwise invalid, or a
    /// required argument is missing.
    InvalidArguments,
    /// The PatchObject given to update the record was not a valid patch.
    InvalidPatch,
    /// The record given is invalid.
    InvalidProperties,
    /// The id given cannot be found.
    NotFound,
    /// The content type of the request was not application/json or the request
    /// did not parse as I-JSON.
    NotJSON,
    /// The request parsed as JSON but did not match the type signature of the
    /// Request object.
    NotRequest,
    /// The create would exceed a server-defined limit on the number or total
    /// size of objects of this type.
    OverQuota,
    /// Too many objects of this type have been created recently, and a
    /// server-defined rate limit has been reached. It may work if tried again
    /// later.
    RateLimit,
    /// The total number of actions exceeds the maximum number the server is
    /// willing to process in a single method call.
    RequestTooLarge,
    /// The method used a result reference for one of its arguments, but this
    /// failed to resolve.
    InvalidResultReference,
    /// An unexpected or unknown error occurred during the processing of the
    /// call. The method call made no changes to the server’s state.
    ServerFail,
    /// Some, but not all, expected changes described by the method occurred.
    /// The client MUST re-synchronise impacted data to determine server state.
    /// Use of this error is strongly discouraged.
    ServerPartialFail,
    /// Some internal server resource was temporarily unavailable. Attempting
    /// the same operation later (perhaps after a backoff with a random factor)
    /// may succeed.
    ServerUnavailable,
    /// This is a singleton type, so you cannot create another one or destroy
    /// the existing one.
    Singleton,
    /// An ifInState argument was supplied, and it does not match the current
    /// state.
    StateMismatch,
    /// The action would result in an object that exceeds a server-defined limit
    /// for the maximum size of a single object of this type.
    TooLarge,
    /// There are more changes than the client’s maxChanges argument.
    TooManyChanges,
    /// The client included a capability in the “using” property of the request
    /// that the server does not support.
    UnknownCapability,
    /// The server does not recognise this method name.
    UnknownMethod,
    /// The filter is syntactically valid, but the server cannot process it.
    UnsupportedFilter,
    /// The sort is syntactically valid, but includes a property the server does
    /// not support sorting on, or a collation method it does not recognise.
    UnsupportedSort,
    /// The client requested an object be both updated and destroyed in the same
    /// /set request, and the server has decided to therefore ignore the update.
    WillDestroy,
    /// The Mailbox still has at least one child Mailbox. The client MUST remove
    /// these before it can delete the parent Mailbox.
    MailboxHasChild,
    /// The Mailbox has at least one message assigned to it and the
    /// onDestroyRemoveEmails argument was false.
    MailboxHasEmail,
    /// At least one blob id referenced in the object doesn’t exist.
    BlobNotFound,
    /// The change to the Email’s keywords would exceed a server-defined
    /// maximum.
    TooManyKeywords,
    /// The change to the set of Mailboxes that this Email is in would exceed a
    /// server-defined maximum.
    TooManyMailboxes,
    /// The Email to be sent is invalid in some way.
    InvalidEmail,
    /// The envelope [@!RFC5321] (supplied or generated) has more recipients
    /// than the server allows.
    TooManyRecipients,
    /// The envelope [@!RFC5321] (supplied or generated) does not have any
    /// rcptTo email addresses.
    NoRecipients,
    /// The rcptTo property of the envelope [@!RFC5321] (supplied or generated)
    /// contains at least one rcptTo value that is not a valid email address for
    /// sending to.
    InvalidRecipients,
    /// The server does not permit the user to send a message with this envelope
    /// From address [@!RFC5321].
    ForbiddenMailFrom,
    /// The server does not permit the user to send a message with the From
    /// header field [@!RFC5322] of the message to be sent.
    ForbiddenFrom,
    /// The user does not have permission to send at all right now.
    ForbiddenToSend,
}
