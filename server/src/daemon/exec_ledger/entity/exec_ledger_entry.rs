use sea_orm::entity::prelude::*;

/// One dispatch of a command, as this host recorded it.
///
/// The primary key is the execution generation, not the task: a task may be
/// retried, and each retry is a separate row. That is what makes "this exact
/// dispatch already happened" answerable at all.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "exec_ledger_entry")]
pub struct Model {
    /// The dispatch this row is about. Unique by construction — being the primary
    /// key *is* the deduplication.
    #[sea_orm(primary_key, auto_increment = false)]
    pub execution_generation: String,
    /// The logical work this dispatch belongs to, stable across retries.
    #[sea_orm(indexed)]
    pub task_id: String,
    /// One of [`State`]'s string values.
    #[sea_orm(indexed)]
    pub state: String,
    /// Fingerprint of the command that was authorized, so a replayed generation
    /// carrying different content is refused instead of silently accepted.
    pub plan_fingerprint: String,
    /// How to find and reclaim the process tree: a logical identity (job name,
    /// cgroup path) written before the spawn, replaced by the pid-derived one
    /// afterwards. `None` where the platform offers nothing to register up front.
    pub containment_identity: Option<String>,
    /// The execution's own output, cleared once it ages out. The row survives it.
    pub result_json: Option<String>,
    pub created_at: ChronoDateTime,
    pub updated_at: ChronoDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// The states one dispatch moves through. Stored as strings so the schema stays
/// readable in a sqlite shell during an incident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Accepted and about to be spawned. A row left here after a restart means the
    /// host crashed mid-spawn.
    Reserved,
    /// The process is up.
    Running,
    /// Finished; `result_json` holds what happened.
    Terminal,
    /// The host lost track of it during the spawn and will not claim either way.
    Indeterminate,
    /// The spawn failed, so the command provably never started.
    SpawnFailed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Reserved => "reserved",
            State::Running => "running",
            State::Terminal => "terminal",
            State::Indeterminate => "indeterminate",
            State::SpawnFailed => "spawn_failed",
        }
    }

    /// Read back a state stored by [`State::as_str`].
    ///
    /// A value this host does not recognise is treated as unreadable rather than
    /// guessed at: claiming the wrong state about a command that may have run is
    /// the failure the ledger exists to prevent.
    pub fn parse(stored: &str) -> Option<Self> {
        match stored {
            "reserved" => Some(State::Reserved),
            "running" => Some(State::Running),
            "terminal" => Some(State::Terminal),
            "indeterminate" => Some(State::Indeterminate),
            "spawn_failed" => Some(State::SpawnFailed),
            _ => None,
        }
    }

    /// Whether no further transition is possible from this state.
    pub fn is_final(self) -> bool {
        matches!(
            self,
            State::Terminal | State::Indeterminate | State::SpawnFailed
        )
    }
}
