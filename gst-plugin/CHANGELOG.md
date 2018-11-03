# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](http://keepachangelog.com/en/1.0.0/)
and this project adheres to [Semantic Versioning](http://semver.org/spec/v2.0.0.html),
specifically the [variant used by Rust](http://doc.crates.io/manifest.html#the-version-field).

## [0.3.0] - 2018-09-08
### Added
- Support for subclassing pads, ghost pads, aggregator and aggregator pads
- Support for implementing child proxy interface
- Generic catch_panic_pad_function() that allows wrapping around pad functions
  for catching their panics and converting them into error messages
- More Rust-like FlowError enum that can be used to simplify implementations

### Changed
- Use ptr::NonNull in various places
- Move to standalone gobject-subclass crate and refactor for its API changes
- Removed CallbackGuard as unwinding across FFI boundaries is not undefined
  behaviour anymore and will cause an immediate panic instead
- Use new Object::downcast_ref() to prevent some unneeded clones

## [0.2.1] - 2018-05-09
### Fixed
- Fix memory leak in ElementClass::add_pad_template() related to floating
  reference handling.

## [0.2.0] - 2018-03-20
### Changed
- Update to gstreamer-rs 0.11
- BaseTransform::transform_caps() takes caps by reference instead of value

### Added
- Send+Sync impls for all wrapper types

## [0.1.4] - 2018-02-12
### Fixed
- Fix BaseSrc::unlock_stop() and the same in BaseSink. It was calling unlock()
  instead of unlock_stop() before, making it rather useless.

## [0.1.3] - 2018-01-15
### Fixed
- Only require GStreamer >= 1.8, not >= 1.10. We didn't use any 1.10 API
  anymore since quite a while
- Don't call BaseTransform::transform_ip in passthrough mode with a mutable
  reference, but call a new transform_ip_passthrough with an immutable
  reference. The mutable reference would've failed all mutable operations.

## [0.1.2] - 2018-01-03
### Fixed
- BaseTransform::transform_caps() caps parameter is not owned when chainging
  to the parent class' implementation either

## [0.1.1] - 2018-01-03
### Fixed
- BaseTransform::transform_caps() caps parameter is not owned

## [0.1.0] - 2017-12-22
- Initial release of the `gst-plugin` crate.

[Unreleased]: https://gitlab.freedesktop.org/gstreamer/gst-plugin-rs/compare/0.3.0...HEAD
[0.2.1]: https://gitlab.freedesktop.org/gstreamer/gst-plugin-rs/compare/0.2.1...0.3.0
[0.2.1]: https://gitlab.freedesktop.org/gstreamer/gst-plugin-rs/compare/0.2.0...0.2.1
[0.2.0]: https://gitlab.freedesktop.org/gstreamer/gst-plugin-rs/compare/0.1.4...0.2.0
[0.1.4]: https://gitlab.freedesktop.org/gstreamer/gst-plugin-rs/compare/0.1.3...0.1.4
[0.1.3]: https://gitlab.freedesktop.org/gstreamer/gst-plugin-rs/compare/0.1.2...0.1.3
[0.1.2]: https://gitlab.freedesktop.org/gstreamer/gst-plugin-rs/compare/0.1.1...0.1.2
[0.1.1]: https://gitlab.freedesktop.org/gstreamer/gst-plugin-rs/compare/0.1.0...0.1.1
