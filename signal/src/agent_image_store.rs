//! Subject-bound screenshot storage, independent of conversation JSON.
use crate::entity::{agent_image_attachment as image, agent_session};
use desk_diagnose_core::{
    conversation_image::{ImageAttachment, MAX_SESSION_IMAGE_BYTES},
    session::PersistedAgentSession,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, ExprTrait, QueryFilter,
    QueryOrder, QuerySelect, Set, sea_query::Expr,
};

fn invalid() -> DbErr {
    DbErr::Custom("Screenshot attachment is missing, inaccessible, or invalid".into())
}
fn decode(row: &image::Model) -> Result<ImageAttachment, DbErr> {
    serde_json::from_str(&row.metadata_json).map_err(|_| invalid())
}
fn safe_id(id: &str) -> bool {
    id.strip_prefix("visual-")
        .is_some_and(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()))
}

pub async fn store(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    attachment: &ImageAttachment,
    pixels: &[u8],
) -> Result<(), DbErr> {
    attachment.restore(pixels).map_err(|_| invalid())?;
    if !safe_id(&attachment.frame.evidence_id)
        || attachment.frame.conversation_id != session.conversation_id
        || attachment.frame.device_id != session.device_id
    {
        return Err(invalid());
    }
    let image_root = root(db).await?;
    let txn = crate::db::begin_write(&db, crate::entity::agent_session::Entity).await?;
    // First statement reserves the writer / locks the parent before quota reads.
    let locked = agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::Version,
            Expr::col(agent_session::Column::Version),
        )
        .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
        .filter(agent_session::Column::ActorId.eq(&session.actor_id))
        .filter(agent_session::Column::DeviceId.eq(&session.device_id))
        .filter(agent_session::Column::Version.eq(session.version))
        .filter(agent_session::Column::LeaseToken.eq(session.lease_token as i64))
        .exec(&txn)
        .await?;
    if locked.rows_affected != 1 {
        return Err(invalid());
    }
    if let Some(existing) = image::Entity::find_by_id(&attachment.frame.evidence_id)
        .one(&txn)
        .await?
    {
        if existing.deleted || decode(&existing)?.frame != attachment.frame {
            return Err(invalid());
        }
        let stored_pixels = tokio::fs::read(image_root.join(&existing.id))
            .await
            .map_err(|_| invalid())?;
        decode(&existing)?
            .restore(&stored_pixels)
            .map_err(|_| invalid())?;
        txn.commit().await?;
        return Ok(());
    }
    let total = image::Entity::find()
        .select_only()
        .column_as(image::Column::SizeBytes.sum(), "total")
        .filter(image::Column::ConversationId.eq(&session.conversation_id))
        .filter(image::Column::Deleted.eq(false))
        .into_tuple::<Option<i64>>()
        .one(&txn)
        .await?
        .flatten()
        .unwrap_or(0);
    if total.saturating_add(pixels.len() as i64) > MAX_SESSION_IMAGE_BYTES {
        return Err(DbErr::Custom("Conversation screenshot quota exceeded; delete unwanted attachments before capturing again".into()));
    }
    write_pixels(image_root, &attachment.frame.evidence_id, pixels).await?;
    image::ActiveModel {
        id: Set(attachment.frame.evidence_id.clone()),
        conversation_id: Set(session.conversation_id.clone()),
        actor_id: Set(session.actor_id.clone()),
        device_id: Set(session.device_id.clone()),
        tool_call_id: Set(attachment.frame.tool_call_id.clone()),
        metadata_json: Set(serde_json::to_string(attachment).map_err(|_| invalid())?),
        size_bytes: Set(pixels.len() as i64),
        created_at_unix_ms: Set(chrono::Utc::now().timestamp_millis()),
        deleted: Set(false),
    }
    .insert(&txn)
    .await?;
    txn.commit().await?;
    Ok(())
}

/// Parent existence and exact actor/device binding are checked on every access.
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
    if session.surface != desk_diagnose_core::session::AgentSessionSurface::DeviceAssistant
        || session.conversation_id != run
        || session.actor_id != actor
        || session.device_id != row.device_id
    {
        return Err(invalid());
    }
    Ok(row)
}

pub async fn list_records(
    db: &DatabaseConnection,
    run: &str,
    actor: &str,
    before: Option<&str>,
) -> Result<Vec<ImageAttachment>, DbErr> {
    let parent = subject(db, run, actor).await?;
    let mut query = image::Entity::find()
        .select_only()
        .column(image::Column::MetadataJson)
        .filter(image::Column::ConversationId.eq(run))
        .filter(image::Column::ActorId.eq(actor))
        .filter(image::Column::DeviceId.eq(parent.device_id))
        .filter(image::Column::Deleted.eq(false));
    if let Some(before) = before {
        let cursor = image::Entity::find_by_id(before)
            .select_only()
            .column(image::Column::CreatedAtUnixMs)
            .filter(image::Column::ConversationId.eq(run))
            .filter(image::Column::ActorId.eq(actor))
            .into_tuple::<i64>()
            .one(db)
            .await?
            .ok_or_else(invalid)?;
        query = query.filter(
            sea_orm::Condition::any()
                .add(image::Column::CreatedAtUnixMs.lt(cursor))
                .add(
                    sea_orm::Condition::all()
                        .add(image::Column::CreatedAtUnixMs.eq(cursor))
                        .add(image::Column::Id.lt(before)),
                ),
        );
    }
    let rows = query
        .order_by_desc(image::Column::CreatedAtUnixMs)
        .order_by_desc(image::Column::Id)
        .limit(32)
        .into_tuple::<String>()
        .all(db)
        .await?;
    rows.into_iter()
        .map(|raw| serde_json::from_str::<ImageAttachment>(&raw).map_err(|_| invalid()))
        .collect()
}

pub async fn read(
    db: &DatabaseConnection,
    run: &str,
    actor: &str,
    id: Option<&str>,
    call: Option<&str>,
) -> Result<(ImageAttachment, Vec<u8>), DbErr> {
    let parent = subject(db, run, actor).await?;
    let mut query = image::Entity::find()
        .filter(image::Column::ConversationId.eq(run))
        .filter(image::Column::ActorId.eq(actor))
        .filter(image::Column::DeviceId.eq(parent.device_id));
    if let Some(id) = id {
        query = query.filter(image::Column::Id.eq(id));
    } else if let Some(call) = call {
        query = query.filter(image::Column::ToolCallId.eq(call));
    } else {
        return Err(invalid());
    }
    let mut rows = query.limit(2).all(db).await?;
    // Some model protocols reuse call IDs across turns. Never return an
    // arbitrary screenshot for an ambiguous historical call reference.
    if rows.len() != 1 {
        return Err(invalid());
    }
    let row = rows.pop().ok_or_else(invalid)?;
    if row.deleted {
        return Err(invalid());
    }
    let attachment = decode(&row)?;
    let pixels = tokio::fs::read(root(db).await?.join(&row.id))
        .await
        .map_err(|_| invalid())?;
    attachment.restore(&pixels).map_err(|_| invalid())?;
    Ok((attachment, pixels))
}

pub async fn delete(
    db: &DatabaseConnection,
    run: &str,
    actor: &str,
    id: &str,
) -> Result<(), DbErr> {
    let parent = subject(db, run, actor).await?;
    // Tombstone is authoritative. GC can be delayed without allowing new reads.
    let result = image::Entity::update_many()
        .col_expr(image::Column::Deleted, Expr::value(true))
        .filter(image::Column::Id.eq(id))
        .filter(image::Column::ConversationId.eq(run))
        .filter(image::Column::ActorId.eq(actor))
        .filter(image::Column::DeviceId.eq(parent.device_id))
        .exec(db)
        .await?;
    if result.rows_affected != 1 {
        return Err(invalid());
    }
    Ok(())
}

/// Idempotent cleanup: remove bytes before metadata, tolerate another sweeper.
pub async fn cleanup(db: &DatabaseConnection) -> Result<(), DbErr> {
    let rows = image::Entity::find()
        .select_only()
        .column(image::Column::Id)
        .filter(
            sea_orm::Condition::any()
                .add(
                    sea_orm::Condition::all()
                        .add(image::Column::Deleted.eq(true))
                        .add(image::Column::SizeBytes.gt(0)),
                )
                .add(
                    Expr::col(image::Column::ConversationId).not_in_subquery(
                        sea_orm::sea_query::Query::select()
                            .column(agent_session::Column::ConversationId)
                            .from(agent_session::Entity)
                            .to_owned(),
                    ),
                ),
        )
        .limit(128)
        .column(image::Column::ConversationId)
        .column(image::Column::Deleted)
        .order_by_asc(image::Column::CreatedAtUnixMs)
        .into_tuple::<(String, String, bool)>()
        .all(db)
        .await?;
    for (id, run, deleted) in rows {
        let orphan = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&run))
            .one(db)
            .await?
            .is_none();
        if deleted || orphan {
            match tokio::fs::remove_file(root(db).await?.join(&id)).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(DbErr::Custom(format!("Screenshot cleanup failed: {e}"))),
            }
            if orphan {
                image::Entity::delete_by_id(id).exec(db).await?;
            } else {
                // Keep the tombstone until the parent is removed: replaying an
                // old tool result must not resurrect an owner-deleted image.
                image::Entity::update_many()
                    .col_expr(image::Column::SizeBytes, Expr::value(0_i64))
                    .filter(image::Column::Id.eq(id))
                    .filter(image::Column::Deleted.eq(true))
                    .exec(db)
                    .await?;
            }
        }
    }
    cleanup_orphan_files(db).await?;
    Ok(())
}

async fn root(db: &DatabaseConnection) -> Result<std::path::PathBuf, DbErr> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
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
            "Screenshot storage requires a file-backed database".into(),
        ));
    }
    Ok(std::path::Path::new(&path)
        .parent()
        .ok_or_else(invalid)?
        .join("assistant-images"))
}

async fn write_pixels(root: std::path::PathBuf, id: &str, pixels: &[u8]) -> Result<(), DbErr> {
    use tokio::io::AsyncWriteExt;
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|e| DbErr::Custom(e.to_string()))?;
    let temporary = root.join(format!("upload-{}", uuid::Uuid::new_v4()));
    let result = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary).await?;
        file.write_all(pixels).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, root.join(id)).await?;
        #[cfg(unix)]
        tokio::task::spawn_blocking(move || std::fs::File::open(root)?.sync_all())
            .await
            .map_err(std::io::Error::other)??;
        Ok::<(), std::io::Error>(())
    }
    .await;
    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(DbErr::Custom(format!("Screenshot storage failed: {e}")));
    }
    Ok(())
}

async fn cleanup_orphan_files(db: &DatabaseConnection) -> Result<(), DbErr> {
    let root = root(db).await?;
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(DbErr::Custom(e.to_string())),
    };
    let mut examined = 0;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| DbErr::Custom(e.to_string()))?
    {
        examined += 1;
        if examined > 1024 {
            break;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        if !safe_id(&id) && !id.starts_with("upload-") {
            continue;
        }
        let metadata = entry
            .metadata()
            .await
            .map_err(|e| DbErr::Custom(e.to_string()))?;
        if !metadata.is_file()
            || metadata
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_none_or(|age| age.as_secs() < 3600)
        {
            continue;
        }
        if image::Entity::find_by_id(&id).one(db).await?.is_none() {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Database, Schema};
    fn fixture() -> (PersistedAgentSession, ImageAttachment, Vec<u8>) {
        use desk_agent_protocol::{AgentScope, ExecutionMode, data_lineage::*};
        use desk_diagnose_core::{
            chat::ChatMessage, image_input::validate_image_data_url, session::AgentSessionSurface,
        };
        use sha2::{Digest, Sha256};
        let scope = AgentScope {
            granted: vec![],
            mode: ExecutionMode::SuggestOnly,
            expires_at: None,
            policy_name: None,
        };
        let mut session =
            PersistedAgentSession::new("conversation", "owner", "device", 1, scope.clone(), "now");
        session.adopt_client_metadata(Some("client"), AgentSessionSurface::DeviceAssistant);
        session.input_revision = 1;
        session.latest_input_seq = 1;
        session.begin_focus_epoch(1, vec![]).unwrap();
        session
            .begin_turn("turn", Some("request".into()), None, 1, scope, "now")
            .unwrap();
        let url = "data:image/png;base64,AQID";
        let mut message =
            ChatMessage::tool_result("result", "call", "screen captured").with_image(url);
        let payload = desk_diagnose_core::model_egress::message_content_bytes(&message).unwrap();
        message.data_envelope = Some(DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: "envelope".into(),
            content: ContentRef::EphemeralObservation {
                observation_id: "observation".into(),
                size_bytes: payload.len() as u64,
                expires_at_unix_ms: 600_000,
            },
            provenance: DataProvenance {
                source_provider_id: "screen.current".into(),
                source_tool_name: "read_current_screen".into(),
                source_object_id: None,
                source_envelope_ids: vec![],
            },
            digest_sha256: format!("{:x}", Sha256::digest(&payload)),
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: vec![DestinationIdentity::Model {
                connection_id: "model".into(),
                connection_revision: 1,
                model_id: "visual".into(),
                profile_revision: 1,
            }],
            retention: RetentionBoundary {
                expires_at_unix_ms: Some(600_000),
                delete_with_run: true,
            },
        });
        session.conversation.push(message);
        let frame = desk_diagnose_core::visual_evidence::record_live_observation(
            &mut session,
            "call",
            url,
            &validate_image_data_url(url).unwrap(),
        )
        .unwrap();
        let (attachment, pixels) = ImageAttachment::prepare(&session, &frame, None).unwrap();
        (session, attachment, pixels)
    }

    #[tokio::test]
    async fn durable_images_enforce_subject_fencing_and_deletion() {
        let directory =
            std::env::temp_dir().join(format!("assistant-images-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let db = Database::connect(format!(
            "sqlite://{}?mode=rwc",
            directory.join("test.db").display()
        ))
        .await
        .unwrap();
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(agent_session::Entity))
            .await
            .unwrap();
        db.execute(&schema.create_table_from_entity(image::Entity))
            .await
            .unwrap();
        let (session, attachment, pixels) = fixture();
        agent_session::ActiveModel {
            conversation_id: Set(session.conversation_id.clone()),
            actor_id: Set(session.actor_id.clone()),
            device_id: Set(session.device_id.clone()),
            state_json: Set(session.encode_json_for_storage().unwrap()),
            version: Set(session.version),
            lease_token: Set(session.lease_token as i64),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        store(&db, &session, &attachment, &pixels).await.unwrap();
        store(&db, &session, &attachment, &pixels).await.unwrap();
        let (loaded, bytes) = read(&db, "conversation", "owner", None, Some("call"))
            .await
            .unwrap();
        assert_eq!(bytes, pixels);
        assert_eq!(loaded.frame, attachment.frame);
        assert!(
            read(&db, "conversation", "other-owner", None, Some("call"))
                .await
                .is_err()
        );
        assert!(
            read(&db, "another-session", "owner", None, Some("call"))
                .await
                .is_err()
        );
        assert_eq!(
            list(&db, "conversation", "owner", None)
                .await
                .unwrap()
                .len(),
            1
        );
        let mut stale = session.clone();
        stale.version += 1;
        assert!(store(&db, &stale, &attachment, &pixels).await.is_err());
        let mut second = attachment.clone();
        second.frame.evidence_id = format!("visual-{}", "b".repeat(64));
        if let Some(desk_agent_protocol::data_lineage::ContentRef::Artifact {
            artifact_id, ..
        }) = &mut second.frame.content
        {
            *artifact_id = second.frame.evidence_id.clone();
        }
        image::Entity::update_many()
            .col_expr(
                image::Column::SizeBytes,
                Expr::value(MAX_SESSION_IMAGE_BYTES),
            )
            .filter(image::Column::Id.eq(&attachment.frame.evidence_id))
            .exec(&db)
            .await
            .unwrap();
        assert!(
            store(&db, &session, &second, &pixels)
                .await
                .unwrap_err()
                .to_string()
                .contains("quota")
        );
        image::Entity::update_many()
            .col_expr(image::Column::SizeBytes, Expr::value(pixels.len() as i64))
            .filter(image::Column::Id.eq(&attachment.frame.evidence_id))
            .exec(&db)
            .await
            .unwrap();
        store(&db, &session, &second, &pixels).await.unwrap();
        let page = list(&db, "conversation", "owner", None).await.unwrap();
        assert_eq!(page.len(), 2);
        let older = list(&db, "conversation", "owner", Some(&page[0].evidence_id))
            .await
            .unwrap();
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].evidence_id, page[1].evidence_id);

        assert!(
            read(&db, "conversation", "owner", None, Some("call"))
                .await
                .is_err()
        );
        assert!(
            read(
                &db,
                "conversation",
                "owner",
                Some(&second.frame.evidence_id),
                None
            )
            .await
            .is_ok()
        );
        delete(&db, "conversation", "owner", &attachment.frame.evidence_id)
            .await
            .unwrap();
        assert!(
            read(&db, "conversation", "owner", None, Some("call"))
                .await
                .is_err()
        );
        cleanup(&db).await.unwrap();
        cleanup(&db).await.unwrap();
        assert_eq!(
            list(&db, "conversation", "owner", None)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(store(&db, &session, &attachment, &pixels).await.is_err());
        let tombstone = image::Entity::find_by_id(&attachment.frame.evidence_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(tombstone.deleted);
        assert_eq!(tombstone.size_bytes, 0);
        agent_session::Entity::delete_many()
            .exec(&db)
            .await
            .unwrap();
        cleanup(&db).await.unwrap();
        assert!(image::Entity::find().one(&db).await.unwrap().is_none());
        db.close().await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
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
        .map(|a| a.frame)
        .collect())
}
