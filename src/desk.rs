//! `desk.*` on the code-cell engine: mount the desktop capability pack
//! (`phi-ns-desktop`) at the assembly point (sdk-assembly §9.3 — the pack
//! names its vocabulary, the host decides it belongs on the engine).
//!
//! Every code-exec engine this runtime opens gets the same pack — the
//! host's own session and each sub-agent's, because the face is
//! factory-wide and the prompt face must declare what is installed, never
//! more. What actually reaches the wire is the engine policy's call: a
//! sub-agent's audit-sealed read-only policy refuses every `Device` method
//! fail-closed before a line leaves.
//!
//! The pack speaks over a spawned `phi-ns-cua-stub` sidecar (phi-wire-protocol
//! JSON-lines in, `cua-driver mcp` out). The sidecar spawns **lazily** —
//! the first `desk.*` call in a cell starts it (the lazy-Chrome precedent
//! in `agent.rs`): coding runs carry no device process. With no stub
//! resolvable nothing mounts and `desk.*` is absent from the face.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use phi_agent::code_cell::ExecutorBuilder;
use phi_ns::NsError;
use phi_ns::remote::{ProcessTransport, Transport};
use phi_ns_desktop::PhiNsDesktop;
use serde_json::Value;

/// Explicit override for the stub sidecar — an absolute path or a bare
/// name for PATH lookup. Same variable the p3 real-device suite reads.
const STUB_ENV: &str = "PHI_NS_CUA_STUB_BIN";
/// The install slot every phimint companion binary uses (`~/.local/bin`).
const STUB_SLOT: &str = ".local/bin/phi-ns-cua-stub";

/// Explicit override for the cua driver the stub wraps. Same variable the
/// p3 real-device suite reads.
const DRIVER_ENV: &str = "CUA_DRIVER_BIN";
const DRIVER_SLOT: &str = ".local/bin/cua-driver-local";
/// PATH-name fallback when neither override nor slot is present — the spawn
/// then fails with the stub's own "required executable not found", which is
/// the honest place to say it.
const DRIVER_FALLBACK: &str = "cua-driver-local";

/// The resolved sidecar configuration for one engine.
struct DeskWiring {
    stub: PathBuf,
    args: Vec<String>,
}

/// The sidecar's argv: `--driver` names the cua driver process the stub
/// wraps, `--shots` is the session-local directory its captures stage in.
/// Pinned to the p3 real-device contract
/// (`agent-works/tests/p3_real_device.rs`, `desktop_stage`).
fn stub_args(driver: &str, shots_dir: &Path) -> Vec<String> {
    vec![
        "--driver".to_string(),
        driver.to_string(),
        "--shots".to_string(),
        shots_dir.to_string_lossy().into_owned(),
    ]
}

/// The stub sidecar to spawn: explicit override (path or PATH name), else
/// the `~/.local/bin` slot when present, else nothing mounts.
fn resolve_stub(env: Option<OsString>, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(override_value) = env {
        return Some(PathBuf::from(override_value));
    }
    let slot = home?.join(STUB_SLOT);
    slot.is_file().then_some(slot)
}

/// The cua driver the stub wraps: explicit override, else the `~/.local/bin`
/// slot, else a bare name for PATH lookup at spawn time.
fn resolve_driver(env: Option<OsString>, home: Option<&Path>) -> String {
    if let Some(override_value) = env {
        return override_value.to_string_lossy().into_owned();
    }
    match home.map(|home| home.join(DRIVER_SLOT)) {
        Some(slot) if slot.is_file() => slot.to_string_lossy().into_owned(),
        _ => DRIVER_FALLBACK.to_string(),
    }
}

/// Resolution seam: the env/home lookups live in [`mount_desk`], the
/// decision over explicit values lives here.
fn desk_wiring_with(
    shots_dir: &Path,
    stub_env: Option<OsString>,
    driver_env: Option<OsString>,
    home: Option<&Path>,
) -> Option<DeskWiring> {
    let stub = resolve_stub(stub_env, home)?;
    let driver = resolve_driver(driver_env, home);
    Some(DeskWiring {
        stub,
        args: stub_args(&driver, shots_dir),
    })
}

/// How one engine's sidecar starts. Boxed so the lazy cell below can own it
/// and tests can count spawns.
type WireSpawner = Box<dyn Fn() -> Result<Arc<dyn Transport>, NsError> + Send + Sync>;

/// One sidecar channel per engine, started on the first `desk.*` call. A
/// failed start leaves the cell empty, so the next call retries instead of
/// pinning a dead channel for the session.
struct LazyStub {
    spawn: WireSpawner,
    wire: tokio::sync::OnceCell<Arc<dyn Transport>>,
}

impl LazyStub {
    fn new(spawn: WireSpawner) -> Arc<Self> {
        Arc::new(Self {
            spawn,
            wire: tokio::sync::OnceCell::new(),
        })
    }
}

#[async_trait]
impl Transport for LazyStub {
    async fn request(&self, method: &str, args: Value) -> Result<Value, NsError> {
        let wire = self
            .wire
            .get_or_try_init(|| async { (self.spawn)() })
            .await?;
        wire.request(method, args).await
    }
}

/// The production spawner: `phi-ns-cua-stub` over `ProcessTransport`
/// (correlated JSON-lines, kill-on-drop — session teardown needs no
/// explicit shutdown).
fn process_spawner(wiring: DeskWiring) -> WireSpawner {
    Box::new(move || {
        let program = wiring.stub.to_string_lossy().into_owned();
        ProcessTransport::spawn(&program, &wiring.args, &BTreeMap::new())
            .map(|wire| wire as Arc<dyn Transport>)
    })
}

/// Mount `desk.*` on a code-exec engine under construction — or leave the
/// engine untouched when no stub is resolvable (no pack, no `desk.*` in the
/// face).
pub fn mount_desk(builder: ExecutorBuilder, shots_dir: &Path) -> ExecutorBuilder {
    mount_desk_with(
        builder,
        shots_dir,
        std::env::var_os(STUB_ENV),
        std::env::var_os(DRIVER_ENV),
        dirs::home_dir().as_deref(),
    )
}

/// [`mount_desk`] with the resolution seam exposed (env/home as arguments)
/// so the face contract below is testable without touching the real
/// environment.
fn mount_desk_with(
    builder: ExecutorBuilder,
    shots_dir: &Path,
    stub_env: Option<OsString>,
    driver_env: Option<OsString>,
    home: Option<&Path>,
) -> ExecutorBuilder {
    match desk_wiring_with(shots_dir, stub_env, driver_env, home) {
        Some(wiring) => {
            tracing::info!(
                stub = %wiring.stub.display(),
                shots = %shots_dir.display(),
                "desk.* pack mounted (sidecar spawns on first use)"
            );
            builder.with_ns(PhiNsDesktop::new(LazyStub::new(process_spawner(wiring))))
        }
        None => {
            tracing::info!(
                "desk.* pack not mounted — no cua stub resolved \
                 (set PHI_NS_CUA_STUB_BIN or install ~/.local/bin/phi-ns-cua-stub)"
            );
            builder
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// Records what the engine routed over the wire.
    struct FakeWire {
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl FakeWire {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Transport for FakeWire {
        async fn request(&self, method: &str, args: Value) -> Result<Value, NsError> {
            self.calls.lock().unwrap().push((method.to_string(), args));
            Ok(Value::from("pong"))
        }
    }

    #[test]
    fn stub_args_match_the_p3_contract() {
        assert_eq!(
            stub_args("cua-driver-local", Path::new("/tmp/shot dir")),
            vec![
                "--driver".to_string(),
                "cua-driver-local".to_string(),
                "--shots".to_string(),
                "/tmp/shot dir".to_string(),
            ]
        );
    }

    #[test]
    fn stub_resolution_prefers_the_explicit_override() {
        // A bare name stays a bare name — PATH lookup is the spawn's job.
        assert_eq!(
            resolve_stub(Some("phi-ns-cua-stub".into()), None),
            Some(PathBuf::from("phi-ns-cua-stub"))
        );
    }

    #[test]
    fn stub_resolution_falls_back_to_the_install_slot_only_when_present() {
        let home = tempfile::tempdir().unwrap();
        let slot = home.path().join(".local/bin");
        std::fs::create_dir_all(&slot).unwrap();
        let stub = slot.join("phi-ns-cua-stub");
        std::fs::write(&stub, b"#!/bin/sh\n").unwrap();
        assert_eq!(resolve_stub(None, Some(home.path())), Some(stub));

        let bare = tempfile::tempdir().unwrap();
        assert_eq!(resolve_stub(None, Some(bare.path())), None);
        assert_eq!(resolve_stub(None, None), None);
    }

    #[test]
    fn driver_resolution_prefers_the_explicit_override() {
        assert_eq!(
            resolve_driver(Some("/opt/cua-driver-local".into()), None),
            "/opt/cua-driver-local"
        );
    }

    #[test]
    fn driver_resolution_falls_back_slot_then_path_name() {
        let home = tempfile::tempdir().unwrap();
        let slot = home.path().join(".local/bin");
        std::fs::create_dir_all(&slot).unwrap();
        let driver = slot.join("cua-driver-local");
        std::fs::write(&driver, b"#!/bin/sh\n").unwrap();
        assert_eq!(
            resolve_driver(None, Some(home.path())),
            driver.to_string_lossy()
        );

        let bare = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_driver(None, Some(bare.path())),
            DRIVER_FALLBACK.to_string()
        );
    }

    #[test]
    fn wiring_is_absent_without_a_stub_and_bare_names_stay_bare() {
        let home = tempfile::tempdir().unwrap();
        assert!(
            desk_wiring_with(Path::new("/tmp/shots"), None, None, Some(home.path())).is_none(),
            "no stub override and an empty install slot wires nothing"
        );
        let wiring = desk_wiring_with(
            Path::new("/tmp/shots"),
            Some("phi-ns-cua-stub".into()),
            None,
            Some(home.path()),
        )
        .expect("an override is enough to wire");
        assert_eq!(wiring.stub, PathBuf::from("phi-ns-cua-stub"));
        assert_eq!(
            wiring.args,
            vec![
                "--driver".to_string(),
                DRIVER_FALLBACK.to_string(),
                "--shots".to_string(),
                "/tmp/shots".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn lazy_wire_starts_on_the_first_call_only() {
        let spawns = Arc::new(Mutex::new(0usize));
        let wire = LazyStub::new(Box::new({
            let spawns = Arc::clone(&spawns);
            move || {
                *spawns.lock().unwrap() += 1;
                Ok(FakeWire::new() as Arc<dyn Transport>)
            }
        }));
        assert_eq!(
            *spawns.lock().unwrap(),
            0,
            "nothing starts before the first call"
        );

        wire.request("desk.shot", json!({})).await.unwrap();
        wire.request("desk.click", json!({"x": 1})).await.unwrap();
        assert_eq!(*spawns.lock().unwrap(), 1, "one engine, one sidecar");
    }

    #[tokio::test]
    async fn lazy_wire_routes_calls_and_keeps_the_seeded_shots_dir() {
        let captured: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let calls: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let wiring = desk_wiring_with(
            Path::new("/tmp/shots"),
            Some("phi-ns-cua-stub".into()),
            Some("driver-bin".into()),
            None,
        )
        .expect("override is enough to wire");
        assert_eq!(
            wiring.args,
            stub_args("driver-bin", Path::new("/tmp/shots"))
        );
        let captured_args = Arc::clone(&captured);
        let recorded = Arc::clone(&calls);
        let wire = LazyStub::new(Box::new(move || {
            captured_args.lock().unwrap().push(wiring.args.clone());
            struct Recorded {
                calls: Arc<Mutex<Vec<(String, Value)>>>,
            }
            #[async_trait]
            impl Transport for Recorded {
                async fn request(&self, method: &str, args: Value) -> Result<Value, NsError> {
                    self.calls.lock().unwrap().push((method.to_string(), args));
                    Ok(Value::Null)
                }
            }
            Ok(Arc::new(Recorded {
                calls: Arc::clone(&recorded),
            }) as Arc<dyn Transport>)
        }));
        wire.request("desk.win_state", json!({})).await.unwrap();
        assert_eq!(
            captured.lock().unwrap().as_slice(),
            &[stub_args("driver-bin", Path::new("/tmp/shots"))]
        );
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[("desk.win_state".to_string(), json!({}))]
        );
    }

    #[tokio::test]
    async fn lazy_wire_surfaces_a_spawn_failure_and_retries_next_call() {
        let attempts = Arc::new(Mutex::new(0usize));
        let wire = LazyStub::new(Box::new({
            let attempts = Arc::clone(&attempts);
            move || {
                *attempts.lock().unwrap() += 1;
                Err(NsError::Transport("stub missing".to_string()))
            }
        }));
        let err = wire
            .request("desk.shot", json!({}))
            .await
            .expect_err("spawn failure must reach the observation");
        assert!(
            err.to_string().contains("stub missing"),
            "error text keeps the cause: {err}"
        );
        assert_eq!(*attempts.lock().unwrap(), 1);

        // The failed start pinned nothing: the next call tries again.
        wire.request("desk.shot", json!({})).await.unwrap_err();
        assert_eq!(*attempts.lock().unwrap(), 2);
    }

    /// The face contract (提示面同源): resolution decides the face — a mounted
    /// pack declares its vocabulary, an unresolvable stub declares nothing.
    #[test]
    fn resolution_decides_the_face_declared_is_installed() {
        use phi_agent::code_cell::{JournalHandle, MemJournal};
        let builder = || ExecutorBuilder::new(JournalHandle::new(Box::new(MemJournal::default())));

        let mounted = mount_desk_with(
            builder(),
            Path::new("/tmp/shots"),
            Some("phi-ns-cua-stub".into()),
            None,
            None,
        );
        let face = mounted.prompt_face();
        assert!(
            face.contains("desk.shot"),
            "face declares the lookers: {face}"
        );
        assert!(
            face.contains("desk.click"),
            "face declares the acts: {face}"
        );

        let bare = mount_desk_with(builder(), Path::new("/tmp/shots"), None, None, None);
        assert!(
            !bare.prompt_face().contains("desk."),
            "no stub resolved, no desk vocabulary declared"
        );
    }
}
