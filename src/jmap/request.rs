use super::{Id, State};
use serde::{ser::SerializeSeq, Serialize, Serializer};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Serialize)]
pub enum CapabilityKind {
    #[serde(rename = "urn:ietf:params:jmap:mail")]
    Mail,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
    /// The set of capabilities the client wishes to use. The client MAY include capability
    /// identifiers even if the method calls it makes do not utilise those capabilities.
    pub using: &'a [CapabilityKind],
    /// An array of method calls to process on the server. The method calls MUST be processed
    /// sequentially, in order.
    pub method_calls: &'a [RequestInvocation<'a>],
    /// A map of a (client-specified) creation id to the id the server assigned when a record was
    /// successfully created.
    ///
    /// As described later in this specification, some records may have a property that contains the
    /// id of another record. To allow more efficient network usage, you can set this property to
    /// reference a record created earlier in the same API request. Since the real id is unknown
    /// when the request is created, the client can instead specify the creation id it assigned,
    /// prefixed with a #.
    ///
    /// As the server processes API requests, any time it successfully creates a new record, it adds
    /// the creation id to this map with the server-assigned real id as the value. If it comes
    /// across a reference to a creation id in a create/update, it looks it up in the map and
    /// replaces the reference with the real id, if found.
    ///
    /// The client can pass an initial value for this map as the `created_ids` property of the
    /// `Request` object. This may be an empty object. If given in the request, the response will
    /// also include a `created_ids` property.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_ids: Option<HashMap<String, String>>,
}

pub struct RequestInvocation<'a> {
    pub call: MethodCall<'a>,
    /// An arbitrary string from the client to be echoed back with the responses emitted by that
    /// method call (a method may return 1 or more responses, as it may make implicit calls to other
    /// methods; all responses initiated by this method call get the same method call id in the
    /// response).
    pub id: &'a str,
}

impl<'a> Serialize for RequestInvocation<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(3))?;

        match self.call {
            MethodCall::EmailGet { .. } => {
                seq.serialize_element("Email/get")?;
            }
            MethodCall::EmailQuery { .. } => {
                seq.serialize_element("Email/query")?;
            }
            MethodCall::EmailChanges { .. } => {
                seq.serialize_element("Email/changes")?;
            }
            MethodCall::EmailSet { .. } => {
                seq.serialize_element("Email/set")?;
            }
            MethodCall::MailboxGet { .. } => {
                seq.serialize_element("Mailbox/get")?;
            }
            MethodCall::MailboxSet { .. } => {
                seq.serialize_element("Mailbox/set")?;
            }
        }

        seq.serialize_element(&self.call)?;
        seq.serialize_element(self.id)?;
        seq.end()
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum MethodCall<'a> {
    #[serde(rename_all = "camelCase")]
    EmailGet {
        #[serde(flatten)]
        get: MethodCallGet<'a>,
    },

    #[serde(rename_all = "camelCase")]
    EmailQuery {
        #[serde(flatten)]
        query: MethodCallQuery<'a>,
    },

    #[serde(rename_all = "camelCase")]
    EmailChanges {
        #[serde(flatten)]
        changes: MethodCallChanges<'a>,
    },

    #[serde(rename_all = "camelCase")]
    EmailSet {
        #[serde(flatten)]
        set: MethodCallSet<'a, EmptyCreate>,
    },

    #[serde(rename_all = "camelCase")]
    MailboxGet {
        #[serde(flatten)]
        get: MethodCallGet<'a>,
    },

    #[serde(rename_all = "camelCase")]
    MailboxSet {
        #[serde(flatten)]
        set: MethodCallSet<'a, MailboxCreate>,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodCallGet<'a> {
    /// The id of the account to use.
    pub account_id: &'a Id,
    /// The ids of the Foo objects to return. If `None`, then all records of the data type are
    /// returned, if this is supported for that data type and the number of records does not exceed
    /// the `max_objects_in_get` limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ids: Option<&'a [&'a Id]>,
    /// If supplied, only the properties listed in the array are returned for each Foo object. If
    /// `None`, all properties of the object are returned. The id property of the object is always
    /// returned, even if not explicitly requested. If an invalid property is requested, the call
    /// MUST be rejected with a `ResponseError::InvalidArguments` error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<&'a [&'a str]>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodCallQuery<'a> {
    /// The id of the account to use.
    pub account_id: &'a Id,
    /// The zero-based index of the first id in the full list of results to return.
    ///
    /// If a negative value is given, it is an offset from the end of the list. Specifically, the
    /// negative value MUST be added to the total number of results given the filter, and if still
    /// negative, it’s clamped to 0. This is now the zero-based index of the first id to return.
    ///
    /// If the index is greater than or equal to the total number of objects in the results list,
    /// then the ids array in the response will be empty, but this is not an error.
    #[serde(default, skip_serializing_if = "default")]
    pub position: i64,
    /// A `Foo` id. If supplied, the position argument is ignored. The index of this id in the
    /// results will be used in combination with the `anchor_offset` argument to determine the index
    /// of the first result to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<&'a Id>,
    /// The index of the first result to return relative to the index of the anchor, if an anchor is
    /// given. This MAY be negative. For example, -1 means the Foo immediately preceding the anchor
    /// is the first result in the list returned.
    #[serde(default, skip_serializing_if = "default")]
    pub anchor_offset: i64,
    /// The maximum number of results to return. If `None`, no limit presumed. The server MAY choose
    /// to enforce a maximum limit argument. In this case, if a greater value is given (or if it is
    /// `None`), the limit is clamped to the maximum; the new limit is returned with the response so
    /// the client is aware. If a negative value is given, the call MUST be rejected with a
    /// `jmap::ResponseError::InvalidArguments` error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// Does the client wish to know the total number of results in the query? This may be slow and
    /// expensive for servers to calculate, particularly with complex filters, so clients should
    /// take care to only request the total when needed.
    #[serde(default, skip_serializing_if = "default")]
    pub calculate_total: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodCallChanges<'a> {
    /// The id of the account to use.
    pub account_id: &'a Id,
    /// The current state of the client. This is the string that was returned as the state argument
    /// in the Foo/get response. The server will return the changes that have occurred since this
    /// state.
    pub since_state: &'a State,
    /// The maximum number of ids to return in the response. The server MAY choose to return fewer
    /// than this value but MUST NOT return more. If not given by the client, the server may choose
    /// how many to return. If supplied by the client, the value MUST be a positive integer greater
    /// than 0. If a value outside of this range is given, the server MUST reject the call with an
    /// invalidArguments error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_changes: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodCallSet<'a, C> {
    /// The id of the account to use.
    pub account_id: &'a Id,
    /// This is a state string as returned by the `Foo/get` method (representing the state of all
    /// objects of this type in the account). If supplied, the string must match the current state;
    /// otherwise, the method will be aborted and a stateMismatch error returned. If `None`, any
    /// changes will be applied to the current state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub if_in_state: Option<&'a Id>,
    /// A map of a creation id (a temporary id set by the client) to `Foo` objects, or `None` if no
    /// objects are to be created.
    ///
    /// The Foo object type definition may define default values for properties. Any such property
    /// may be omitted by the client.
    ///
    /// The client MUST omit any properties that may only be set by the server (for example, the id
    /// property on most object types).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create: Option<HashMap<&'a Id, &'a C>>,
    /// A map of an id to a Patch object to apply to the current `Foo` object with that id, or
    /// `None` if no objects are to be updated.
    ///
    /// A `PatchObject` is of type `String[*]` and represents an unordered set of patches. The keys
    /// are a path in JSON Pointer Format
    /// \[[RFC6901](https://datatracker.ietf.org/doc/html/rfc6901)\], with an implicit leading “/”
    /// (i.e., prefix each key with “/” before applying the JSON Pointer evaluation algorithm).
    ///
    /// All paths MUST also conform to the following restrictions; if there is any violation, the
    /// update MUST be rejected with an invalidPatch error:
    ///
    /// * The pointer MUST NOT reference inside an array (i.e., you MUST NOT insert/delete from an
    /// array; the array MUST be replaced in its entirety instead). * All parts prior to the last
    /// (i.e., the value after the final slash) MUST already exist on the object being patched. *
    /// There MUST NOT be two patches in the PatchObject where the pointer of one is the prefix of
    /// the pointer of the other, e.g., “alerts/1/offset” and “alerts”.
    ///
    /// The value associated with each pointer determines how to apply that patch:
    ///
    /// * If `None`, set to the default value if specified for this property; otherwise, remove the
    /// property from the patched object. If the key is not present in the parent, this a no-op. *
    /// Anything else: The value to set for this property (this may be a replacement or addition to
    /// the object being patched).
    ///
    /// Any server-set properties MAY be included in the patch if their value is identical to the
    /// current server value (before applying the patches to the object). Otherwise, the update MUST
    /// be rejected with an invalidProperties SetError.
    ///
    /// This patch definition is designed such that an entire `Foo` object is also a valid
    /// `PatchObject`. The client may choose to optimise network usage by just sending the diff or
    /// may send the whole object; the server processes it the same either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update: Option<HashMap<&'a Id, HashMap<&'a str, Value>>>,
    /// A list of ids for `Foo` objects to permanently delete, or `None` if no objects are to be
    /// destroyed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destroy: Option<Vec<&'a Id>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmptyCreate;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxCreate {
    /// The Mailbox id for the parent of this `Mailbox`, or `None` if this Mailbox is at the top
    /// level. Mailboxes form acyclic graphs (forests) directed by the child-to-parent relationship.
    /// There MUST NOT be a loop.
    pub parent_id: Option<Id>,
    /// User-visible name for the Mailbox, e.g., “Inbox”. This MUST be a Net-Unicode string
    /// \[[RFC5198](https://datatracker.ietf.org/doc/html/rfc5198)\] of at least 1 character in
    /// length, subject to the maximum size given in the capability object. There MUST NOT be two
    /// sibling Mailboxes with both the same parent and the same name. Servers MAY reject names that
    /// violate server policy (e.g., names containing a slash (/) or control characters).
    pub name: String,
}

fn default<T: Default + PartialEq>(t: &T) -> bool {
    *t == Default::default()
}
