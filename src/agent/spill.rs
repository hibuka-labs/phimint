//! Shell-output spill adapter: routes phi-kernel-tools' `SpillSink` into the
//! code engine's artifact store — the same `captures/` directory `code_cell`
//! spills into (M3c: adapters use store-level `spill`, no CellEvent), so a
//! shell overflow artifact is fetched exactly like a cell artifact.
//!
//! Mounting is late by design: tool registration runs before the code-exec
//! engine is assembled, so the sink starts empty and the engine mounts its
//! hub into it. Unmounted (no code-exec in this build, or the engine failed to
//! assemble) every spill fails — the shell tools then keep their plain
//! truncation markers, never a fake path.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use phi_agent::code_cell::ArtifactHub;
use phi_kernel_tools::spill::SpillSink;

/// Spill sink backed by the session's code-engine artifact hub.
#[derive(Default)]
pub struct HubSpillSink {
    hub: OnceLock<Arc<ArtifactHub>>,
}

impl HubSpillSink {
    /// Fresh, unmounted sink; mount the hub with [`mount`](Self::mount) once
    /// the engine is assembled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount the engine's artifact hub. First mount wins — a second engine
    /// sharing this sink keeps the first store (per-session engines open
    /// their own stores for their own spills).
    pub fn mount(&self, hub: Arc<ArtifactHub>) {
        let _ = self.hub.set(hub);
    }
}

impl SpillSink for HubSpillSink {
    fn spill(&self, kind: &str, body: &str) -> Result<PathBuf, String> {
        let hub = self
            .hub
            .get()
            .ok_or_else(|| "code engine not mounted".to_string())?;
        // Store-level spill (not `ArtifactHub::spill`): shell overflow is not
        // a cell event — it just shares the store, its `art_` number pool and
        // the `kind` discriminator.
        let artifact = hub.store().spill(kind, body)?;
        Ok(hub.store().path_of(&artifact.capture_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// protects: the adapter seam — a mounted hub's store receives the spill
    /// (body lands byte-exact at the returned path) and an unmounted sink
    /// fails closed with no path.
    /// fails_when: an unmounted spill fabricates a path, or a mounted spill's
    /// artifact is missing/different bytes.
    /// no_claim: first-mount-wins sharing across engines (policy) or the
    /// tools' marker text (phi-kernel-tools cards).
    #[test]
    fn hub_spill_sink_round_trips_and_fails_closed() {
        let sink = HubSpillSink::new();
        assert!(
            sink.spill("shell_output", "body").is_err(),
            "unmounted sink must fail, never fabricate a path"
        );

        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(ArtifactHub::new(tmp.path().join("captures")));
        sink.mount(hub);

        let path = sink.spill("shell_output", "full body").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "full body");
    }
}
