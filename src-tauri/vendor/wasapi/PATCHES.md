# wasapi 0.24.0 local safety patch

Upstream: https://github.com/HEnquist/wasapi-rs (crates.io wasapi 0.24.0).
Imported src, README.md, LICENSE.txt and Cargo.toml.orig (named Cargo.toml).
MIT license retained. Global Cargo registry is unchanged.

Only src/api.rs is modified: both capture read APIs treat SILENT as zero bytes
without constructing a slice from the input pointer, reject null nonsilent data,
and release the acquired nonempty buffer before propagating copy errors.
The deque API propagates ReleaseBuffer failure instead of panicking.
Two narrow regression tests exercise the shared pointer/flag conversion boundary.
Activation waiting semantics are unchanged and remain synchronous/unbounded.

Added `AudioSessionControl::set_display_name` (IAudioSessionControl::SetDisplayName with
GUID_NULL as the event context), so devocal-engine can name its render session in the
volume mixer. Nothing else calls it.
