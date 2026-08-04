//! The host's durable record of which exec generations it has accepted.
//!
//! The manager cannot tell "the command never ran" from "the command ran and the
//! answer was lost" — only the host that spawned the process knows. This ledger is
//! where the host writes that down so the knowledge survives a crash.
//!
//! # Why it is on disk
//!
//! Both crash shapes lead to the same requirement. Under `ServiceDaemon` the worker
//! can die while the daemon lives on and restarts it, so the record has to outlive
//! the worker. In-process (`Default` / `DeskServer`) a panic takes the whole runtime
//! down, so the record has to outlive the process. Memory satisfies neither.
//!
//! # Two axes, deliberately
//!
//! - `task_id` is stable across retries of the same logical piece of work.
//! - `execution_generation` identifies one dispatch of it, and is what the ledger
//!   keys on. Retrying a task is legitimate; re-running a generation is not.
//!
//! The wire field carrying these has historically meant opposite things on the
//! agentic and fleet paths, which is exactly the confusion this split removes.
//! Never infer an axis from a field's name alone.
//!
//! # The tombstone is permanent
//!
//! A row's *result* is disposable and ages out. Its *existence* is not: the row is
//! the proof that this generation was already accepted, and dropping it would let
//! the same generation spawn a second time. It also lets the host answer "I never
//! accepted that" distinctly from "I accepted it and no longer hold the output" —
//! a distinction a manager that was offline for a day depends on. Any future TTL on
//! tombstones is an explicit decision to give up "a generation never runs twice",
//! not a housekeeping tweak.

use std::path::Path;
use std::time::Duration;

use desk_agent_protocol::exec_lifecycle::{ExecState, ExecStateReplyPayload};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectOptions, ConnectionTrait, Database, DatabaseConnection,
    DbErr, EntityTrait, QueryFilter, Schema, Set, TransactionTrait,
};

pub mod entity;

pub use entity::exec_ledger_entry::{self, State};

/// Outcome of reserving a generation, i.e. of asking "may I spawn this?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reservation {
    /// First sighting of this generation; the caller owns it and must spawn.
    Granted,
    /// This generation was accepted before. The caller must **not** spawn again;
    /// it should answer from what the ledger already knows.
    Duplicate(Box<exec_ledger_entry::Model>),
    /// The generation was seen before but carrying a different command. Refused
    /// without spawning: a replayed id must not become a vehicle for new content.
    FingerprintMismatch,
}

/// How an execution ended, as the host observed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    /// The process ran to completion (whatever its exit code) and the result is
    /// the host's own.
    Completed(String),
    /// The host could not establish what happened — it crashed inside the window
    /// between reserving and confirming the spawn, so it can neither claim the
    /// command ran nor claim it did not.
    Indeterminate,
    /// The spawn itself failed, so the command provably never started.
    SpawnFailed(String),
}

/// A durable exec ledger backed by its own SQLite file.
///
/// Deliberately not the signal database: that one is absent in a pure
/// `DeskServer` process, and coupling the ledger's lifecycle and schema to the
/// signalling module's would make "does this host have a ledger?" conditional on
/// the startup mode. The ledger must simply always exist.
///
/// A single connection with `synchronous = FULL`: a reservation that is not
/// durable before the spawn buys nothing, and the ledger is written a few times
/// per execution, so one connection is not a bottleneck. `synchronous` is a
/// connection-level pragma, so this cannot be varied per statement — hence the
/// whole file is durable rather than only the writes that need it.
#[derive(Clone)]
pub struct ExecLedger {
    db: DatabaseConnection,
}

impl ExecLedger {
    /// Open (creating if needed) the ledger under `config_dir` and initialize it.
    pub async fn open(config_dir: &str) -> Result<Self, DbErr> {
        let path = Path::new(config_dir).join("desk_exec_ledger.db");
        Self::connect(&sqlite_url(&path)).await
    }

    /// Open an in-memory ledger. Tests only — an in-memory ledger survives
    /// nothing, which is the one property the real one exists to provide.
    pub async fn open_in_memory() -> Result<Self, DbErr> {
        Self::connect("sqlite::memory:").await
    }

    async fn connect(url: &str) -> Result<Self, DbErr> {
        let mut opt = ConnectOptions::new(url.to_string());
        // One connection, so the pragmas below apply to every statement and the
        // daemon (the only writer) never contends with itself.
        opt.max_connections(1)
            .min_connections(1)
            .connect_timeout(Duration::from_secs(8))
            .sqlx_logging(false);
        let db = Database::connect(opt).await?;
        for pragma in ["PRAGMA journal_mode = WAL", "PRAGMA synchronous = FULL"] {
            db.execute_unprepared(pragma).await?;
        }
        initialize_schema(&db).await?;
        Ok(Self { db })
    }

    /// Claim the right to spawn `generation`, durably, **before** the spawn.
    ///
    /// Writing this after the spawn would leave the window it exists to close: a
    /// crash in between would lose the fact that a process was started, and the
    /// retry would start a second one.
    pub async fn reserve(
        &self,
        task_id: &str,
        generation: &str,
        plan_fingerprint: &str,
        containment_identity: Option<&str>,
    ) -> Result<Reservation, DbErr> {
        let now = chrono::Utc::now().naive_utc();
        let insert = exec_ledger_entry::Entity::insert(exec_ledger_entry::ActiveModel {
            execution_generation: Set(generation.to_string()),
            task_id: Set(task_id.to_string()),
            state: Set(State::Reserved.as_str().to_string()),
            plan_fingerprint: Set(plan_fingerprint.to_string()),
            containment_identity: Set(containment_identity.map(str::to_string)),
            result_json: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
        })
        .on_conflict(
            OnConflict::column(exec_ledger_entry::Column::ExecutionGeneration)
                .do_nothing()
                .to_owned(),
        );
        // `do_nothing` reports no insert as an error rather than a row count, so
        // the loser of the race is identified by reading the existing row back.
        match insert.exec(&self.db).await {
            Ok(_) => Ok(Reservation::Granted),
            Err(DbErr::RecordNotInserted) => {
                let Some(existing) = self.get(generation).await? else {
                    // The row lost the insert race and then vanished, which the
                    // permanent-tombstone rule forbids. Refuse rather than spawn.
                    return Ok(Reservation::FingerprintMismatch);
                };
                if existing.plan_fingerprint != plan_fingerprint {
                    return Ok(Reservation::FingerprintMismatch);
                }
                Ok(Reservation::Duplicate(Box::new(existing)))
            }
            Err(e) => Err(e),
        }
    }

    /// Record that the process is up, backfilling the containment identity that
    /// only exists once it has a pid.
    ///
    /// The gap between [`reserve`](Self::reserve) and this call is the window in
    /// which a crash leaves the host unable to say what happened; on macOS, where
    /// nothing can be registered before the spawn, that window is unavoidably the
    /// whole spawn.
    pub async fn mark_running(
        &self,
        generation: &str,
        containment_identity: Option<&str>,
    ) -> Result<bool, DbErr> {
        self.advance(
            generation,
            State::Running,
            containment_identity,
            None,
            &[State::Reserved],
        )
        .await
    }

    /// Record how the execution ended. Terminal states are final: a second report
    /// for the same generation is ignored rather than overwriting the first.
    pub async fn mark_terminal(&self, generation: &str, outcome: Terminal) -> Result<bool, DbErr> {
        let (state, result) = match outcome {
            Terminal::Completed(json) => (State::Terminal, Some(json)),
            Terminal::Indeterminate => (State::Indeterminate, None),
            Terminal::SpawnFailed(reason) => (State::SpawnFailed, Some(reason)),
        };
        self.advance(
            generation,
            state,
            None,
            result,
            &[State::Reserved, State::Running],
        )
        .await
    }

    /// Apply a state change under a transaction, refusing it unless the row is in
    /// one of `allowed_from`. The read and the write share the transaction so two
    /// concurrent reports cannot both see a non-terminal row and both apply.
    async fn advance(
        &self,
        generation: &str,
        to: State,
        containment_identity: Option<&str>,
        result_json: Option<String>,
        allowed_from: &[State],
    ) -> Result<bool, DbErr> {
        let generation = generation.to_string();
        let containment_identity = containment_identity.map(str::to_string);
        let allowed: Vec<String> = allowed_from
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();
        self.db
            .transaction::<_, bool, DbErr>(|txn| {
                Box::pin(async move {
                    let Some(row) = exec_ledger_entry::Entity::find_by_id(&generation)
                        .one(txn)
                        .await?
                    else {
                        return Ok(false);
                    };
                    if !allowed.contains(&row.state) {
                        return Ok(false);
                    }
                    let mut active: exec_ledger_entry::ActiveModel = row.into();
                    active.state = Set(to.as_str().to_string());
                    if let Some(id) = containment_identity {
                        active.containment_identity = Set(Some(id));
                    }
                    if let Some(json) = result_json {
                        active.result_json = Set(Some(json));
                    }
                    active.updated_at = Set(chrono::Utc::now().naive_utc());
                    active.update(txn).await?;
                    Ok(true)
                })
            })
            .await
            .map_err(|e| match e {
                sea_orm::TransactionError::Connection(e) => e,
                sea_orm::TransactionError::Transaction(e) => e,
            })
    }

    /// Read one generation's record, whether live or a tombstone.
    pub async fn get(&self, generation: &str) -> Result<Option<exec_ledger_entry::Model>, DbErr> {
        exec_ledger_entry::Entity::find_by_id(generation)
            .one(&self.db)
            .await
    }

    /// Answer "what do you know about this generation?" in the shared wire shape.
    ///
    /// This is the reply to both a state query and a cancel: an upstream asking
    /// either question wants the same fact back, so there is one answer type and
    /// one rule for reading it (keep asking until the state is settled).
    ///
    /// A generation with no row reports [`ExecState::Unknown`] — distinct from any
    /// settled state, because "I never accepted that" and "I accepted it and it
    /// failed" call for opposite responses upstream.
    pub async fn describe(&self, generation: &str) -> Result<ExecStateReplyPayload, DbErr> {
        let Some(row) = self.get(generation).await? else {
            return Ok(ExecStateReplyPayload {
                execution_generation: generation.to_string(),
                state: ExecState::Unknown,
                containment_identity: None,
                running_ms: None,
                detail: None,
                result_json: None,
            });
        };

        // An unreadable stored value is reported as indeterminate rather than
        // guessed at or reported as unknown: the row proves the generation was
        // accepted, so the one thing the host must not say is that it never saw it.
        let state = match State::parse(&row.state) {
            Some(State::Reserved) => ExecState::Reserved,
            Some(State::Running) => ExecState::Running,
            Some(State::Terminal) => ExecState::Terminal,
            Some(State::SpawnFailed) => ExecState::SpawnFailed,
            Some(State::Indeterminate) | None => ExecState::Indeterminate,
        };

        // Elapsed time is only meaningful while the command may still be running.
        let running_ms = (!state.is_settled()).then(|| {
            (chrono::Utc::now().naive_utc() - row.created_at)
                .num_milliseconds()
                .max(0) as u64
        });

        // `result_json` holds different things by state: a completed command's
        // recorded outcome, or a failed spawn's reason. The outcome is offered as a
        // replay source so an upstream that lost the live result frame can recover
        // it (rather than travelling only on the live result path); the spawn
        // reason is surfaced as human-readable detail.
        let detail = match state {
            ExecState::SpawnFailed => row.result_json.clone(),
            ExecState::Indeterminate => Some(
                "the host lost track of this execution and cannot say whether it ran".to_string(),
            ),
            _ => None,
        };
        let result_json = match state {
            ExecState::Terminal => row.result_json.clone(),
            _ => None,
        };

        Ok(ExecStateReplyPayload {
            execution_generation: generation.to_string(),
            state,
            containment_identity: row.containment_identity.clone(),
            running_ms,
            detail,
            result_json,
        })
    }

    /// Everything this host still considers in flight — what a restarting daemon
    /// reads to find the executions it lost track of.
    pub async fn in_flight(&self) -> Result<Vec<exec_ledger_entry::Model>, DbErr> {
        exec_ledger_entry::Entity::find()
            .filter(
                exec_ledger_entry::Column::State
                    .is_in([State::Reserved.as_str(), State::Running.as_str()]),
            )
            .all(&self.db)
            .await
    }

    /// Settle every execution still recorded as in flight as `indeterminate`.
    ///
    /// Called once at daemon startup. Anything the ledger still considers in
    /// flight belongs to a process that is gone, so the host cannot say whether it
    /// ran — and `indeterminate` says exactly that. Leaving the rows in `reserved`
    /// or `running` would be worse than useless: a manager asking about them would
    /// be told "not yet known" for ever, which reads as "still working on it".
    ///
    /// Returns what it settled, so the host can log which executions it lost track
    /// of across the restart.
    pub async fn abandon_in_flight(&self) -> Result<Vec<exec_ledger_entry::Model>, DbErr> {
        let lost = self.in_flight().await?;
        for row in &lost {
            self.mark_terminal(&row.execution_generation, Terminal::Indeterminate)
                .await?;
        }
        Ok(lost)
    }

    /// Drop stored results older than `retain`, leaving the rows themselves.
    ///
    /// Only `result_json` goes. The row stays forever so the generation can never
    /// be spawned again and so "I have no result for that" stays distinguishable
    /// from "I never accepted that".
    pub async fn forget_results_older_than(&self, retain: Duration) -> Result<u64, DbErr> {
        let cutoff = chrono::Utc::now().naive_utc()
            - chrono::Duration::from_std(retain).unwrap_or_else(|_| chrono::Duration::hours(24));
        let res = exec_ledger_entry::Entity::update_many()
            .col_expr(
                exec_ledger_entry::Column::ResultJson,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .filter(exec_ledger_entry::Column::UpdatedAt.lt(cutoff))
            .filter(exec_ledger_entry::Column::ResultJson.is_not_null())
            .exec(&self.db)
            .await?;
        Ok(res.rows_affected)
    }
}

/// Create the ledger at its current schema.
///
/// The desk server has not shipped with an older ledger schema, so maintaining a
/// migration history would only add upgrade machinery for a version no user can
/// have. Introduce migrations when a released schema actually needs upgrading.
async fn initialize_schema(db: &DatabaseConnection) -> Result<(), DbErr> {
    let schema = Schema::new(db.get_database_backend());
    let mut table = schema.create_table_from_entity(exec_ledger_entry::Entity);
    table.if_not_exists();
    db.execute(&table).await?;

    for mut index in schema.create_index_from_entity(exec_ledger_entry::Entity) {
        index.if_not_exists();
        db.execute(&index).await?;
    }
    Ok(())
}

fn sqlite_url(path: &Path) -> String {
    let path_str = path.to_string_lossy();
    let stripped = path_str.strip_prefix(r"\\?\").unwrap_or(&path_str);
    format!("sqlite://{}?mode=rwc", stripped.replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ledger() -> ExecLedger {
        ExecLedger::open_in_memory().await.unwrap()
    }

    /// A generation this host never accepted is reported as unknown, which an
    /// upstream must not confuse with any settled state: "I never ran that" and
    /// "I ran it and it failed" call for opposite responses.
    #[tokio::test]
    async fn an_unseen_generation_is_unknown_rather_than_settled() {
        let l = ledger().await;
        let reply = l.describe("never-seen").await.unwrap();
        assert_eq!(reply.state, ExecState::Unknown);
        assert!(!reply.state.is_settled());
        assert_eq!(reply.running_ms, None);
    }

    /// The query answers with the live state and how the host would reclaim the
    /// process tree, which is what a cancel needs to act on.
    #[tokio::test]
    async fn a_running_execution_reports_how_to_reclaim_it() {
        let l = ledger().await;
        l.reserve("task-1", "gen-1", "fp-1", None).await.unwrap();
        assert_eq!(
            l.describe("gen-1").await.unwrap().state,
            ExecState::Reserved
        );

        l.mark_running("gen-1", Some("pgid:4242")).await.unwrap();
        let reply = l.describe("gen-1").await.unwrap();
        assert_eq!(reply.state, ExecState::Running);
        assert_eq!(reply.containment_identity.as_deref(), Some("pgid:4242"));
        assert!(
            reply.running_ms.is_some(),
            "a live execution reports elapsed time"
        );
    }

    /// Once settled, elapsed time stops being reported — there is nothing still
    /// elapsing, and a number here would read as progress.
    #[tokio::test]
    async fn a_settled_execution_reports_no_elapsed_time() {
        let l = ledger().await;
        l.reserve("task-1", "gen-1", "fp-1", None).await.unwrap();
        l.mark_terminal("gen-1", Terminal::Completed("{}".into()))
            .await
            .unwrap();
        let reply = l.describe("gen-1").await.unwrap();
        assert_eq!(reply.state, ExecState::Terminal);
        assert!(reply.state.is_settled());
        assert_eq!(reply.running_ms, None);
    }

    /// A terminal reply carries the stored result verbatim, so an upstream that
    /// lost the live result frame can replay the answer instead of guessing. A
    /// still-running or unknown generation carries no result to replay.
    #[tokio::test]
    async fn a_terminal_reply_replays_the_stored_result() {
        let l = ledger().await;
        l.reserve("task-1", "gen-1", "fp-1", None).await.unwrap();
        assert_eq!(l.describe("gen-1").await.unwrap().result_json, None);

        l.mark_terminal("gen-1", Terminal::Completed("{\"status\":\"ok\"}".into()))
            .await
            .unwrap();
        assert_eq!(
            l.describe("gen-1").await.unwrap().result_json.as_deref(),
            Some("{\"status\":\"ok\"}")
        );

        // Once the result ages out, the tombstone remains but there is nothing to
        // replay — the reply is honest about no longer holding it.
        l.forget_results_older_than(Duration::ZERO).await.unwrap();
        let reply = l.describe("gen-1").await.unwrap();
        assert_eq!(reply.state, ExecState::Terminal);
        assert_eq!(reply.result_json, None);
    }

    /// A failed spawn hands back why, so an upstream can report it without having
    /// to fetch a result that does not exist.
    #[tokio::test]
    async fn a_failed_spawn_explains_itself() {
        let l = ledger().await;
        l.reserve("task-1", "gen-1", "fp-1", None).await.unwrap();
        l.mark_terminal("gen-1", Terminal::SpawnFailed("no such program".into()))
            .await
            .unwrap();
        let reply = l.describe("gen-1").await.unwrap();
        assert_eq!(reply.state, ExecState::SpawnFailed);
        assert_eq!(reply.detail.as_deref(), Some("no such program"));
    }

    /// A row whose stored state cannot be read still proves the generation was
    /// accepted, so the host reports indeterminate. Reporting it as unknown would
    /// invite a redispatch of a command that may already have run.
    #[tokio::test]
    async fn an_unreadable_state_is_never_reported_as_unseen() {
        let l = ledger().await;
        l.reserve("task-1", "gen-1", "fp-1", None).await.unwrap();
        l.db.execute_unprepared("UPDATE exec_ledger_entry SET state = 'nonsense'")
            .await
            .unwrap();
        let reply = l.describe("gen-1").await.unwrap();
        assert_eq!(reply.state, ExecState::Indeterminate);
        assert!(reply.state.is_settled());
    }

    /// The first reservation wins; a replay of the same generation is refused a
    /// spawn and handed what the ledger already knows.
    #[tokio::test]
    async fn a_generation_is_reserved_once() {
        let l = ledger().await;
        assert_eq!(
            l.reserve("task-1", "gen-1", "fp-1", Some("job-1"))
                .await
                .unwrap(),
            Reservation::Granted
        );
        match l.reserve("task-1", "gen-1", "fp-1", None).await.unwrap() {
            Reservation::Duplicate(row) => {
                assert_eq!(row.task_id, "task-1");
                assert_eq!(row.state, State::Reserved.as_str());
            }
            other => panic!("expected Duplicate, got {other:?}"),
        }
    }

    /// Retrying a task is legitimate: a new generation of the same task reserves
    /// cleanly. This is the axis split doing its job.
    #[tokio::test]
    async fn a_new_generation_of_the_same_task_is_allowed() {
        let l = ledger().await;
        l.reserve("task-1", "gen-1", "fp-1", None).await.unwrap();
        assert_eq!(
            l.reserve("task-1", "gen-2", "fp-1", None).await.unwrap(),
            Reservation::Granted
        );
    }

    /// A replayed generation carrying a different command is refused outright —
    /// neither spawned nor answered from the earlier record.
    #[tokio::test]
    async fn a_replayed_generation_cannot_smuggle_a_new_command() {
        let l = ledger().await;
        l.reserve("task-1", "gen-1", "fp-1", None).await.unwrap();
        assert_eq!(
            l.reserve("task-1", "gen-1", "fp-EVIL", None).await.unwrap(),
            Reservation::FingerprintMismatch
        );
    }

    /// The normal path: reserved → running (with the pid-derived identity filled
    /// in) → terminal, with the result readable afterwards.
    #[tokio::test]
    async fn lifecycle_records_the_containment_identity_and_result() {
        let l = ledger().await;
        l.reserve("task-1", "gen-1", "fp-1", Some("cgroup:/exec/gen-1"))
            .await
            .unwrap();
        assert!(l.mark_running("gen-1", Some("pid:4242")).await.unwrap());
        assert!(
            l.mark_terminal("gen-1", Terminal::Completed(r#"{"exit_code":0}"#.into()))
                .await
                .unwrap()
        );

        let row = l.get("gen-1").await.unwrap().unwrap();
        assert_eq!(row.state, State::Terminal.as_str());
        assert_eq!(row.containment_identity.as_deref(), Some("pid:4242"));
        assert!(row.result_json.unwrap().contains("exit_code"));
    }

    /// A terminal state is final: a second, contradicting report does not land.
    #[tokio::test]
    async fn a_terminal_state_is_not_overwritten() {
        let l = ledger().await;
        l.reserve("task-1", "gen-1", "fp-1", None).await.unwrap();
        l.mark_terminal("gen-1", Terminal::Completed(r#"{"exit_code":0}"#.into()))
            .await
            .unwrap();

        assert!(
            !l.mark_terminal("gen-1", Terminal::Indeterminate)
                .await
                .unwrap()
        );
        assert!(!l.mark_running("gen-1", None).await.unwrap());
        let row = l.get("gen-1").await.unwrap().unwrap();
        assert_eq!(row.state, State::Terminal.as_str());
        assert!(row.result_json.unwrap().contains("exit_code"));
    }

    /// A crash between reserving and confirming the spawn leaves a record the host
    /// can find again, and it says "unknown" rather than guessing either way.
    #[tokio::test]
    async fn an_interrupted_spawn_is_recoverable_and_admits_ignorance() {
        let l = ledger().await;
        l.reserve("task-1", "gen-1", "fp-1", Some("job-1"))
            .await
            .unwrap();

        let lost = l.in_flight().await.unwrap();
        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0].execution_generation, "gen-1");

        l.mark_terminal("gen-1", Terminal::Indeterminate)
            .await
            .unwrap();
        let row = l.get("gen-1").await.unwrap().unwrap();
        assert_eq!(row.state, State::Indeterminate.as_str());
        assert_eq!(row.result_json, None);
        assert!(l.in_flight().await.unwrap().is_empty());
    }

    /// A failed spawn is recorded as such — provably not executed, which is a
    /// stronger and more useful statement than "unknown".
    #[tokio::test]
    async fn a_failed_spawn_is_distinguishable_from_an_unknown_one() {
        let l = ledger().await;
        l.reserve("task-1", "gen-1", "fp-1", None).await.unwrap();
        l.mark_terminal("gen-1", Terminal::SpawnFailed("no such program".into()))
            .await
            .unwrap();
        let row = l.get("gen-1").await.unwrap().unwrap();
        assert_eq!(row.state, State::SpawnFailed.as_str());
    }

    /// Ageing out results must not age out the tombstone: the generation stays
    /// un-spawnable and stays distinguishable from one never seen.
    #[tokio::test]
    async fn ageing_out_a_result_keeps_the_tombstone() {
        let l = ledger().await;
        l.reserve("task-1", "gen-1", "fp-1", None).await.unwrap();
        l.mark_terminal("gen-1", Terminal::Completed(r#"{"exit_code":0}"#.into()))
            .await
            .unwrap();

        assert_eq!(
            l.forget_results_older_than(Duration::from_secs(0))
                .await
                .unwrap(),
            1
        );
        let row = l.get("gen-1").await.unwrap().unwrap();
        assert_eq!(row.result_json, None);
        assert_eq!(row.state, State::Terminal.as_str());
        // Still refuses a second spawn, and still says "I accepted this".
        assert!(matches!(
            l.reserve("task-1", "gen-1", "fp-1", None).await.unwrap(),
            Reservation::Duplicate(_)
        ));
        assert!(l.get("gen-unseen").await.unwrap().is_none());
    }

    /// The property everything else rests on: a reservation written before a spawn
    /// is still there after the process that wrote it is gone. Uses a real file,
    /// because an in-memory ledger cannot demonstrate this.
    #[tokio::test]
    async fn a_reservation_survives_the_process_that_wrote_it() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().to_string_lossy().to_string();

        {
            let l = ExecLedger::open(&config_dir).await.unwrap();
            l.reserve("task-1", "gen-1", "fp-1", Some("job-1"))
                .await
                .unwrap();
        } // the ledger — and in production its whole process — goes away here.

        let reopened = ExecLedger::open(&config_dir).await.unwrap();
        let lost = reopened.in_flight().await.unwrap();
        assert_eq!(lost.len(), 1, "the reservation did not survive the restart");
        assert_eq!(lost[0].execution_generation, "gen-1");
        assert_eq!(lost[0].containment_identity.as_deref(), Some("job-1"));
        // And it still refuses to let that generation run a second time.
        assert!(matches!(
            reopened
                .reserve("task-1", "gen-1", "fp-1", None)
                .await
                .unwrap(),
            Reservation::Duplicate(_)
        ));
    }

    /// After a restart, executions the host lost track of are settled as
    /// indeterminate rather than left looking like they are still progressing —
    /// and they still refuse a second spawn.
    #[tokio::test]
    async fn a_restart_settles_what_it_lost_track_of() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().to_string_lossy().to_string();
        {
            let l = ExecLedger::open(&config_dir).await.unwrap();
            l.reserve("task-1", "mid-spawn", "fp-1", None)
                .await
                .unwrap();
            l.reserve("task-2", "was-running", "fp-2", None)
                .await
                .unwrap();
            l.mark_running("was-running", Some("pgid:7")).await.unwrap();
            l.reserve("task-3", "finished", "fp-3", None).await.unwrap();
            l.mark_terminal("finished", Terminal::Completed("{}".into()))
                .await
                .unwrap();
        }

        let reopened = ExecLedger::open(&config_dir).await.unwrap();
        let lost = reopened.abandon_in_flight().await.unwrap();
        assert_eq!(lost.len(), 2, "both in-flight entries should be settled");

        for generation in ["mid-spawn", "was-running"] {
            let row = reopened.get(generation).await.unwrap().unwrap();
            assert_eq!(row.state, State::Indeterminate.as_str(), "{generation}");
        }
        // An execution that had already finished keeps its own answer.
        let finished = reopened.get("finished").await.unwrap().unwrap();
        assert_eq!(finished.state, State::Terminal.as_str());
        assert!(finished.result_json.is_some());

        assert!(reopened.in_flight().await.unwrap().is_empty());
        // Settled is still not re-runnable.
        assert!(matches!(
            reopened
                .reserve("task-1", "mid-spawn", "fp-1", None)
                .await
                .unwrap(),
            Reservation::Duplicate(_)
        ));
    }

    /// A young result is left alone by the sweep.
    #[tokio::test]
    async fn ageing_out_spares_recent_results() {
        let l = ledger().await;
        l.reserve("task-1", "gen-1", "fp-1", None).await.unwrap();
        l.mark_terminal("gen-1", Terminal::Completed("{}".into()))
            .await
            .unwrap();
        assert_eq!(
            l.forget_results_older_than(Duration::from_secs(3600))
                .await
                .unwrap(),
            0
        );
        assert!(l.get("gen-1").await.unwrap().unwrap().result_json.is_some());
    }
}
