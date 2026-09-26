//! Bind registration and lifecycle signals to one AT-SPI Registry owner.
use super::super::{Result, error};
use futures_util::{
    StreamExt,
    stream::{BoxStream, SelectAll},
};
use zbus::{Connection, MessageStream, Proxy};

const REGISTRY: &str = "org.a11y.atspi.Registry";

pub(super) struct Subscription {
    pub owner: String,
    pub events: SelectAll<BoxStream<'static, Result<bool>>>,
}

pub(super) async fn owner(connection: &Connection) -> Result<String> {
    Proxy::new(
        connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    .map_err(error)?
    .call("GetNameOwner", &(REGISTRY,))
    .await
    .map_err(error)
}

pub(super) async fn subscribe(connection: &Connection) -> Result<Subscription> {
    let changed = MessageStream::for_match_rule(
        "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='org.a11y.atspi.Registry'",
        connection, Some(16),
    ).await.map_err(error)?;
    let objects = MessageStream::for_match_rule(
        "type='signal',interface='org.a11y.atspi.Event.Object'",
        connection,
        Some(256),
    )
    .await
    .map_err(error)?;
    let original = owner(connection).await?;
    // The well-known name could move while registration calls are in flight.
    // Send every registration to the captured unique owner, then recheck it.
    let registry = Proxy::new(
        connection,
        original.as_str(),
        "/org/a11y/atspi/registry",
        REGISTRY,
    )
    .await
    .map_err(error)?;
    for event in [
        "object:children-changed",
        "object:state-changed:defunct",
        "object:property-change:accessible-role",
    ] {
        registry
            .call::<_, _, ()>("RegisterEvent", &(event, Vec::<String>::new(), ""))
            .await
            .map_err(error)?;
    }
    if owner(connection).await? != original {
        return Err(error("AT-SPI Registry changed during subscription"));
    }
    let mut events = SelectAll::new();
    events.push(
        changed
            .map(|message| {
                message.map_err(error)?;
                Err(error("AT-SPI Registry owner changed"))
            })
            .chain(futures_util::stream::once(async {
                Err(error("AT-SPI owner subscription ended"))
            }))
            .boxed(),
    );
    events.push(
        objects
            .map(|message| {
                let message = message.map_err(error)?;
                let header = message.header();
                let member = header.member().map(|member| member.as_str()).unwrap_or("");
                Ok(matches!(
                    member,
                    "ChildrenChanged" | "StateChanged" | "PropertyChange"
                ))
            })
            .chain(futures_util::stream::once(async {
                Err(error("AT-SPI tree subscription ended"))
            }))
            .boxed(),
    );
    Ok(Subscription {
        owner: original,
        events,
    })
}

#[cfg(test)]
mod tests;
