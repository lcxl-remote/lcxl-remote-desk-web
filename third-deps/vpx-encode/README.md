# vpx-encode

Rust interface to libvpx encoder

This crate provides a Rust API to use
[libvpx](https://en.wikipedia.org/wiki/Libvpx) for encoding images.

It it based entirely on code from [srs](https://crates.io/crates/srs).
Compared to the original `srs`, this code has been simplified for use as a
library and updated to add support for both the VP8 codec and (optionally)
the VP9 codec.

## Optional features

Compile with the cargo feature `vp9` to enable support for the VP9 codec.

## Example

An example of using `vpx-encode` can be found in the
[`record-screen`](https://crates.io/crates/record-screen) program. The source
code of record screen is
[here](https://github.com/astraw/vpx-encode/tree/master/record-screen).

## Contributing

All contributions are appreciated.

License: MIT
