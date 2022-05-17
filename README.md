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

**If you do decide to use mujmap**, please look at the list of open issues
first. If you are installing the latest Cargo release instead of the latest git
revision, **also consider** looking at the issues which have since been closed
since the latest Cargo release.

## Installation

Please first read the [Disclaimer](#disclaimer) section.

Install with [cargo](https://doc.rust-lang.org/cargo/):

```shell
cargo install mujmap
```

You may instead want to install from the latest `main` revision as bugs are
regularly being fixed:

```shell
cargo install --git https://github.com/elizagamedev/mujmap
```

Plans also include an official [Nix package](https://nixos.org/) and
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
-   Duplicate messages may behave strangely. See #13.
-   This software probably doesn't work on Windows. I have no evidence of this
    being the case, it's just a hunch. Please prove me wrong.

## Migrating from IMAP+notmuch

Unfortunately, there is no straightforward way to migrate yet. The following is
an (untested) method you can use, **ONLY after you make a backup of your notmuch
database**, and **ONLY after you have verified that mujmap works correctly for
your account in an independent instance of a notmuch database (see the notmuch
manpages for information on how to do this)**:

1.  Ensure you're fully synchronized with the IMAP server.
2.  Add a maildir for mujmap as a sibling of your already-existing maildirs.
    Configure it as you please, but don't invoke `mujmap sync` yet.
3.  Create a file called `mujmap.state.json` in this directory alongside
    `mujmap.toml` with the following contents:

```json
{"notmuch_revision":0}
```
4.  Run `mujmap --dry-run sync` here. This will not actually make any changes to
    your maildir, but will allow you to verify your config and download email
    into a cache.
5.  Run `mujmap sync` here to sync your mail for real. This will the downloaded
    email to the mujmap maildir and add them to your notmuch database. Because
    these messages should be duplicates of your existing messages, they will
    inherit the duplicates' tags, and then push them back to the server.
5.  Remove your old IMAP maildirs and run `notmuch new --no-hooks`. If
    everything went smoothly, notmuch shouldn't mention any files being removed
    in its output.

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

## Troubleshooting

### Status Code 401 (Unauthorized)
- [ ] Ensure that your mail server supports HTTP Basic Auth. *Fastmail does.* See #5.
- [ ] Verify that you are using the correct username and password. Fastmail
      requires a special third-party password *specifically for JMAP access*.
- [ ] Verify that you are using a `password_command` which prints the correct
      password to stdout. If the password command fails, mujmap logs its stderr.
- [ ] If using Fastmail, check your login logs on the website for additional
      context.
