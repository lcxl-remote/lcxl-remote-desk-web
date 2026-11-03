//! HTTP access logs omit query strings and client-supplied headers.

/// `%U` renders only `HttpRequest::path()`. Do not use `%r` or Referer: both
/// may contain signaling node credentials or capability selectors.
pub const REQUEST_LOG_FORMAT: &str = "%a %U %s %b %T";
