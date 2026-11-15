use jiff_sqlx::Timestamp;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, sqlx::Type, Serialize)]
#[sqlx(type_name = "ref_kind", rename_all = "snake_case")]
pub enum RefKind {
    Branch,
    Pull,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type, Serialize)]
#[sqlx(type_name = "run_state", rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Evaluating,
    Building,
    Succeeded,
    Failed,
    Superseded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type, Serialize)]
#[sqlx(type_name = "build_state", rename_all = "snake_case")]
pub enum BuildState {
    Queued,
    Running,
    Uploading,
    Succeeded,
    Failed,
    DepFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type, Serialize)]
#[sqlx(type_name = "attr_result", rename_all = "snake_case")]
pub enum AttrResult {
    EvalError,
    Cached,
    Succeeded,
    Failed,
    DepFailed,
}

impl AttrResult {
    pub fn is_failure(self) -> bool {
        matches!(self, Self::EvalError | Self::Failed | Self::DepFailed)
    }
}

impl AttrResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EvalError => "eval_error",
            Self::Cached => "cached",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::DepFailed => "dep_failed",
        }
    }
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Run {
    pub id: i64,
    pub repo: String,
    pub sha: String,
    pub jobset: String,
    pub attr: String,
    pub flake_ref: String,
    pub state: RunState,
    pub error: Option<String>,
    pub superseded_by: Option<i64>,
    pub execution: i32,
    pub evaluated_at: Option<Timestamp>,
    pub check_eval_id: Option<i64>,
    pub check_build_id: Option<i64>,
    pub report_seq: i64,
    pub finalize_seq: i64,
}

impl Run {
    pub fn short_sha(&self) -> &str {
        &self.sha[..self.sha.len().min(7)]
    }
}

/// A watched ref at the commit of a run.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Ref {
    pub kind: RefKind,
    pub name: String,
    pub pin: bool,
    pub issues: bool,
}

impl Ref {
    /// The refs whose head is the run's commit.
    pub async fn at(db: &sqlx::PgPool, run: &Run) -> sqlx::Result<Vec<Self>> {
        sqlx::query_as(
            "SELECT kind, name, pin, issues FROM refs WHERE repo = $1 AND sha = $2 \
             ORDER BY kind, name",
        )
        .bind(&run.repo)
        .bind(&run.sha)
        .fetch_all(db)
        .await
    }
}

/// Counts of a run's builds by state.
#[derive(Debug, Default, Serialize)]
pub struct Progress {
    pub queued: i64,
    pub running: i64,
    pub uploading: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub dep_failed: i64,
}

impl Progress {
    pub fn total(&self) -> i64 {
        self.queued + self.running + self.uploading + self.succeeded + self.failed + self.dep_failed
    }

    pub fn done(&self) -> i64 {
        self.succeeded + self.failed + self.dep_failed
    }

    pub async fn of_run(db: &sqlx::PgPool, run_id: i64) -> sqlx::Result<Self> {
        let rows: Vec<(BuildState, i64)> = sqlx::query_as(
            "SELECT b.state, count(*) FROM run_build_states b \
             WHERE b.run_id = $1 GROUP BY b.state",
        )
        .bind(run_id)
        .fetch_all(db)
        .await?;
        let mut progress = Self::default();
        for (state, count) in rows {
            *match state {
                BuildState::Queued => &mut progress.queued,
                BuildState::Running => &mut progress.running,
                BuildState::Uploading => &mut progress.uploading,
                BuildState::Succeeded => &mut progress.succeeded,
                BuildState::Failed => &mut progress.failed,
                BuildState::DepFailed => &mut progress.dep_failed,
            } = count;
        }
        Ok(progress)
    }
}

/// An attribute of a run with its result, live while the run is active.
#[derive(Debug, sqlx::FromRow)]
pub struct AttrRow {
    pub attr: String,
    pub drv_path: Option<String>,
    pub outputs: Option<sqlx::types::Json<std::collections::BTreeMap<String, Option<String>>>>,
    pub error: Option<String>,
    pub result: Option<AttrResult>,
    pub build_state: Option<BuildState>,
}

impl AttrRow {
    pub async fn of_run(db: &sqlx::PgPool, run_id: i64) -> sqlx::Result<Vec<Self>> {
        sqlx::query_as(
            "SELECT a.attr, a.drv_path, a.outputs, a.error, a.result, \
             b.state AS build_state \
             FROM run_attrs a \
             LEFT JOIN run_build_states b ON b.run_id = a.run_id AND b.drv_path = a.drv_path \
             WHERE a.run_id = $1 ORDER BY a.attr",
        )
        .bind(run_id)
        .fetch_all(db)
        .await
    }

    /// The final result, or what the result would be if the run ended now.
    pub fn status(&self) -> AttrStatus {
        if let Some(result) = self.result {
            return AttrStatus::Done(result);
        }
        if self.error.is_some() {
            return AttrStatus::Done(AttrResult::EvalError);
        }
        match self.build_state {
            None => AttrStatus::Done(AttrResult::Cached),
            Some(BuildState::Queued) => AttrStatus::Queued,
            Some(BuildState::Running) => AttrStatus::Running,
            Some(BuildState::Uploading) => AttrStatus::Uploading,
            Some(BuildState::Succeeded) => AttrStatus::Done(AttrResult::Succeeded),
            Some(BuildState::Failed) => AttrStatus::Done(AttrResult::Failed),
            Some(BuildState::DepFailed) => AttrStatus::Done(AttrResult::DepFailed),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttrStatus {
    Queued,
    Running,
    Uploading,
    Done(AttrResult),
}

impl AttrStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Uploading => "uploading",
            Self::Done(result) => result.as_str(),
        }
    }
}
