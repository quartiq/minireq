# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

# [0.5.0]

## Changed
* [breaking] Handlers are no longer stored internally. The `poll()` function is now required to
perform the mapping to individual handlers. This is intended to make the handler API signature more
flexible, as not all handlers may need the same context.

## [0.4.0] - 2024-06-13

* [breaking] Updated to `minimq` v0.9.0


## [0.3.0] - 2023-11-01

* Handlers now take a fourth argument, `output_buffer`, where they can serialize their response into
directly
* Minimq was bumped
* Users now provide memory for storing handlers into.
* Handler errors will now automatically be serialized into the response on failure, and are required
to implement `core::fmt::Display`

## [0.2.0] - 2023-06-22

* Minimq bumped to v0.7

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

[0.5.0]: https://github.com/quartiq/minireq/releases/tag/v0.5.0
[0.4.0]: https://github.com/quartiq/minireq/releases/tag/v0.4.0
[0.3.0]: https://github.com/quartiq/minireq/releases/tag/v0.3.0
[0.2.0]: https://github.com/quartiq/minireq/releases/tag/v0.2.0
[0.1.1]: https://github.com/quartiq/minireq/releases/tag/v0.1.1
[0.1.0]: https://github.com/quartiq/minireq/releases/tag/v0.1.0
