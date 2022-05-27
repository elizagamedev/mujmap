# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
### Fixed
- Introduced workaround for some JMAP servers which did not support the patch
  syntax that mujmap was using for updating mailboxes. (#19)
- Mail which belongs to "ignored" mailboxes will no longer be added to the
  `archive`-role mailbox unnecessarily.

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

[Unreleased]: https://github.com/elizagamedev/mujmap/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/elizagamedev/mujmap/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/elizagamedev/mujmap/releases/tag/v0.1.0
