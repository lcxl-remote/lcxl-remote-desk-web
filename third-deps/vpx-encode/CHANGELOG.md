# 0.5.0 - 2021-06-07

## Changed

* `Error` type is now a struct with a message and a, optionally, a backtrace. To
  get backtraces, use the `backtrace` cargo feature. (Currently, nightly rust is
  required for this feature.)
