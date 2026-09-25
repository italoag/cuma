//! Which orchestrator serves which working directory.
//!
//! An ACP client names a working directory in every `session/new`. Everything
//! an orchestrator does is rooted in one workspace — the agents' sessions, the
//! file-ownership ledger, checkpoints, worktrees, skills, memory, the runtime
//! database — so honouring the client's choice means an orchestrator per
//! directory, not one orchestrator told a different path.
//!
//! Orchestrators are built on first use and shared by every session in the
//! same directory. Sharing is not an optimisation: two sessions editing one
//! repository must go through one ownership ledger, or their writers could
//! collide exactly as ADR-011 describes.

use cuma_core::error::{MetaAgentError, Result};
use cuma_orchestrator::Orchestrator;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;

/// How many workspaces keep an orchestrator at once.
///
/// Beyond this, the least recently used idle one is dropped and rebuilt if a
/// session returns to it. One in use is never dropped, whatever the count.
const MAX_WORKSPACES: usize = 8;

/// A future building an orchestrator.
pub type BuildFuture = Pin<Box<dyn Future<Output = Result<Orchestrator>> + Send>>;

/// Builds the orchestrator for a workspace, given its canonical path.
pub type Builder = Arc<dyn Fn(PathBuf) -> BuildFuture + Send + Sync>;

struct Slot {
    path: PathBuf,
    orchestrator: Arc<OnceCell<Arc<Orchestrator>>>,
    last_used: u64,
}

/// The orchestrators behind an ACP server, one per working directory.
pub struct Workspaces {
    /// Where sessions with no usable directory of their own are served.
    default_path: PathBuf,
    default: Arc<Orchestrator>,
    /// `None`: every session is served by the default orchestrator.
    build: Option<Builder>,
    slots: Mutex<Vec<Slot>>,
    clock: std::sync::atomic::AtomicU64,
}

impl Workspaces {
    /// Serve every session from one orchestrator, whatever directory it
    /// names. For tests and embedders that manage workspaces themselves.
    pub fn fixed(orchestrator: Orchestrator) -> Self {
        Self {
            default_path: std::env::current_dir().unwrap_or_default(),
            default: Arc::new(orchestrator),
            build: None,
            slots: Mutex::new(Vec::new()),
            clock: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Serve each directory from its own orchestrator, built by `build`.
    ///
    /// `default` is the orchestrator already built for `default_path`, the
    /// directory CUMA was started in; a session there reuses it.
    pub fn per_directory(default_path: PathBuf, default: Orchestrator, build: Builder) -> Self {
        let default_path = std::fs::canonicalize(&default_path).unwrap_or(default_path);
        let default = Arc::new(default);
        let seeded = Slot {
            path: default_path.clone(),
            orchestrator: Arc::new(OnceCell::new_with(Some(Arc::clone(&default)))),
            last_used: 0,
        };
        Self {
            default_path,
            default,
            build: Some(build),
            slots: Mutex::new(vec![seeded]),
            clock: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Whether sessions get an orchestrator of their own directory.
    pub fn is_per_directory(&self) -> bool {
        self.build.is_some()
    }

    /// The orchestrator sessions fall back to.
    pub fn default_orchestrator(&self) -> Arc<Orchestrator> {
        Arc::clone(&self.default)
    }

    /// Check a directory a client asked for, and return its canonical form.
    ///
    /// ACP requires an absolute path. It must also exist and be a directory:
    /// an orchestrator rooted in a path that is not there would fail every
    /// task, later and less clearly.
    pub fn validate(&self, requested: &Path) -> Result<PathBuf> {
        if !self.is_per_directory() {
            // Recorded, not acted on: the fixed orchestrator has its own root.
            return Ok(requested.to_path_buf());
        }
        if !requested.is_absolute() {
            return Err(MetaAgentError::Configuration(format!(
                "the session's working directory must be an absolute path, got {}",
                requested.display()
            )));
        }
        let canonical = std::fs::canonicalize(requested).map_err(|err| {
            MetaAgentError::Configuration(format!(
                "cannot use {} as a working directory: {err}",
                requested.display()
            ))
        })?;
        if !canonical.is_dir() {
            return Err(MetaAgentError::Configuration(format!(
                "{} is not a directory",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    /// The orchestrator for a session's workspace, building it on first use.
    ///
    /// `None` — a session CUMA never recorded — gets the default one.
    pub async fn orchestrator(&self, workspace: Option<&Path>) -> Result<Arc<Orchestrator>> {
        let (Some(build), Some(workspace)) = (&self.build, workspace) else {
            return Ok(self.default_orchestrator());
        };

        let cell = self.slot(workspace);
        let orchestrator = cell
            .get_or_try_init(|| {
                let build = Arc::clone(build);
                let path = workspace.to_path_buf();
                async move {
                    tracing::info!(workspace = %path.display(), "building an orchestrator for a new workspace");
                    build(path).await.map(Arc::new)
                }
            })
            .await?;
        Ok(Arc::clone(orchestrator))
    }

    /// The slot for `workspace`, created if missing, marked as just used.
    fn slot(&self, workspace: &Path) -> Arc<OnceCell<Arc<Orchestrator>>> {
        let now = self
            .clock
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(slot) = slots.iter_mut().find(|s| s.path == workspace) {
            slot.last_used = now;
            return Arc::clone(&slot.orchestrator);
        }

        let cell = Arc::new(OnceCell::new());
        slots.push(Slot {
            path: workspace.to_path_buf(),
            orchestrator: Arc::clone(&cell),
            last_used: now,
        });
        Self::evict(&mut slots, &self.default_path);
        cell
    }

    /// Drop idle orchestrators, least recently used first, until within the
    /// limit. Never the default, never one being built, never one in use.
    fn evict(slots: &mut Vec<Slot>, default_path: &Path) {
        while slots.len() > MAX_WORKSPACES {
            let idle = slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.path != default_path)
                .filter(|(_, s)| {
                    // Held only by this slot: no session is running on it.
                    Arc::strong_count(&s.orchestrator) == 1
                        && s.orchestrator
                            .get()
                            .is_some_and(|o| Arc::strong_count(o) == 1)
                })
                .min_by_key(|(_, s)| s.last_used)
                .map(|(index, _)| index);
            let Some(index) = idle else {
                return;
            };
            let dropped = slots.remove(index);
            tracing::info!(workspace = %dropped.path.display(), "dropping an idle workspace's orchestrator");
        }
    }

    /// How many workspaces currently hold an orchestrator.
    pub fn len(&self) -> usize {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|s| s.orchestrator.get().is_some())
            .count()
    }

    /// Whether no workspace holds an orchestrator.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct NoPlanner;

    #[async_trait::async_trait]
    impl cuma_core::ports::Planner for NoPlanner {
        async fn plan(
            &self,
            _goal: &str,
            _context: &cuma_core::ports::PlanningContext,
        ) -> Result<cuma_core::TaskGraph> {
            Ok(cuma_core::TaskGraph::new())
        }
    }

    fn orchestrator(root: &Path) -> Orchestrator {
        Orchestrator::new(
            cuma_config::Config::default(),
            Arc::new(NoPlanner),
            root.to_path_buf(),
        )
    }

    fn counting_builder(built: Arc<AtomicUsize>) -> Builder {
        Arc::new(move |path: PathBuf| {
            let built = Arc::clone(&built);
            Box::pin(async move {
                built.fetch_add(1, Ordering::SeqCst);
                Ok(orchestrator(&path))
            })
        })
    }

    #[tokio::test]
    async fn each_directory_gets_its_own_orchestrator_built_once() {
        let home = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let built = Arc::new(AtomicUsize::new(0));
        let pool = Workspaces::per_directory(
            home.path().to_path_buf(),
            orchestrator(home.path()),
            counting_builder(Arc::clone(&built)),
        );

        let path = pool.validate(a.path()).unwrap();
        let (first, second) = tokio::join!(
            pool.orchestrator(Some(&path)),
            pool.orchestrator(Some(&path))
        );
        let (first, second) = (first.unwrap(), second.unwrap());

        assert!(
            Arc::ptr_eq(&first, &second),
            "sessions in one directory share it"
        );
        assert_eq!(
            built.load(Ordering::SeqCst),
            1,
            "built once, even when asked twice at once"
        );
        assert_eq!(
            first.workspace(),
            path.as_path(),
            "rooted where the client asked"
        );
        assert!(!Arc::ptr_eq(&first, &pool.default_orchestrator()));
    }

    #[tokio::test]
    async fn the_directory_cuma_started_in_reuses_the_orchestrator_it_started_with() {
        let home = tempfile::tempdir().unwrap();
        let built = Arc::new(AtomicUsize::new(0));
        let pool = Workspaces::per_directory(
            home.path().to_path_buf(),
            orchestrator(home.path()),
            counting_builder(Arc::clone(&built)),
        );

        let path = pool.validate(home.path()).unwrap();
        let served = pool.orchestrator(Some(&path)).await.unwrap();
        assert!(Arc::ptr_eq(&served, &pool.default_orchestrator()));
        assert_eq!(built.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn relative_missing_and_non_directory_paths_are_refused() {
        let home = tempfile::tempdir().unwrap();
        let pool = Workspaces::per_directory(
            home.path().to_path_buf(),
            orchestrator(home.path()),
            counting_builder(Arc::default()),
        );
        let file = home.path().join("file.txt");
        std::fs::write(&file, "x").unwrap();

        assert!(pool.validate(Path::new("relative/dir")).is_err());
        assert!(pool.validate(&home.path().join("missing")).is_err());
        assert!(pool.validate(&file).is_err());
        assert!(pool.validate(home.path()).is_ok());
    }

    #[tokio::test]
    async fn a_failed_build_is_reported_and_retried_next_time() {
        let home = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let build: Builder = Arc::new(move |path: PathBuf| {
            let attempt = counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if attempt == 0 {
                    Err(MetaAgentError::Configuration("no agents".into()))
                } else {
                    Ok(orchestrator(&path))
                }
            })
        });
        let pool =
            Workspaces::per_directory(home.path().to_path_buf(), orchestrator(home.path()), build);
        let path = pool.validate(a.path()).unwrap();

        assert!(pool.orchestrator(Some(&path)).await.is_err());
        assert!(
            pool.orchestrator(Some(&path)).await.is_ok(),
            "a fixed configuration is picked up"
        );
    }

    #[tokio::test]
    async fn idle_workspaces_are_dropped_beyond_the_limit_but_busy_ones_are_kept() {
        let home = tempfile::tempdir().unwrap();
        let pool = Workspaces::per_directory(
            home.path().to_path_buf(),
            orchestrator(home.path()),
            counting_builder(Arc::default()),
        );

        let dirs: Vec<_> = (0..MAX_WORKSPACES + 3)
            .map(|_| tempfile::tempdir().unwrap())
            .collect();
        // The first one stays in use throughout.
        let busy_path = pool.validate(dirs[0].path()).unwrap();
        let busy = pool.orchestrator(Some(&busy_path)).await.unwrap();

        for dir in &dirs[1..] {
            let path = pool.validate(dir.path()).unwrap();
            pool.orchestrator(Some(&path)).await.unwrap();
        }

        assert!(
            pool.len() <= MAX_WORKSPACES + 1,
            "bounded, got {}",
            pool.len()
        );
        let again = pool.orchestrator(Some(&busy_path)).await.unwrap();
        assert!(
            Arc::ptr_eq(&busy, &again),
            "an orchestrator in use is never replaced"
        );
    }

    #[tokio::test]
    async fn a_fixed_pool_serves_every_directory_from_one_orchestrator() {
        let pool = Workspaces::fixed(orchestrator(Path::new("/")));
        let served = pool
            .orchestrator(Some(Path::new("/anywhere")))
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&served, &pool.default_orchestrator()));
    }
}
