use serde::Deserialize;
use std::collections::HashMap;

use super::{Id, State};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    /// An object specifying the capabilities of this server.
    pub capabilities: Capabilities,
    /// A map of an account id to an `Account` object for each account the user
    /// has access to.
    pub accounts: HashMap<Id, Account>,
    /// A map of capabilities to the account id that is considered to be the
    /// user’s main or default account for data pertaining to that capability.
    pub primary_accounts: PrimaryAccounts,
    /// The username associated with the given credentials, or the empty string
    /// if none.
    pub username: String,
    /// The URL to use for JMAP API requests.
    pub api_url: String,
    /// The URL endpoint to use when downloading files, in URI Template (level
    /// 1) format [@!RFC6570]. The URL MUST contain variables called accountId,
    /// blobId, type, and name.
    pub download_url: String,
    /// The URL endpoint to use when uploading files, in URI Template (level 1)
    /// format [@!RFC6570]. The URL MUST contain a variable called accountId.
    pub upload_url: String,
    /// The URL to connect to for push events in URI Template (level 1) format
    /// [@!RFC6570]. The URL MUST contain variables called types, closeafter,
    /// and ping.
    pub event_source_url: String,
    /// A string representing the state of this object on the server. If the
    /// value of any other property on the Session object changes, this string
    /// will change. The current value is also returned on the API Response
    /// object, allowing clients to quickly determine if the session information
    /// has changed (e.g., an account has been added or removed), so they need
    /// to refetch the object.
    pub state: State,
}

#[derive(Debug, Deserialize)]
pub struct PrimaryAccounts {
    #[serde(rename = "urn:ietf:params:jmap:core")]
    pub core: Id,
    #[serde(rename = "urn:ietf:params:jmap:mail")]
    pub mail: Id,
}

#[derive(Debug, Deserialize)]
pub struct Capabilities {
    #[serde(rename = "urn:ietf:params:jmap:core")]
    pub core: CoreCapabilities,
    #[serde(rename = "urn:ietf:params:jmap:mail")]
    pub mail: EmptyCapabilities,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreCapabilities {
    /// The maximum file size, in octets, that the server will accept for a
    /// single file upload (for any purpose).
    pub max_size_upload: u64,
    /// The maximum number of concurrent requests the server will accept to the
    /// upload endpoint.
    pub max_concurrent_upload: u64,
    /// The maximum size, in octets, that the server will accept for a single
    /// request to the API endpoint.
    pub max_size_request: u64,
    /// The maximum number of concurrent requests the server will accept to the
    /// API endpoint.
    pub max_concurrent_requests: u64,
    /// The maximum number of method calls the server will accept in a single
    /// request to the API endpoint.
    pub max_calls_in_request: u64,
    /// The maximum number of objects that the client may request in a single
    /// /get type method call.
    pub max_objects_in_get: u64,
    /// The maximum number of objects the client may send to create, update, or
    /// destroy in a single /set type method call. This is the combined total,
    /// e.g., if the maximum is 10, you could not create 7 objects and destroy
    /// 6, as this would be 13 actions, which exceeds the limit.
    pub max_objects_in_set: u64,
    /// A list of identifiers for algorithms registered in the collation
    /// registry, as defined in [@!RFC4790], that the server supports for
    /// sorting when querying records.
    pub collation_algorithms: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct EmptyCapabilities {}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    /// A user-friendly string to show when presenting content from this
    /// account, e.g., the email address representing the owner of the account.
    pub name: String,
    /// This is `true` if the account belongs to the authenticated user rather
    /// than a group account or a personal account of another user that has been
    /// shared with them.
    pub is_personal: bool,
    /// This is `true` if the entire account is read-only.
    pub is_read_only: bool,
    /// The set of capabilities for the methods supported in this account. Each
    /// key is capability that has methods you can use with this account. The
    /// value for each of these keys is an object with further information about
    /// the account’s permissions and restrictions with respect to this
    /// capability, as defined in the capability’s specification.
    pub account_capabilities: AccountCapabilities,
}

#[derive(Debug, Deserialize)]
pub struct AccountCapabilities {
    #[serde(rename = "urn:ietf:params:jmap:core")]
    pub core: EmptyCapabilities,
    #[serde(rename = "urn:ietf:params:jmap:mail")]
    pub mail: MailAccountCapabilities,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailAccountCapabilities {
    /// The maximum number of Mailboxes that can be can assigned to a single
    /// Email object. This MUST be an integer >= 1, or `None` for no limit (or
    /// rather, the limit is always the number of Mailboxes in the account).
    pub max_mailboxes_per_email: Option<u64>,
    /// The maximum depth of the Mailbox hierarchy (i.e., one more than the
    /// maximum number of ancestors a Mailbox may have), or `None` for no limit.
    pub max_mailbox_depth: Option<u64>,
    /// The maximum length, in (UTF-8) octets, allowed for the name of a
    /// Mailbox. This MUST be at least 100, although it is recommended servers
    /// allow more.
    pub max_size_mailbox_name: u64,
    /// The maximum total size of attachments, in octets, allowed for a single
    /// Email object. A server MAY still reject the import or creation of an
    /// Email with a lower attachment size total (for example, if the body
    /// includes several megabytes of text, causing the size of the encoded MIME
    /// structure to be over some server-defined limit).
    ///
    /// Note that this limit is for the sum of unencoded attachment sizes. Users
    /// are generally not knowledgeable about encoding overhead, etc., nor
    /// should they need to be, so marketing and help materials normally tell
    /// them the “max size attachments”. This is the unencoded size they see on
    /// their hard drive, so this capability matches that and allows the client
    /// to consistently enforce what the user understands as the limit.
    ///
    /// The server may separately have a limit for the total size of the message
    /// [@!RFC5322], created by combining the attachments (often base64 encoded)
    /// with the message headers and bodies. For example, suppose the server
    /// advertises `max_size_attachments_per_email`: 50000000 (50 MB). The
    /// enforced server limit may be for a message size of 70000000 octets. Even
    /// with base64 encoding and a 2 MB HTML body, 50 MB attachments would fit
    /// under this limit.
    pub max_size_attachments_per_email: u64,
    /// A list of all the values the server supports for the “property” field of
    /// the Comparator object in an Email/query sort. This MAY include
    /// properties the client does not recognise (for example, custom properties
    /// specified in a vendor extension). Clients MUST ignore any unknown
    /// properties in the list.
    pub email_query_sort_options: Vec<String>,
    /// If true, the user may create a Mailbox in this account with a `None`
    /// parentId. (Permission for creating a child of an existing Mailbox is
    /// given by the myRights property on that Mailbox.)
    pub may_create_top_level_mailbox: bool,
}
