use super::*;
#[cfg(feature = "code-exec")]
use std::path::Path;

/// The code-exec engine's journal for this session's artifacts (M2/M3): one
/// `code_journal.jsonl` beside the session's own files.
#[cfg(feature = "code-exec")]
pub(super) fn code_exec_journal(scope: &SessionScope) -> Result<JournalHandle> {
    let journal_path = scope.session_dir().join("code_journal.jsonl");
    let journal = phi_agent::code_cell::FileJournal::open(&journal_path)
        .context("code-exec journal must open")?;
    Ok(JournalHandle::new(Box::new(journal)))
}

/// The engine's artifact store — `captures/` beside the journal (M3c, one
/// accounting: the journal records the cut, the store holds the body).
#[cfg(feature = "code-exec")]
pub(super) fn code_artifacts_dir(scope: &SessionScope) -> PathBuf {
    scope.session_dir().join("captures")
}

/// Handover gate for the startup probe's host executor.
///
/// The probe is built at agent-build time journaling to the *then* live
/// session's tree. A `/resume` switch before the host session's first cell
/// would hand that executor — frozen journal, frozen captures — to another
/// session's work (the 2026-11-04 misrouting: the resumed session's audit
/// stream landed in the abandoned bootstrap directory). `take_for` hands the
/// probe over only for the very tree it was built for; on a mismatch it
/// drops the probe and the caller builds a session-correct executor.
#[cfg(feature = "code-exec")]
struct ProbeHandover<T> {
    probe: std::sync::Mutex<Option<T>>,
    probe_dir: PathBuf,
}

#[cfg(feature = "code-exec")]
impl<T> ProbeHandover<T> {
    fn new(probe: T, probe_dir: PathBuf) -> Self {
        Self {
            probe: std::sync::Mutex::new(Some(probe)),
            probe_dir,
        }
    }

    /// The probe, if `session_dir` is the tree it was built for. A mismatch
    /// consumes (drops) the probe — its paths are wrong for good.
    fn take_for(&self, session_dir: &Path) -> Option<T> {
        let mut slot = self.probe.lock().unwrap_or_else(|e| e.into_inner());
        if self.probe_dir == session_dir {
            slot.take()
        } else {
            *slot = None;
            None
        }
    }
}

/// The root engine's session-id generator: process-unique numbers — a host
/// and its sub-agents are several engines in one process, and per-engine
/// counters handed every one of them a "session 1" — with the host's own
/// sessions stamped [`ROOT_SESSION_MARK`]. The code-cell factory reads the
/// stamp to tell the host's unrestricted executor apart from a sub-agent's
/// read-only one; sub-agent engines draw plain numbers from the same shared
/// counter.
pub(super) struct RootMarkedIds;

/// The external-id mark on sessions the host engine itself owns.
pub(super) const ROOT_SESSION_MARK: &str = "root";

impl SessionIdGenerator for RootMarkedIds {
    fn generate(&self) -> SessionId {
        let id = shared_session_ids().generate();
        SessionId::with_external_id(id.id, ROOT_SESSION_MARK)
    }
}

/// The code-cell factory (M2 tool road): one resident engine per session —
/// the host's own and every sub-agent's. The host's session runs `AllowAll`
/// (the step already passes phimint's approval gate and every effect
/// journals); a sub-agent's session is audit-sealed read-only — its cells may
/// read the disk but a write is refused at the seam (PEP 578 mediates `open`
/// into ReadFs/WriteFs), so write capability stays on the file tools where
/// the approval gate sees it. Journals: the host's own stays
/// `code_journal.jsonl`; a sub-agent's gets `code_journal-<id>.jsonl` beside
/// it. The host's executor is the one the startup probe already built — `open`
/// hands it over on the host session's first call instead of spawning a
/// second worker.
#[cfg(feature = "code-exec")]
pub(super) struct CellSessions {
    /// The live session's storage scope — journals and captures resolve
    /// through it at `open` time, so a `/resume` re-point moves every new
    /// session's streams (frozen copies kept writing the abandoned bootstrap
    /// session's trees after a switch, 2026-11-04).
    scope: Arc<SessionScope>,
    limits: Limits,
    face: String,
    /// The probe-built host executor, handed to the first root session — but
    /// only while the scope still points at the tree the probe journals to
    /// (see [`ProbeHandover`]).
    probe: Arc<ProbeHandover<CodeCellExecutor<PythonWorker>>>,
    /// The consent bridge for the host's own cells — a held device action
    /// reaches the human through it (`None` when the product has no
    /// interaction surface: holds stay holds).
    consent: Option<Arc<ConsentBridge>>,
}

#[cfg(feature = "code-exec")]
impl CellSessions {
    pub(super) fn new(
        scope: Arc<SessionScope>,
        limits: Limits,
        face: String,
        root: CodeCellExecutor<PythonWorker>,
        consent: Option<Arc<ConsentBridge>>,
    ) -> Self {
        let probe = Arc::new(ProbeHandover::new(root, scope.session_dir()));
        Self {
            scope,
            limits,
            face,
            probe,
            consent,
        }
    }

    /// Sessions the host engine itself owns — stamped by [`RootMarkedIds`].
    fn is_root(session: &SessionId) -> bool {
        session.external_id.as_deref() == Some(ROOT_SESSION_MARK)
    }

    /// `code_journal.jsonl` for the host, `code_journal-<id>.jsonl` for a
    /// sub-agent — one row stream per resident session.
    pub(super) fn journal_name(session: &SessionId) -> String {
        if Self::is_root(session) {
            "code_journal.jsonl".to_string()
        } else {
            format!("code_journal-{}.jsonl", session.id)
        }
    }

    /// `captures/` for the host, `captures-<id>/` for a sub-agent — the
    /// artifact store mirrors the journal's one-stream-per-session split.
    /// It must: every store mints its own `art_` ids from its own counter,
    /// so two stores sharing a directory could write the same name.
    fn captures_name(session: &SessionId) -> String {
        if Self::is_root(session) {
            "captures".to_string()
        } else {
            format!("captures-{}", session.id)
        }
    }
}

#[cfg(feature = "code-exec")]
impl SessionFactory<PythonWorker> for CellSessions {
    fn prompt_face(&self) -> String {
        self.face.clone()
    }

    fn limits(&self) -> Limits {
        self.limits
    }

    fn open(&self, session: &SessionId) -> SessionFuture<PythonWorker> {
        let root = Self::is_root(session);
        // Resolved at call time, not at factory construction: a `/resume`
        // switch may have re-pointed the scope since the agent was built.
        let session_dir = self.scope.session_dir();
        let journal_path = session_dir.join(Self::journal_name(session));
        let artifacts_path = session_dir.join(Self::captures_name(session));
        let limits = self.limits;
        let probe = Arc::clone(&self.probe);
        let consent = self.consent.clone();
        Box::pin(async move {
            if root {
                // The startup probe's executor IS the host's own — hand it
                // over instead of spawning a second worker, but only while it
                // still journals to this session's tree (see `ProbeHandover`).
                if let Some(executor) = probe.take_for(&session_dir) {
                    return Ok(executor);
                }
            }
            let journal = phi_agent::code_cell::FileJournal::open(&journal_path)
                .map_err(|e| AgentError::internal(format!("code-exec journal must open: {e}")))?;
            let builder = ExecutorBuilder::new(JournalHandle::new(Box::new(journal)))
                .with_limits(limits)
                .with_ns(PhiNsShell::builtin())
                .with_artifacts(artifacts_path);
            // Same face for every session this factory opens (the tool
            // description is factory-wide): a sub-agent's read-only policy
            // below refuses the `Device` calls fail-closed before the
            // sidecar's lazy wire even starts.
            //
            // The desk.* pack stays unmounted for now: the device road is
            // frozen while the product consolidates on the code road. The
            // hook is one line — `crate::desk::mount_desk(builder, &desk_shots)`.
            let builder = if root {
                // The host's own cells: the device pack's consent posture
                // (posture B) and the bridge that carries a held device
                // action to the human. Without the bridge a hold stays a
                // hold — fail-closed, never a silent run.
                let builder = builder.with_policy(ConsentPolicy::tool_road());
                match consent.as_ref() {
                    Some(bridge) => {
                        builder.with_consent(Arc::clone(bridge) as Arc<dyn InteractionBridge>)
                    }
                    None => builder,
                }
            } else {
                // Audit-sealed read-only cells for sub-agents: reads pass,
                // every write refuses fail-closed at the seam. Pure passes
                // too — the obs.* face is pure computation against the
                // engine's own store.
                builder.with_policy(
                    ClassPolicy::new(PolicyDecision::Deny)
                        .with(Capability::ReadFs, PolicyDecision::Allow)
                        .with(Capability::Pure, PolicyDecision::Allow),
                )
            };
            builder
                .build(move |registry| PythonWorker::spawn(registry, limits))
                .await
                .map_err(|e| AgentError::internal(format!("code-exec engine must assemble: {e}")))
        })
    }

    fn pinned(&self, session: &SessionId) -> bool {
        Self::is_root(session)
    }
}

/// The code road's engine, assembled before the prompt so its face can seed
/// the act section (提示面同源), attached to the agent builder once that
/// exists. One pack list serves both faces — `prompt_face()` and `build()`
/// read the same builder.
#[cfg(feature = "code-exec")]
pub(super) enum CodeRoadEngine {
    Tool {
        engine: ExecutorBuilder,
        limits: Limits,
        /// The consent bridge the host's own cells mount (the device
        /// pack's holds reach the human through it).
        consent: Option<Arc<ConsentBridge>>,
    },
    Act {
        engine: ExecutorBuilder,
        limits: Limits,
        bridge: Arc<ExecCtxBridge>,
    },
}

#[cfg(feature = "code-exec")]
impl CodeRoadEngine {
    pub(super) fn prompt_face(&self) -> String {
        match self {
            Self::Tool { engine, .. } | Self::Act { engine, .. } => engine.prompt_face(),
        }
    }
}

#[cfg(all(test, feature = "code-exec"))]
mod tests {
    use super::*;

    #[test]
    fn probe_hands_over_for_its_own_tree() {
        let handover = ProbeHandover::new("executor", PathBuf::from("/data/sessions/a"));
        assert_eq!(
            handover.take_for(Path::new("/data/sessions/a")),
            Some("executor")
        );
    }

    /// Regression (2026-11-04 incident): a `/resume` switch before the host
    /// session's first cell must NOT hand the probe — journaling to the
    /// abandoned bootstrap session's tree — to the resumed session's work.
    #[test]
    fn probe_is_dropped_on_tree_mismatch() {
        let handover = ProbeHandover::new("executor", PathBuf::from("/data/sessions/bootstrap"));
        assert_eq!(handover.take_for(Path::new("/data/sessions/resumed")), None);
        // The dropped probe is gone for good: a later switch back through a
        // consumed slot cannot resurrect a stale handover.
        assert_eq!(
            handover.take_for(Path::new("/data/sessions/bootstrap")),
            None
        );
    }
}
