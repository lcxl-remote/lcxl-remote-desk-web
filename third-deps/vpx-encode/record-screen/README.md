# record-screen

Record a movie of your screen

This simple utility records a movie (video only, no audio) of your screen.

It it based entirely on code from [srs](), and has been maintained as an
example application to test the [vpx-encode]() library.

Note: compile and test in release mode. Otherwise, converting the video
frames is too slow for realtime encoding.

## Installation

This can be installed with:

```sh
cargo install record-screen
```

Don't forget to install `libvpx`.

## Video Format

The video is stored as a WebM file.

## Contributing

All contributions are appreciated.

License: MIT
