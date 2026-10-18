//! Transaction-bound reuse of a fully decoded, unchanged session row.
use super::*;
use crate::config::connection::DatabaseTransaction;
use std::ops::Deref;

pub(super) struct TxnValidatedSession<'t> {
    txn: &'t DatabaseTransaction,
    row: session_row::Model,
    session: Box<PersistedAgentSession>,
}

impl<'t> TxnValidatedSession<'t> {
    pub(super) fn from_row(
        txn: &'t DatabaseTransaction,
        row: session_row::Model,
    ) -> Result<Self, DbErr> {
        #[cfg(test)]
        crate::stack_probe::observe(&row.conversation_id, "decode");
        let session =
            Box::new(PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?);
        Ok(Self { txn, row, session })
    }

    pub(super) fn txn(&self) -> &'t DatabaseTransaction {
        self.txn
    }
    pub(super) fn session(&self) -> &PersistedAgentSession {
        &self.session
    }
    pub(super) fn version(&self) -> i64 {
        self.row.version
    }

    pub(super) fn reuse<'a>(
        &'a self,
        row: &session_row::Model,
    ) -> Result<&'a PersistedAgentSession, DbErr> {
        if row.id != self.row.id
            || row.conversation_id != self.row.conversation_id
            || row.actor_id != self.row.actor_id
            || row.device_id != self.row.device_id
            || row.version != self.row.version
            || row.lease_token != self.row.lease_token
            || row.state_json != self.row.state_json
        {
            return Err(invalid());
        }
        Ok(&self.session)
    }

    #[cfg(test)]
    pub(super) async fn recheck(&self) -> Result<session_row::Model, DbErr> {
        self.recheck_subject(
            &self.row.conversation_id,
            &self.row.actor_id,
            &self.row.device_id,
        )
        .await
    }

    pub(super) async fn recheck_subject(
        &self,
        conversation_id: &str,
        actor_id: &str,
        device_id: &str,
    ) -> Result<session_row::Model, DbErr> {
        let row = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(conversation_id))
            .filter(session_row::Column::ActorId.eq(actor_id))
            .filter(session_row::Column::DeviceId.eq(device_id))
            .filter(session_row::Column::Version.eq(self.row.version))
            .filter(session_row::Column::LeaseToken.eq(self.row.lease_token))
            .one(self.txn)
            .await?
            .ok_or_else(invalid)?;
        self.reuse(&row)?;
        Ok(row)
    }
    pub(super) async fn validate_parent(&self) -> Result<(), DbErr> {
        let row = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(&self.row.conversation_id))
            .filter(session_row::Column::ActorId.eq(&self.row.actor_id))
            .filter(session_row::Column::DeviceId.eq(&self.row.device_id))
            .filter(session_row::Column::Version.eq(self.row.version))
            .filter(session_row::Column::LeaseToken.eq(self.row.lease_token))
            .one(self.txn)
            .await?
            .ok_or_else(invalid)?;
        let session = self.reuse(&row)?;
        if !session.agent_role.is_main()
            || session.surface != AgentSessionSurface::AiAssistant
            || session.conversation_id != row.conversation_id
            || session.actor_id != row.actor_id
            || session.device_id != row.device_id
            || session.version != row.version
        {
            return Err(invalid());
        }
        Ok(())
    }
}

pub(super) enum DecodedSession<'a> {
    Owned(Box<PersistedAgentSession>),
    Borrowed(&'a PersistedAgentSession),
}
impl Deref for DecodedSession<'_> {
    type Target = PersistedAgentSession;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Owned(value) => value,
            Self::Borrowed(value) => value,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum SessionRead<'a> {
    Load(&'a DatabaseTransaction),
    Reuse(&'a TxnValidatedSession<'a>),
}
impl<'a> SessionRead<'a> {
    pub(super) fn txn(self) -> &'a DatabaseTransaction {
        match self {
            Self::Load(txn) => txn,
            Self::Reuse(proof) => proof.txn(),
        }
    }
    pub(super) fn decode(self, row: &session_row::Model) -> Result<DecodedSession<'a>, DbErr> {
        match self {
            Self::Load(_) => Ok(DecodedSession::Owned(Box::new(
                PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?,
            ))),
            Self::Reuse(proof) => Ok(DecodedSession::Borrowed(proof.reuse(row)?)),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum CallSession<'a> {
    Current {
        txn: &'a DatabaseTransaction,
        session: &'a PersistedAgentSession,
    },
    Validated(&'a TxnValidatedSession<'a>),
}
impl<'a> CallSession<'a> {
    pub(super) fn read(self) -> SessionRead<'a> {
        match self {
            Self::Current { txn, .. } => SessionRead::Load(txn),
            Self::Validated(proof) => SessionRead::Reuse(proof),
        }
    }
    pub(super) fn txn(self) -> &'a DatabaseTransaction {
        self.read().txn()
    }
    pub(super) fn session(self) -> &'a PersistedAgentSession {
        match self {
            Self::Current { session, .. } => session,
            Self::Validated(proof) => proof.session(),
        }
    }
    pub(super) fn version(self) -> i64 {
        match self {
            Self::Current { session, .. } => session.version,
            Self::Validated(proof) => proof.version(),
        }
    }
}
