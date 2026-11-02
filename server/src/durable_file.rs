//! Stable server facade for durable file operations owned by `desk-utils`.

pub use desk_utils::durable_file::{
    FileMode, PublishError, durable_atomic_write, durable_publish_new, durable_replace,
};
