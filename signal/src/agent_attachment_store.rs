//! Transactional attachment metadata with immutable, durably written local files.
use crate::entity::{agent_attachment as attachment, agent_session};
use desk_diagnose_core::{
    conversation_attachment::{
        AttachmentMetadata, Availability,
        batch::{PreparedAttachment, plan_batch},
    },
    session::PersistedAgentSession,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction,
    DbBackend, DbErr, EntityTrait, ExprTrait, QueryFilter, QueryOrder, QuerySelect, Set, Statement,
    sea_query::Expr,
};

fn invalid() -> DbErr {
    DbErr::Custom("Conversation attachment is inaccessible or invalid".into())
}

fn decode(row: &attachment::Model) -> Result<AttachmentMetadata, DbErr> {
    let mut meta: AttachmentMetadata =
        serde_json::from_str(&row.metadata_json).map_err(|_| invalid())?;
    if meta.attachment_id != row.id
        || meta.conversation_id != row.conversation_id
        || meta.actor_id != row.actor_id
        || meta.device_id != row.device_id
        || meta.size_bytes != row.size_bytes as u64
        || (meta.availability == Availability::Available) != row.available
        || row.created_at_unix_ms < 0
        || row.last_accessed_at_unix_ms < 0
    {
        return Err(invalid());
    }
    meta.created_at_unix_ms = row.created_at_unix_ms as u64;
    meta.last_accessed_at_unix_ms = row.last_accessed_at_unix_ms as u64;
    Ok(meta)
}

async fn database_time(txn: &DatabaseTransaction) -> Result<u64, DbErr> {
    let sql = match txn.get_database_backend() {
        DbBackend::Postgres => {
            "SELECT CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) AS now_ms"
        }
        DbBackend::Sqlite => {
            "SELECT CAST((julianday('now') - 2440587.5) * 86400000 AS BIGINT) AS now_ms"
        }
        DbBackend::MySql => return Err(invalid()),
        // SeaORM marks this external enum non-exhaustive.
        _ => return Err(invalid()),
    };
    let row = txn
        .query_one_raw(Statement::from_string(txn.get_database_backend(), sql))
        .await?
        .ok_or_else(invalid)?;
    let time: i64 = row.try_get("", "now_ms")?;
    u64::try_from(time).map_err(|_| invalid())
}

async fn lock_parent(
    txn: &DatabaseTransaction,
    run: &str,
    actor: &str,
    device: &str,
    fence: Option<(i64, u64)>,
) -> Result<(), DbErr> {
    let mut update = agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::Version,
            Expr::col(agent_session::Column::Version),
        )
        .filter(agent_session::Column::ConversationId.eq(run))
        .filter(agent_session::Column::ActorId.eq(actor))
        .filter(agent_session::Column::DeviceId.eq(device));
    if let Some((version, lease)) = fence {
        update = update
            .filter(agent_session::Column::Version.eq(version))
            .filter(agent_session::Column::LeaseToken.eq(lease as i64));
    }
    if update.exec(txn).await?.rows_affected != 1 {
        return Err(invalid());
    }
    Ok(())
}

pub async fn store_batch(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    incoming: &[PreparedAttachment],
) -> Result<Vec<AttachmentMetadata>, DbErr> {
    for part in incoming {
        if part.metadata.conversation_id != session.conversation_id
            || part.metadata.actor_id != session.actor_id
            || part.metadata.device_id != session.device_id
        {
            return Err(invalid());
        }
    }
    // Validate even an empty-store batch before acquiring a database lock.
    plan_batch(&[], incoming).map_err(|e| DbErr::Custom(e.message))?;
    let root = content_root(db).await?;
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    lock_parent(
        &txn,
        &session.conversation_id,
        &session.actor_id,
        &session.device_id,
        Some((session.version, session.lease_token)),
    )
    .await?;
    let rows = attachment::Entity::find()
        .filter(attachment::Column::ConversationId.eq(&session.conversation_id))
        .all(&txn)
        .await?;
    let existing = rows.iter().map(decode).collect::<Result<Vec<_>, _>>()?;
    let plan = plan_batch(&existing, incoming).map_err(|e| DbErr::Custom(e.message))?;
    let time = database_time(&txn).await?;
    for id in &plan.evict_ids {
        let mut meta = existing
            .iter()
            .find(|meta| &meta.attachment_id == id)
            .ok_or_else(invalid)?
            .clone();
        meta.availability = Availability::Evicted { at_unix_ms: time };
        attachment::Entity::update_many()
            .col_expr(
                attachment::Column::MetadataJson,
                Expr::value(serde_json::to_string(&meta).map_err(|_| invalid())?),
            )
            .col_expr(attachment::Column::Available, Expr::value(false))
            .filter(attachment::Column::Id.eq(id))
            .exec(&txn)
            .await?;
    }
    let mut saved = Vec::new();
    for (index, part) in incoming.iter().enumerate() {
        if !plan.insert_indices.contains(&index) {
            let row = rows
                .iter()
                .find(|row| row.id == part.metadata.attachment_id)
                .ok_or_else(invalid)?;
            let meta = decode(row)?;
            meta.verify(&read_content(&root, row.content_key.as_deref()).await?)
                .map_err(|_| invalid())?;
            saved.push(meta);
            continue;
        }
        let mut meta = part.metadata.clone();
        meta.created_at_unix_ms = time;
        meta.last_accessed_at_unix_ms = time;
        let content_key = write_content(&root, &part.content).await?;
        attachment::ActiveModel {
            id: Set(meta.attachment_id.clone()),
            conversation_id: Set(meta.conversation_id.clone()),
            actor_id: Set(meta.actor_id.clone()),
            device_id: Set(meta.device_id.clone()),
            metadata_json: Set(serde_json::to_string(&meta).map_err(|_| invalid())?),
            size_bytes: Set(meta.size_bytes as i64),
            created_at_unix_ms: Set(time as i64),
            last_accessed_at_unix_ms: Set(time as i64),
            available: Set(true),
            content_key: Set(Some(content_key)),
        }
        .insert(&txn)
        .await?;
        saved.push(meta);
    }
    txn.commit().await?;
    Ok(saved)
}

async fn subject(
    db: &DatabaseConnection,
    run: &str,
    actor: &str,
) -> Result<agent_session::Model, DbErr> {
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(run))
        .filter(agent_session::Column::ActorId.eq(actor))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
    if session.surface != desk_diagnose_core::session::AgentSessionSurface::AiAssistant
        || session.conversation_id != run
        || session.actor_id != actor
        || session.device_id != row.device_id
    {
        return Err(invalid());
    }
    Ok(row)
}

/// Metadata-only paging deliberately does not update access times.
pub async fn list(
    db: &DatabaseConnection,
    run: &str,
    actor: &str,
    before: Option<&str>,
) -> Result<Vec<AttachmentMetadata>, DbErr> {
    list_filtered(db, run, actor, before, false).await
}

pub async fn list_images(
    db: &DatabaseConnection,
    run: &str,
    actor: &str,
    before: Option<&str>,
) -> Result<Vec<AttachmentMetadata>, DbErr> {
    list_filtered(db, run, actor, before, true).await
}

fn image_filter(db: &DatabaseConnection) -> Result<sea_orm::sea_query::SimpleExpr, DbErr> {
    Ok(Expr::cust(match db.get_database_backend() {
        DbBackend::Sqlite => "json_extract(metadata_json, '$.kind') = 'image'",
        DbBackend::Postgres => "(metadata_json::jsonb ->> 'kind') = 'image'",
        _ => return Err(invalid()),
    }))
}

pub async fn image_id_by_call(
    db: &DatabaseConnection,
    run: &str,
    actor: &str,
    call: &str,
) -> Result<String, DbErr> {
    let parent = subject(db, run, actor).await?;
    let field = match db.get_database_backend() {
        DbBackend::Sqlite => "json_extract(metadata_json, '$.tool_call_id') = ?",
        DbBackend::Postgres => "(metadata_json::jsonb ->> 'tool_call_id') = ?",
        _ => return Err(invalid()),
    };
    let ids = attachment::Entity::find()
        .select_only()
        .column(attachment::Column::Id)
        .filter(attachment::Column::ConversationId.eq(run))
        .filter(attachment::Column::ActorId.eq(actor))
        .filter(attachment::Column::DeviceId.eq(parent.device_id))
        .filter(image_filter(db)?)
        .filter(Expr::cust_with_values(field, [call.to_string()]))
        .limit(2)
        .into_tuple::<String>()
        .all(db)
        .await?;
    // Reused call IDs must not silently select an arbitrary historical image.
    if ids.len() != 1 {
        return Err(invalid());
    }
    Ok(ids.into_iter().next().unwrap())
}

async fn list_filtered(
    db: &DatabaseConnection,
    run: &str,
    actor: &str,
    before: Option<&str>,
    images_only: bool,
) -> Result<Vec<AttachmentMetadata>, DbErr> {
    let parent = subject(db, run, actor).await?;
    let mut query = attachment::Entity::find()
        .filter(attachment::Column::ConversationId.eq(run))
        .filter(attachment::Column::ActorId.eq(actor))
        .filter(attachment::Column::DeviceId.eq(&parent.device_id));
    if images_only {
        query = query.filter(image_filter(db)?);
    }
    if let Some(id) = before {
        let cursor = query
            .clone()
            .select_only()
            .column(attachment::Column::CreatedAtUnixMs)
            .filter(attachment::Column::Id.eq(id))
            .into_tuple::<i64>()
            .one(db)
            .await?
            .ok_or_else(invalid)?;
        query = query.filter(
            sea_orm::Condition::any()
                .add(attachment::Column::CreatedAtUnixMs.lt(cursor))
                .add(
                    sea_orm::Condition::all()
                        .add(attachment::Column::CreatedAtUnixMs.eq(cursor))
                        .add(attachment::Column::Id.lt(id)),
                ),
        );
    }
    if images_only {
        query = query.filter(attachment::Column::Available.eq(true));
    }
    // Explicit projection prevents loading all attachment blobs for the list.
    let rows = query
        .select_only()
        .column(attachment::Column::Id)
        .column(attachment::Column::ConversationId)
        .column(attachment::Column::ActorId)
        .column(attachment::Column::DeviceId)
        .column(attachment::Column::MetadataJson)
        .column(attachment::Column::SizeBytes)
        .column(attachment::Column::CreatedAtUnixMs)
        .column(attachment::Column::LastAccessedAtUnixMs)
        .column(attachment::Column::Available)
        .expr_as(Expr::value(Option::<String>::None), "content_key")
        .order_by_desc(attachment::Column::CreatedAtUnixMs)
        .order_by_desc(attachment::Column::Id)
        .limit(32)
        .all(db)
        .await?;
    rows.iter().map(decode).collect()
}

/// A successful content snapshot may finish even if a later transaction evicts it.
pub async fn read(
    db: &DatabaseConnection,
    run: &str,
    actor: &str,
    id: &str,
    consume: bool,
) -> Result<PreparedAttachment, DbErr> {
    let parent = subject(db, run, actor).await?;
    let root = content_root(db).await?;
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    lock_parent(&txn, run, actor, &parent.device_id, None).await?;
    let row = attachment::Entity::find_by_id(id)
        .filter(attachment::Column::ConversationId.eq(run))
        .filter(attachment::Column::ActorId.eq(actor))
        .filter(attachment::Column::DeviceId.eq(parent.device_id))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    let mut metadata = decode(&row)?;
    let content = if row.available {
        read_content(&root, row.content_key.as_deref()).await?
    } else {
        vec![]
    };
    metadata
        .verify(&content)
        .map_err(|e| DbErr::Custom(e.message))?;
    if consume {
        metadata.last_accessed_at_unix_ms = std::cmp::max(
            database_time(&txn).await?,
            metadata.last_accessed_at_unix_ms,
        );
        attachment::Entity::update_many()
            .col_expr(
                attachment::Column::LastAccessedAtUnixMs,
                Expr::value(metadata.last_accessed_at_unix_ms as i64),
            )
            .filter(attachment::Column::Id.eq(id))
            .exec(&txn)
            .await?;
    }
    txn.commit().await?;
    Ok(PreparedAttachment { metadata, content })
}

async fn content_root(db: &impl ConnectionTrait) -> Result<std::path::PathBuf, DbErr> {
    let row = db
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA database_list",
        ))
        .await?
        .ok_or_else(invalid)?;
    let path: String = row.try_get("", "file")?;
    if path.is_empty() {
        return Err(DbErr::Custom(
            "Attachment storage requires a file-backed database".into(),
        ));
    }
    Ok(std::path::Path::new(&path)
        .parent()
        .ok_or_else(invalid)?
        .join("assistant-attachments"))
}

fn content_path(root: &std::path::Path, key: &str) -> Result<std::path::PathBuf, DbErr> {
    if key.len() != 32 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    Ok(root.join(key))
}

async fn read_content(root: &std::path::Path, key: Option<&str>) -> Result<Vec<u8>, DbErr> {
    use tokio::io::AsyncReadExt;
    let path = content_path(root, key.ok_or_else(invalid)?)?;
    let file = tokio::fs::File::open(path).await.map_err(|_| invalid())?;
    let limit = desk_diagnose_core::conversation_attachment::MAX_TEXT_BYTES;
    let mut content = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut content)
        .await
        .map_err(|_| invalid())?;
    if content.len() > limit {
        return Err(invalid());
    }
    Ok(content)
}

async fn write_content(root: &std::path::Path, content: &[u8]) -> Result<String, DbErr> {
    let key = uuid::Uuid::new_v4().simple().to_string();
    let path = content_path(root, &key)?;
    let bytes = content.to_vec();
    tokio::task::spawn_blocking(move || {
        desk_utils::durable_file::durable_atomic_write(
            &path,
            &bytes,
            desk_utils::durable_file::FileMode::OwnerOnly,
        )
    })
    .await
    .map_err(|e| DbErr::Custom(e.to_string()))?
    .map_err(|e| DbErr::Custom(e.to_string()))?;
    Ok(key)
}

/// Deletion is atomic for one bounded selection; a cross-subject ID aborts it.
pub async fn delete(
    db: &DatabaseConnection,
    run: &str,
    actor: &str,
    ids: &[String],
) -> Result<(), DbErr> {
    if ids.is_empty() || ids.len() > 32 {
        return Err(invalid());
    }
    let parent = subject(db, run, actor).await?;
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    lock_parent(&txn, run, actor, &parent.device_id, None).await?;
    let time = database_time(&txn).await?;
    for id in ids {
        let row = attachment::Entity::find_by_id(id)
            .filter(attachment::Column::ConversationId.eq(run))
            .filter(attachment::Column::ActorId.eq(actor))
            .filter(attachment::Column::DeviceId.eq(&parent.device_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let mut meta = decode(&row)?;
        if meta.availability != Availability::Available {
            continue;
        }
        meta.availability = Availability::Deleted { at_unix_ms: time };
        attachment::Entity::update_many()
            .col_expr(attachment::Column::Available, Expr::value(false))
            .col_expr(
                attachment::Column::MetadataJson,
                Expr::value(serde_json::to_string(&meta).map_err(|_| invalid())?),
            )
            .filter(attachment::Column::Id.eq(id))
            .exec(&txn)
            .await?;
    }
    txn.commit().await?;
    Ok(())
}

pub async fn usage(db: &DatabaseConnection, run: &str, actor: &str) -> Result<u64, DbErr> {
    let parent = subject(db, run, actor).await?;
    let used = attachment::Entity::find()
        .select_only()
        .column_as(attachment::Column::SizeBytes.sum(), "used")
        .filter(attachment::Column::ConversationId.eq(run))
        .filter(attachment::Column::ActorId.eq(actor))
        .filter(attachment::Column::DeviceId.eq(parent.device_id))
        .filter(attachment::Column::Available.eq(true))
        .into_tuple::<Option<i64>>()
        .one(db)
        .await?
        .flatten()
        .unwrap_or(0);
    u64::try_from(used).map_err(|_| invalid())
}

/// The SQLite writer reservation excludes concurrent file staging. Content
/// removal follows committed tombstones; a rollback never deletes live bytes.
pub async fn cleanup(db: &DatabaseConnection) -> Result<(), DbErr> {
    let root = content_root(db).await?;
    let txn = crate::db::begin_write(db, attachment::Entity).await?;
    let orphan = Expr::col(attachment::Column::ConversationId).not_in_subquery(
        sea_orm::sea_query::Query::select()
            .column(agent_session::Column::ConversationId)
            .from(agent_session::Entity)
            .to_owned(),
    );
    let rows = attachment::Entity::find()
        .filter(
            sea_orm::Condition::any().add(orphan).add(
                sea_orm::Condition::all()
                    .add(attachment::Column::Available.eq(false))
                    .add(attachment::Column::ContentKey.is_not_null()),
            ),
        )
        .order_by_asc(attachment::Column::CreatedAtUnixMs)
        .limit(128)
        .all(&txn)
        .await?;
    for row in rows {
        if let Some(key) = row.content_key {
            let path = content_path(&root, &key)?;
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(DbErr::Custom(error.to_string())),
            }
            attachment::Entity::update_many()
                .col_expr(
                    attachment::Column::ContentKey,
                    Expr::value(Option::<String>::None),
                )
                .filter(attachment::Column::Id.eq(&row.id))
                .exec(&txn)
                .await?;
        }
        if agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&row.conversation_id))
            .one(&txn)
            .await?
            .is_none()
        {
            attachment::Entity::delete_by_id(row.id).exec(&txn).await?;
        }
    }
    // Keep enumeration progress between bounded sweeps, otherwise a directory
    // containing 1024 live files can starve orphan files forever. This cursor is
    // only cleanup scheduling; all deletion decisions still query the database.
    static SCAN: tokio::sync::Mutex<Option<(std::path::PathBuf, tokio::fs::ReadDir)>> =
        tokio::sync::Mutex::const_new(None);
    let mut scan = SCAN.lock().await;
    if scan.as_ref().is_none_or(|(path, _)| path != &root) {
        *scan = None;
        match tokio::fs::read_dir(&root).await {
            Ok(files) => *scan = Some((root.clone(), files)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                txn.commit().await?;
                return Ok(());
            }
            Err(error) => return Err(DbErr::Custom(error.to_string())),
        }
    }
    for _ in 0..1024 {
        let Some(file) = scan
            .as_mut()
            .ok_or_else(invalid)?
            .1
            .next_entry()
            .await
            .map_err(|error| DbErr::Custom(error.to_string()))?
        else {
            *scan = None;
            break;
        };
        let key = file.file_name().to_string_lossy().into_owned();
        let temporary = key
            .strip_prefix('.')
            .and_then(|value| value.strip_suffix(".tmp"))
            .and_then(|value| value.split_once('.'))
            .is_some_and(|(id, nonce)| {
                content_path(&root, id).is_ok() && nonce.parse::<u128>().is_ok()
            });
        if !temporary && content_path(&root, &key).is_err() {
            continue;
        }
        if !file
            .file_type()
            .await
            .map_err(|error| DbErr::Custom(error.to_string()))?
            .is_file()
        {
            continue;
        }
        let referenced = !temporary
            && attachment::Entity::find()
                .select_only()
                .column(attachment::Column::Id)
                .filter(attachment::Column::ContentKey.eq(&key))
                .into_tuple::<String>()
                .one(&txn)
                .await?
                .is_some();
        if !referenced {
            tokio::fs::remove_file(file.path())
                .await
                .map_err(|error| DbErr::Custom(error.to_string()))?;
        }
    }
    txn.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests;

/// Internal original-result resolution under the caller's existing subject fence.
/// This path never renews LRU or acquires a nested transaction.
pub(crate) async fn restore_results(
    db: &impl ConnectionTrait,
    session: &mut PersistedAgentSession,
) -> Result<(), desk_agent_protocol::AgentError> {
    if !session.conversation.iter().any(|m| m.raw_result.is_some()) {
        return Ok(());
    }
    let run = session.conversation_id.clone();
    let actor = session.actor_id.clone();
    let device = session.device_id.clone();
    let root = content_root(db).await.map_err(|_| {
        desk_diagnose_core::conversation_attachment::invalid("Attachment storage is unavailable")
    })?;
    desk_diagnose_core::conversation_attachment::delivery::resolve_with(session, |id| {
        let run = &run;
        let actor = &actor;
        let device = &device;
        let root = &root;
        async move {
            let read = async {
                let row = attachment::Entity::find_by_id(id)
                    .filter(attachment::Column::ConversationId.eq(run))
                    .filter(attachment::Column::ActorId.eq(actor))
                    .filter(attachment::Column::DeviceId.eq(device))
                    .one(db)
                    .await?
                    .ok_or_else(invalid)?;
                let metadata = decode(&row)?;
                let content = if row.available {
                    read_content(root, row.content_key.as_deref()).await?
                } else {
                    vec![]
                };
                metadata
                    .verify(&content)
                    .map_err(|error| DbErr::Custom(error.message))?;
                Ok::<_, DbErr>(PreparedAttachment { metadata, content })
            }
            .await;
            read.map_err(|_| {
                desk_diagnose_core::conversation_attachment::invalid(
                    "Original attachment is unavailable",
                )
            })
        }
    })
    .await
}
