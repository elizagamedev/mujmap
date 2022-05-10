# mujmap

mujmap is a tool to synchronize your [notmuch](https://notmuchmail.org/)
database with a server supporting the [JMAP mail
protocol](https://jmap.io/spec.html). Specifically, it downloads new messages
and synchronizes notmuch tags with mailboxes and keywords both ways. It is very
similar to [Lieer](https://github.com/gauteh/lieer) in terms of design and
operation.

## Disclaimer
mujmap is in quite an early state and comes with no warranty. While I am using
it myself for my email, and I have taken caution to insert an abundance of
paranoia where permanent changes are concerned, I have only tested it on one
provider ([Fastmail](https://fastmail.com)) and one OS (Linux) and I can't
guarantee it won't completely explode your inbox and destroy all your most
treasured kitten photos. Please use with caution for the time being.
Contributions very welcome!

## Installation

Install with [cargo](https://doc.rust-lang.org/cargo/):

```shell
cargo install --git https://github.com/elizagamedev/mujmap.git
```

Availablility on [crates.io](https://crates.io) is pending a new minor release
of [notmuch-rs](https://github.com/vhdirk/notmuch-rs). After that, plans also
include an official [Nix package](https://nixos.org/) and
[home-manager](https://github.com/nix-community/home-manager) module.

## Usage
mujmap can be the sole mail agent in your notmuch database or live alongside
others, it can manage two or more independent JMAP accounts in the same
database, and be used across different notmuch databases, all with different
configurations.

In the directory that you want to use as the maildir for a specific mujmap
instance, place a mujmap.toml file
([example](https://github.com/elizagamedev/mujmap/blob/main/mujmap.toml.example)). This
directory *must* be a subdirectory of the notmuch root directory. Then, invoke
mujmap from that directory, or from another directory pointing to it with the
`-C` option. Check `mujmap --help` for more options.

## Quirks
-   If you change any of the "tag" options in the config file *after* you
    already have a working setup, be sure to heed the warning in the example
    config file and follow the instructions!
-   No matter how old the change, any messages changed in the local database
    in-between syncs will overwrite remote changes. This is due to an API
    limitation, described in more detail in the [Behavior](#behavior) section.
-   This software probably doesn't work on Windows. I have no evidence of this
    being the case, it's just a hunch. Please prove me wrong.

## Migrating from IMAP+notmuch

Unfortunately, there is no straightforward way to migrate yet. The following is
one method you can use, **ONLY after you make a backup of your notmuch
database**:

1. Ensure you're fully synchronized with the IMAP server.
2. Create a new notmuch database by setting the environment variable
   `NOTMUCH_CONFIG` to a different path from your IMAP-managed database. This
   will be our "intermediate" database.
3. Configure mujmap in this intermediate database and perform a `mujmap sync`
   here. Now your new database will contain a copy of all the messages on the
   server with the correct filenames necessary for mujmap to function.
4. Copy your newly-populated mujmap maildir to be under your *IMAP* database's
   path. Make sure that `NOTMUCH_CONFIG` points to the original database again.
   Run `notmuch new --no-hooks` to add these new messages to your IMAP database.
   They should all be duplicates of your IMAP messages, so they will inherit all
   of your tags.
5. Remove your IMAP maildir, leaving only the JMAP/mujmap maildir. If you run
   `notmuch new --no-hooks` again, it should tell you messages were renamed, but
   not removed. If not, *stop* and restore your backup, because something might
   have gone wrong. Now your IMAP database with your carefully curated tags are
   pointing to files with mujmap-compatible filenames.
6. Edit the `mujmap.state.json` file and change the `"notmuch_revision"` value
   to `0`. This will tell mujmap that *all* local mail tags since revision `0`
   (i.e. everything) should override the server mail properties. You can leave
   the `"jmap_state"` property alone.
7. Run `mujmap sync` again and cross your fingers!

## Behavior
TL;DR: mujmap downloads new mail files, merges changes locally, preferring local
changes in the event of a conflict, and then pushes changes to the remote.

mujmap operates in roughly these steps:

1.  mujmap gathers all metadata about emails that were created, potentially
    updated, or destroyed on the server since it was last run.

    JMAP does not tell us *exactly* what changes about a message, only that one
    of the [very many
    properties](https://datatracker.ietf.org/doc/html/rfc8621#section-4) of the
    JMAP `Email` object has changed. It's possible that nothing at all that we
    care about has changed. This is especially true if we're doing a "full
    sync", which can happen if we lose the state information from the last run
    or if such information expires server-side. In that case, we have to query
    everything from scratch and treat every single message as a "potential
    update".
2.  mujmap downloads all new messages into a cache.
3.  mujmap gathers a list of all messages which were updated in the database
    locally since it was last ran; we call these "locally updated" messages.
4.  mujmap adds the new remote messages to the local notmuch database, then
    updates all local messages *except* the locally updated messages to reflect
    the remote state of the message.

    We skip updating the locally updated messages because again, there is no way
    to ask the JMAP server *what* changes were made; we can only retrieve the
    latest state of the tags as they exist on the server. We prefer preserving
    local tag changes over remote changes.
5.  We push the locally updated messages to the remote.

    Unfortunately, the notmuch API also does not grant us any change history, so
    we are limited to looking at the latest state of the database entries as
    with JMAP. It seems possible that Xapian, the underlying database backend,
    does in fact support something like this, but it's not exposed by notmuch
    yet.
6.  Record the *first* JMAP `Email` state we received and the *next* notmuch
    database revision in "mujmap.state.json" to be read next time mujmap is run
    back in step 1.

For more of an explanation about this already probably over-explained process,
the slightly out-of-date and not completely-accurately-implemented-as-written
[DESIGN.org](https://github.com/elizagamedev/mujmap/blob/main/DESIGN.org) file
goes into more detail.

## Limitations
mujmap cannot and will never be able to:

-   Upload new messages
-   Modify message contents
-   Delete messages (other than tagging them as `deleted` or `spam`)
