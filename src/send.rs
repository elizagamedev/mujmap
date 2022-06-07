use either::Either;
use fqdn::FQDN;
use log::{debug, warn};
use snafu::prelude::*;
use std::{
    collections::HashSet,
    io::{Cursor, Read},
    iter,
    str::FromStr,
    string::FromUtf8Error,
};

use crate::{
    config::Config,
    jmap,
    remote::{self, Remote},
};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Could not read mail from stdin: {}", source))]
    ReadStdin { source: loe::ParseError },

    #[snafu(display("Could not read mail from CRLF stdin buffer: {}", source))]
    ReadCrlfStdin { source: FromUtf8Error },

    #[snafu(display("Could not parse mail: {}", source))]
    ParseEmail { source: email_parser::error::Error },

    #[snafu(display("Could not parse sender domain: {}", source))]
    ParseSenderDomain { domain: String, source: fqdn::Error },

    #[snafu(display("Could not open remote session: {}", source))]
    OpenRemote { source: remote::Error },

    #[snafu(display("Could not enumerate identities: {}", source))]
    GetIdentities { source: remote::Error },

    #[snafu(display("JMAP server has identity with invalid email address `{}'", address))]
    InvalidEmailAddress { address: String },

    #[snafu(display("Could not parse JMAP identity domain `{}': {}", domain, source))]
    ParseIdentityDomain { domain: String, source: fqdn::Error },

    #[snafu(display("No JMAP identities match sender `{}'", sender))]
    NoIdentitiesForSender { sender: String },

    #[snafu(display("Could not index mailboxes: {}", source))]
    IndexMailboxes { source: remote::Error },

    #[snafu(display("No recipients specified. Did you forget to specify `-t'?"))]
    NoRecipients {},

    #[snafu(display("Could not send email: {}", source))]
    SendEmail { source: remote::Error },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub fn send(read_recipients: bool, recipients: Vec<String>, config: Config) -> Result<()> {
    // Read mail from stdin, converting Unix newlines to DOS newlines to coimply with RFC5322.
    // Truncate the input so we don't infinitely grow a buffer if someone pipes /dev/urandom into
    // mujmap or something similar by mistake.
    let mut stdio_crlf = Cursor::new(Vec::new());
    loe::process(
        &mut std::io::stdin().take(10_000_000),
        &mut stdio_crlf,
        loe::Config::default().transform(loe::TransformMode::Crlf),
    )
    .context(ReadStdinSnafu {})?;

    let email_string = String::from_utf8(stdio_crlf.into_inner()).context(ReadCrlfStdinSnafu {})?;
    let parsed_email =
        email_parser::email::Email::parse(email_string.as_bytes()).context(ParseEmailSnafu {})?;

    let mut remote = Remote::open(&config).context(OpenRemoteSnafu {})?;

    let identity_id =
        get_identity_id_for_sender_address(&parsed_email.sender.address, &mut remote)?;
    let mailboxes = remote
        .get_mailboxes(&config.tags)
        .context(IndexMailboxesSnafu {})?;

    let from_address = address_to_string(&parsed_email.sender.address);
    let addresses_to_iter = |a| {
        // Use `as' here as a workaround for lifetime inference.
        (a as Option<Vec<email_parser::address::Address>>).map_or_else(
            || Either::Left(iter::empty()),
            |x| Either::Right(x.into_iter()),
        )
    };
    let to_addresses: HashSet<String> = if read_recipients {
        if !recipients.is_empty() {
            warn!(concat!(
                "Both `-t' and recipients were specified in the same command; ",
                "ignoring recipient arguments"
            ));
        }
        addresses_to_iter(parsed_email.to)
            .chain(addresses_to_iter(parsed_email.cc))
            .chain(addresses_to_iter(parsed_email.bcc))
            .flat_map(|x| match x {
                email_parser::address::Address::Mailbox(mailbox) => {
                    Either::Left(iter::once(address_to_string(&mailbox.address)))
                }
                email_parser::address::Address::Group((_, mailboxes)) => Either::Right(
                    mailboxes
                        .into_iter()
                        .map(|mailbox| address_to_string(&mailbox.address)),
                ),
            })
            .collect()
    } else {
        // TODO: Locally verify that all recipients are valid email addresses.
        recipients.into_iter().collect()
    };

    ensure!(!to_addresses.is_empty(), NoRecipientsSnafu {});

    debug!(
        "Envelope sender is `{}', recipients are `{:?}'",
        from_address, to_addresses
    );

    // Create the email!
    remote
        .send_email(
            identity_id,
            &mailboxes,
            &from_address,
            &to_addresses,
            &email_string,
        )
        .context(SendEmailSnafu {})?;

    Ok(())
}

fn get_identity_id_for_sender_address(
    sender_address: &email_parser::address::EmailAddress,
    remote: &mut Remote,
) -> Result<jmap::Id> {
    let sender_local_part = &sender_address.local_part;
    let sender_domain = &sender_address.domain;
    let sender_fqdn = FQDN::from_str(sender_domain.as_ref()).context(ParseSenderDomainSnafu {
        domain: sender_domain.as_ref(),
    })?;
    debug!(
        "Sender is `{}@{}', fqdn `{}'",
        sender_local_part, sender_domain, sender_fqdn
    );

    // Find the identity which matches the sender of this email.
    let identities = remote.get_identities().context(GetIdentitiesSnafu {})?;
    let sender_identities: Vec<_> = identities
        .iter()
        .map(|identity| {
            let (local_part, domain) =
                identity
                    .email
                    .split_once('@')
                    .context(InvalidEmailAddressSnafu {
                        address: &identity.email,
                    })?;
            let fqdn = FQDN::from_str(domain).context(ParseIdentityDomainSnafu { domain })?;
            Ok((identity, local_part, fqdn))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|(_, local_part, fqdn)| {
            *fqdn == sender_fqdn && (*local_part == "*" || *local_part == sender_local_part)
        })
        .map(|(identity, local_part, _)| (identity, local_part))
        .collect();
    ensure!(
        !sender_identities.is_empty(),
        NoIdentitiesForSenderSnafu {
            sender: address_to_string(&sender_address),
        }
    );
    // Prefer a concrete identity over a wildcard.
    let identity = sender_identities
        .iter()
        .filter(|(_, local_part)| *local_part != "*")
        .map(|(identity, _)| identity)
        .next()
        .unwrap_or_else(|| {
            sender_identities
                .first()
                .map(|(identity, _)| identity)
                .unwrap()
        });
    debug!("JMAP identity for sender is `{:?}'", identity);

    // TODO: avoid clone here?
    Ok(identity.id.clone())
}

fn address_to_string(address: &email_parser::address::EmailAddress) -> String {
    format!("{}@{}", address.local_part, address.domain)
}
