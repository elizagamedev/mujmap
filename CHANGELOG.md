# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
### Added
- Support for bearer token authentication (#40)
- New config options `mail_dir` and `state_dir` to allow mujmap's persistent
  storage to be split out according to local policy (eg XDG dirs) (#33)

### Changed
- mujmap now prints a more comprehensive guide on how to recover from a missing
  state file. (#15)
- Leading and trailing whitespace (including newlines) is now removed from the
  password returned by `password_command`. (#41)

## [0.2.0] - 2022-06-06
### Added
- mujmap can now send emails! See the readme for details.
- New configuration option `convert_dos_to_unix` which converts DOS newlines to
  Unix newlines for all newly downloaded mail files.
- New configuration option `cache_dir` which changes the directory where mujmap
  downloads new files before adding them to the maildir.
- By default, try to discover the JMAP server from the domain part of the
  `username` configuration option. (#28)

### Changed
- New mail files will have their line endings changed by default to Unix; see
  the above `convert_dos_to_unix` configuration option. Existing files are
  unaffected.
- Most JMAP error objects now contain additional properties besides
  "description". (#20)

### Fixed
- Introduced workaround for some JMAP servers which did not support the patch
  syntax that mujmap was using for updating mailboxes. (#19)
- Mail which belongs to "ignored" mailboxes will no longer be added to the
  `archive`-role mailbox unnecessarily.
- Symlinked maildirs now properly handled. (#16)
- Messages managed by mujmap now synchronize their tags with the maildir flags
  if notmuch is configured to do so. This fixes interfaces which depend on such
  flags being present, like neomutt. (#8)

## [0.1.1] - 2022-05-17
### Changed
- Improved diagnostics for password command/authentication failures.
- mujmap will replace replace unindexed mail files in the maildir with files
  from the cache if they have the same filename.

### Fixed
- Mail download operations will now correctly retry in all cases of failure.
  (#7)
- A `retries` configuration option of `0` now correctly interpreted as infinite.
- Automatic tags are no longer clobbered. mujmap will actively ignore automatic
  tags. (#9)
- Messages considered duplicates by notmuch will now properly synchronize with
  the server. See #13 for more details about duplicate messages. (#12)

## [0.1.0] - 2022-05-12
### Added
- Initial release.

[Unreleased]: https://github.com/elizagamedev/mujmap/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/elizagamedev/mujmap/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/elizagamedev/mujmap/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/elizagamedev/mujmap/releases/tag/v0.1.0
