//! Screenshot projection over the single conversation attachment store.
use crate::agent_attachment_store as store;
use desk_diagnose_core::{conversation_image::ImageAttachment, session::PersistedAgentSession};
use sea_orm::{DatabaseConnection, DbErr};

fn invalid() -> DbErr {
    DbErr::Custom("Screenshot attachment is unavailable".into())
}

pub async fn store(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    image: &ImageAttachment,
    pixels: &[u8],
) -> Result<(), DbErr> {
    let part = image
        .attachment_record(session, pixels)
        .map_err(|_| invalid())?;
    store::store_batch(db, session, &[part]).await?;
    Ok(())
}

pub async fn list_records(
    db: &DatabaseConnection,
    run: &str,
    actor: &str,
    before: Option<&str>,
) -> Result<Vec<ImageAttachment>, DbErr> {
    store::list_images(db, run, actor, before)
        .await?
        .into_iter()
        .map(|meta| meta.image_source.ok_or_else(invalid))
        .collect()
}

pub async fn list(
    db: &DatabaseConnection,
    run: &str,
    actor: &str,
    before: Option<&str>,
) -> Result<Vec<desk_agent_protocol::visual_evidence::VisualEvidenceFrame>, DbErr> {
    Ok(list_records(db, run, actor, before)
        .await?
        .into_iter()
        .map(|image| image.frame)
        .collect())
}

pub async fn read(
    db: &DatabaseConnection,
    run: &str,
    actor: &str,
    id: Option<&str>,
    call: Option<&str>,
) -> Result<(ImageAttachment, Vec<u8>), DbErr> {
    let id = match (id, call) {
        (Some(id), None) => id.to_string(),
        (None, Some(call)) => store::image_id_by_call(db, run, actor, call).await?,
        _ => return Err(invalid()),
    };
    let part = store::read(db, run, actor, &id, true).await?;
    let image = part.metadata.image_source.ok_or_else(invalid)?;
    image.restore(&part.content).map_err(|_| invalid())?;
    Ok((image, part.content))
}

pub async fn delete(
    db: &DatabaseConnection,
    run: &str,
    actor: &str,
    id: &str,
) -> Result<(), DbErr> {
    store::delete(db, run, actor, &[id.to_string()]).await
}
