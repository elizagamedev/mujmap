# mujmap

mujmap is a tool to synchronize your [notmuch](https://notmuchmail.org/)
database with a server supporting the [JMAP mail
protocol](https://jmap.io/spec.html). Specifically, it downloads new messages
and synchronizes notmuch tags with mailboxes and keywords both ways and can send
emails via a sendmail-like interface. It is very similar to
[Lieer](https://github.com/gauteh/lieer) in terms of design and operation.

## Disclaimer
mujmap is in quite an early state and comes with no warranty. I use it myself,
it has been seeing steady adoption among other users, and I have taken caution
to insert an abundance of paranoia where permanent changes are concerned. It is
known to work on Linux and macOS with at least one webmail provider
([Fastmail](https://fastmail.com)).

**If you do decide to use mujmap**, please look at the list of open issues
first. If you are installing the latest Cargo release instead of the latest git
revision, **also consider** looking at the issues in the
[changelog](https://github.com/elizagamedev/mujmap/blob/main/CHANGELOG.md) that
have been found and resolved since the latest release.

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

There is also an official [Nix
package](https://github.com/NixOS/nixpkgs/blob/master/pkgs/applications/networking/mujmap/default.nix).
A [home-manager module](https://github.com/nix-community/home-manager/pull/2960)
is underway.

## Usage
mujmap can be the sole mail agent in your notmuch database or live alongside
others, it can manage two or more independent JMAP accounts in the same
database, and be used across different notmuch databases, all with different
configurations.

In the directory that you want to use as the maildir for a specific mujmap
instance, place a mujmap.toml file
([example](https://github.com/elizagamedev/mujmap/blob/main/mujmap.toml.example)).
This directory *must* be a subdirectory of the notmuch root directory. Then,
invoke mujmap from that directory, or from another directory pointing to it with
the `-C` option. Check `mujmap --help` for more options. Specific

### Syncing
Use `mujmap sync` to synchronize your mail. TL;DR: mujmap downloads new mail
files, merges changes locally, preferring local changes in the event of a
conflict, and then pushes changes to the remote.

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

#### Pushing without Pulling
Besides what is described above, you may also use `mujmap push` to local push
changes without pulling in remote changes. This may be useful when invoking
mujmap in pre/post notmuch hooks. You should only use `push` over `sync` when
specifically necessary to reduce the number of redundant operations.

There is no `mujmap pull`, since pulling without pushing complicates the design
tenet that the mujmap database is the single source of truth during a conflict.
(The reason being that pulling without pushing changes the notmuch database, and
now mujmap thinks those changes are in fact local revisions which must be
pushed, potentially reverting changes made by a third party on the remote. If
that's confusing to you, sorry, it's not easy to describe the problem
succinctly.) It's possible to sort of work around this issue, but in almost
every case I can think of, you might as well just `sync` instead.

### Sending
Use `mujmap send` to send an email. This subcommand is designed to operate
mostly like sendmail; i.e., it reads an
[RFC5322](https://datatracker.ietf.org/doc/html/rfc5322) mail file from stdin
and sends it off into cyberspace. That said, this interface is still
experimental.

The arguments `-i`, `-oi`, `-f`, and `-F` are all accepted but ignored for
sendmail compatibility. The sender is always determined from the email message
itself.

The recipients are specified in the same way as sendmail. They must either be
specified at the end of the argument list, or mujmap can infer them from the
message itself if you specify `-t`. If `-t` is specified, any recipient
arguments at the end of the message are ignored, and mujmap will warn you.

#### Emacs configuration
```elisp
(setq sendmail-program "mujmap"
      message-send-mail-function #'message-send-mail-with-sendmail
      message-sendmail-extra-arguments '("-C" "/path/to/mujmap/maildir" "send"))
```

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

## Limitations
mujmap cannot and will never be able to:

-   Modify message contents.
-   Delete messages (other than tagging them as `deleted` or `spam`).

## Troubleshooting
### Status Code 401 (Unauthorized)

If you're using Fastmail (which, let's be honest, is practically a certainty at
the time of writing), you may have recently encountered errors with
username/password authentication (HTTP Basic Auth). This may be caused by
Fastmail phasing out username/password-based authentication methods, as
described in [this blog
post](https://jmap.topicbox.com/groups/fastmail-api/Tc47db6ee4fbb5451/).

While this is objectively a good thing, and while it seems the intention was to
roll this change out slowly, the API endpoint advertised by Fastmail DNS SRV
records has almost immediately changed following the publication of this blog
post, causing 401 errors in existing mujmap configurations. You have two
options:

- Switch to bearer tokens by following the guide in the blog post. mujmap
  supports bearer tokens via the `password_command` config option in the latest
  `main` branch revision but not yet in a versioned release.
- Remove `fqdn` from your config if it's set, and add or change `session_url` to
  explicitly point to the old JMAP endpoint, located at
  `https://api.fastmail.com/.well-known/jmap`.

If your 401 errors are unrelated to the above situation, try the following
steps:

- [ ] Ensure that your mail server supports either HTTP Basic Auth or Bearer
      token auth.
- [ ] Verify that you are using the correct username and password/bearer token.
      If you are using HTTP Basic Auth, Fastmail requires a special third-party
      password *specifically for JMAP access*.
- [ ] Verify that you are using a `password_command` which prints the correct
      password to stdout. If the password command fails, mujmap logs its stderr.
- [ ] If using Fastmail, check your login logs on the website for additional
      context.

### Invalid cross-device link
This error will occur if your mail directory is stored on a different device
than your cache directory. By default, mujmap's cache is stored in
`XDG_CONFIG_HOME` on Linux/FreeBSD and `~/Library/Caches` on macOS. You can
change this location by setting `config_dir` in mujmap.toml.

The rationale for downloading messages into a cache instead of directly into the
maildir is because mujmap is designed to be able to roll-back local state
changes in the event of a catastrophic failure to the best of its ability, which
includes not leaving mail files in the maildir which haven't been fully
integrated into notmuch's database. As an alternative, mujmap could have
depended on notmuch being configured to ignore in-progress downloads, but this
is much more prone to user error.
