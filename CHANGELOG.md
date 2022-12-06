# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.1] - 2022-12-06

### Added
* All registered command topics are now published upon connection with the broker or when the
command is registered.
* Minimq version updated to simplify core logic.

### Fixed
* [#3](https://github.com/quartiq/minireq/issues/3) Fixed an issue where large responses would trigger an internal panic
* [#7](https://github.com/quartiq/minireq/issues/7) Fixed serialization of responses so they are readable
* [#12](https://github.com/quartiq/minireq/issues/12) `serde_json_core` is now re-exported at the
  crate root.

## [0.1.0] - 2022-03-28

Library initially released on crates.io

[0.1.1]: https://github.com/quartiq/minireq/releases/tag/v0.1.1
[0.1.0]: https://github.com/quartiq/minireq/releases/tag/v0.1.0
