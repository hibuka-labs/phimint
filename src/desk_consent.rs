//! The device consent posture (posture B) and the bridge that carries it to
//! the human.
//!
//! Posture B (2026-10-30, the user's call): **looking** at the desktop never
//! asks; **moving the pointer** never asks; every action that changes the
//! machine — a click, a keystroke, typed text, a window moved, the clipboard
//! written — asks a human, every time.
//!
//! Where each piece of knowledge lives:
//!
//! * the pack (`phi-ns-desktop`) declares *what* each method touches — the
//!   declaration vocabulary is `phi_cap`, never a second one;
//! * this module is the **product's opinion** about which of those effects
//!   need a human. The engine's policy seam says that opinion belongs to
//!   the application, never to the engine ("no domain rules live here — what
//!   'risky' means is the application's opinion");
//! * the two lists below must **partition the pack's methods exactly**, so a
//!   pack that grows a method fails the tests until the product classifies
//!   it. Fail-closed: a method nobody classified is refused, never waved
//!   through.
//!
//! The bridge is the same surface a tool call uses: the product's approval
//! popup, the same mode switch (`auto` / `ask` / `deny`), the same ledger.
//! `--approval ask` (or Shift+Tab to *ask*) is what makes the prompts
//! visible; in *auto* the gate answers without a popup, because "don't block
//! me" is the user's global switch.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use phi_agent::code_cell::{CallCtx, Capability, Policy, PolicyDecision};
use phi_agent::{ApprovalDecision, ApprovalRequest, InteractionChannel, RiskLevel};
use serde_json::Value;

use phi_agent_tui::InteractSlot;

/// The device methods whose effect **changes the machine**: a human decides
/// each one.
///
/// This is the product's consent list, not the pack's — the pack declares
/// that `desk.click` is device input, the same class as `desk.move`; what
/// differs is what *this product* wants a human to sign off on.
const DEVICE_ASK: &[&str] = &[
    "desk.click",
    "desk.drag",
    "desk.key",
    "desk.hotkey",
    "desk.type",
    "desk.menu",
    "desk.clip_write",
    "desk.win_frame",
    "desk.escalate",
];

/// The device methods that only **look** or only **move the pointer**: they
/// flow, unasked.
///
/// Pointer motion is here on purpose (posture B): moving the cursor changes
/// nothing the user can lose, and a prompt per mouse move would train the
/// user to hit `y` without reading.
const DEVICE_FLOW: &[&str] = &[
    "desk.shot",
    "desk.win_state",
    "desk.windows",
    "desk.apps",
    "desk.screen",
    "desk.regions",
    "desk.verify",
    "desk.clip_read",
    "desk.session",
    "desk.move",
    "desk.scroll",
    "desk.agent_cursor",
];

/// The decision for a device method the posture does not name: refused.
///
/// Fail-closed beats "not classified → allowed": an unclassified method is a
/// method nobody has thought about, and the whole point of the gate is that
/// unthought-of effects do not reach the machine.
const UNNAMED_DEVICE: PolicyDecision = PolicyDecision::Deny;

/// The name → decision table of the device pack, built from the two lists.
fn device_table() -> HashMap<String, PolicyDecision> {
    let mut table: HashMap<String, PolicyDecision> = HashMap::new();
    for name in DEVICE_ASK {
        table.insert((*name).to_string(), PolicyDecision::Ask);
    }
    for name in DEVICE_FLOW {
        table.insert((*name).to_string(), PolicyDecision::Allow);
    }
    table
}

/// The consent policy: a class table for everything that is not a device
/// method, a name table for the device pack's methods.
///
/// Two granularities on purpose (the coarse class cannot separate a click
/// from a pointer move — both are `Device` — but the product's consent
/// posture is exactly about that difference).
pub struct ConsentPolicy {
    /// Per-class decisions; a class nobody listed flows.
    classes: HashMap<Capability, PolicyDecision>,
    /// Per-method decisions for the device pack.
    methods: HashMap<String, PolicyDecision>,
    /// The decision for a device method the posture does not name.
    unnamed_device: PolicyDecision,
}

impl ConsentPolicy {
    /// The tool road (`code_cell`): the step itself already passed the
    /// product's approval gate, so the cell's own mechanics ride; only the
    /// device effects the posture names ask.
    pub fn tool_road() -> Self {
        Self {
            classes: HashMap::new(),
            methods: device_table(),
            unnamed_device: UNNAMED_DEVICE,
        }
    }

    /// The act road: a code step *is* the model's action, so processes and
    /// the network ask as they always did; the device methods follow the
    /// posture (reads and pointer moves flow).
    pub fn act_road() -> Self {
        let mut classes = HashMap::new();
        classes.insert(Capability::Process, PolicyDecision::Ask);
        classes.insert(Capability::Net, PolicyDecision::Ask);
        Self {
            classes,
            methods: device_table(),
            unnamed_device: UNNAMED_DEVICE,
        }
    }

    /// The decision for one call — the policy seam's single question.
    pub fn decision(&self, cap: Capability, fn_name: &str) -> PolicyDecision {
        if cap == Capability::Device {
            return self
                .methods
                .get(fn_name)
                .copied()
                .unwrap_or(self.unnamed_device);
        }
        self.classes
            .get(&cap)
            .copied()
            .unwrap_or(PolicyDecision::Allow)
    }
}

impl Policy for ConsentPolicy {
    fn allow(&self, cap: Capability, ctx: &CallCtx<'_>) -> PolicyDecision {
        self.decision(cap, ctx.fn_name)
    }
}

/// The bridge from the engine's consent hold to the product's approval
/// popup.
///
/// One handle, two ends: the engine holds it from assembly (so a hold can
/// reach a human at any point in a step) and the product answers through the
/// approval queue the TUI already drains.
pub struct ConsentBridge {
    /// The product's interaction surface (the live approval gate).
    channel: Arc<InteractSlot>,
    /// Standing grants: methods the human allowed "always" this session.
    granted: Mutex<HashSet<String>>,
}

impl ConsentBridge {
    /// A bridge over the product's interaction slot.
    pub fn new(channel: Arc<InteractSlot>) -> Self {
        Self {
            channel,
            granted: Mutex::new(HashSet::new()),
        }
    }

    /// Is this method standing-granted (the human said "always")?
    fn granted(&self, fn_name: &str) -> bool {
        self.granted
            .lock()
            .expect("consent grant lock")
            .contains(fn_name)
    }
}

/// What the human reads before deciding: the action, in words, with its
/// arguments.
fn describe_action(fn_name: &str, args: Option<&Value>) -> String {
    let payload = match args {
        Some(Value::Object(map)) if !map.is_empty() => {
            let mut parts = Vec::new();
            for (key, value) in map {
                parts.push(format!("{key} = {}", compact(value)));
            }
            parts.join(", ")
        }
        Some(Value::Null) | None => String::new(),
        Some(other) => compact(other),
    };
    let mut message = format!("The agent wants to run a desktop action:\n\n{fn_name}");
    if !payload.is_empty() {
        message.push_str(&format!("\n{payload}"));
    }
    message
        .push_str("\n\ny allows this one action; n refuses it and the agent continues without it.");
    message
}

/// One argument value on one line — short enough for the popup, complete
/// enough to recognize.
fn compact(value: &Value) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    };
    let text = text.replace('\n', " ");
    if text.chars().count() > 120 {
        let mut head: String = text.chars().take(117).collect();
        head.push_str("...");
        head
    } else {
        text
    }
}

#[async_trait::async_trait]
impl phi_agent::code_cell::InteractionBridge for ConsentBridge {
    /// A held call asks the human. `Err` means the interaction itself
    /// failed (cancelled, nobody answered) — the engine refuses
    /// fail-closed and the reason rides the hold the model sees.
    async fn consent(
        &self,
        req: &phi_agent::code_cell::ConsentRequest<'_>,
    ) -> Result<PolicyDecision, String> {
        if self.granted(req.fn_name) {
            return Ok(PolicyDecision::Allow);
        }
        let request = ApprovalRequest {
            title: format!("desktop action: {}", req.fn_name),
            message: describe_action(req.fn_name, req.args),
            // No standing grant through the engine's action-key cache: this
            // bridge keeps its own (see `AllowAlways` below), and a grant
            // recorded anywhere else would outlive the session that made it.
            action_key: None,
            risk_level: RiskLevel::Sensitive,
            raw: req.args.cloned(),
            source: None,
        };
        let cancel = self.channel.cancel_token();
        match self.channel.request_consent(request, &cancel).await {
            Ok(ApprovalDecision::AllowOnce) => Ok(PolicyDecision::Allow),
            Ok(ApprovalDecision::AllowAlways) => {
                // "Always" is a standing grant for this method, for this
                // session — remembered here, because nothing else would
                // honor it and a button that lies is worse than no button.
                self.granted
                    .lock()
                    .expect("consent grant lock")
                    .insert(req.fn_name.to_string());
                Ok(PolicyDecision::Allow)
            }
            Ok(ApprovalDecision::Deny) => Ok(PolicyDecision::Deny),
            Err(e) => Err(e.to_string()),
        }
    }

    /// The cell face has no `ask.user` (consent-only mount), so a cell
    /// cannot ask through this bridge.
    async fn ask(&self, _question: &str, _options: &[Value]) -> Result<Value, String> {
        Err("the code-cell road has no picker; use the ask_user tool".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_ns::NamespaceProvider;

    /// The pack's own method names — the declaration source the posture must
    /// cover exactly.
    fn pack_methods() -> Vec<String> {
        phi_ns_desktop::PhiNsDesktop::unconnected()
            .methods()
            .into_iter()
            .map(|m| m.name.to_string())
            .collect()
    }

    /// The posture covers every method the pack declares — no gaps, no
    /// extras. A pack that grows a method breaks this test until the
    /// product says what it means.
    #[test]
    fn the_two_lists_partition_the_pack_methods() {
        let mut ask: Vec<&str> = DEVICE_ASK.to_vec();
        let mut flow: Vec<&str> = DEVICE_FLOW.to_vec();
        ask.sort_unstable();
        flow.sort_unstable();

        let mut declared: Vec<String> = pack_methods();
        declared.sort();
        let mut covered: Vec<String> = ask
            .iter()
            .chain(flow.iter())
            .map(|n| (*n).to_string())
            .collect();
        covered.sort();

        assert_eq!(
            covered, declared,
            "the posture must cover every desk method exactly once"
        );

        // No name sits on both sides — a name in both lists would have one
        // answer, whichever list was read last.
        for name in &ask {
            assert!(
                !flow.contains(name),
                "{name} is classified twice (ask and flow)"
            );
        }
    }

    /// The pack's methods are all known to the table — no typo, no stale
    /// name from a renamed method.
    #[test]
    fn every_classified_name_is_a_real_method() {
        let declared = pack_methods();
        for name in DEVICE_ASK.iter().chain(DEVICE_FLOW.iter()) {
            assert!(
                declared.iter().any(|d| d == name),
                "{name} is not a method of the desk pack"
            );
        }
    }

    /// The posture itself: mutating input asks, reads and pointer motion
    /// flow.
    #[test]
    fn the_posture_asks_mutation_and_flows_the_rest() {
        let policy = ConsentPolicy::tool_road();
        for name in DEVICE_ASK {
            assert_eq!(
                policy.decision(Capability::Device, name),
                PolicyDecision::Ask,
                "{name} changes the machine — a human decides"
            );
        }
        for name in DEVICE_FLOW {
            assert_eq!(
                policy.decision(Capability::Device, name),
                PolicyDecision::Allow,
                "{name} only looks or only moves the pointer"
            );
        }
    }

    /// Fail-closed: a device method nobody classified is refused.
    #[test]
    fn an_unclassified_device_method_is_refused() {
        let policy = ConsentPolicy::tool_road();
        assert_eq!(
            policy.decision(Capability::Device, "desk.teleport"),
            PolicyDecision::Deny
        );
    }

    /// The tool road leaves everything that is not a device method alone —
    /// the cell's own mechanics ride the tool-call gate.
    #[test]
    fn the_tool_road_leaves_the_other_classes_flowing() {
        let policy = ConsentPolicy::tool_road();
        for cap in [
            Capability::Pure,
            Capability::ReadFs,
            Capability::WriteFs,
            Capability::Net,
            Capability::Process,
        ] {
            assert_eq!(
                policy.decision(cap, "anything"),
                PolicyDecision::Allow,
                "{cap} must not be gated by the device posture"
            );
        }
    }

    /// The act road keeps its own gates: a process and the network ask.
    #[test]
    fn the_act_road_asks_process_and_net() {
        let policy = ConsentPolicy::act_road();
        assert_eq!(
            policy.decision(Capability::Process, "sh.run"),
            PolicyDecision::Ask
        );
        assert_eq!(
            policy.decision(Capability::Net, "net.get"),
            PolicyDecision::Ask
        );
        assert_eq!(
            policy.decision(Capability::ReadFs, "open"),
            PolicyDecision::Allow
        );
        // …and the device methods follow the posture, not the old
        // "everything device asks" rule.
        assert_eq!(
            policy.decision(Capability::Device, "desk.shot"),
            PolicyDecision::Allow
        );
        assert_eq!(
            policy.decision(Capability::Device, "desk.click"),
            PolicyDecision::Ask
        );
    }

    /// The popup's message names the action and its arguments — a human
    /// decides on something they can read.
    #[test]
    fn the_message_shows_the_action_and_its_arguments() {
        let args: Value = serde_json::json!({"target": [855, 553]});
        let message = describe_action("desk.click", Some(&args));
        assert!(message.contains("desk.click"), "{message}");
        assert!(message.contains("855"), "{message}");
        assert!(message.contains("y allows"), "{message}");
    }

    /// A long argument is cut, not dropped — the human still sees what the
    /// action is about.
    #[test]
    fn a_long_argument_is_cut_not_lost() {
        let args: Value = serde_json::json!({"text": "x".repeat(500)});
        let message = describe_action("desk.type", Some(&args));
        assert!(message.contains("..."), "{message}");
        assert!(message.len() < 400, "the popup must stay readable");
    }
}

/// The seam, end to end: the engine's chokepoint, the posture, the bridge and
/// the product's popup — the whole path a desktop click takes.
#[cfg(test)]
mod seam {
    use super::*;
    use crate::approval::build_live_approval;
    use async_trait::async_trait;
    use phi_agent::code_cell::{
        Action, CodeCellExecutor, ConsentRequest, ExecutorBuilder, FileJournal, InteractionBridge,
        JournalHandle, Limits, PythonWorker,
    };
    use phi_cap::{device_capture, device_input};
    use phi_ns::{
        CaptureUse, HostCall, MethodSpec, NamespaceProvider, NsError, NsObject, WorkerSink, m,
    };
    use std::path::Path;
    use std::sync::Mutex;

    /// A stand-in for the desktop pack: the same method **names** and the
    /// same capability classes, so the production posture applies to it —
    /// a look (`desk.shot`) and an act (`desk.click`).
    struct TestDesk;

    const DESK_SHIM: &str = r#"
class DeskSdk:
    def shot(self):
        r = _phi_call("desk.shot", [])
        print("looked")
        return r
    def click(self, target):
        r = _phi_call("desk.click", [target])
        print("clicked")
        return r
desk = DeskSdk()
"#;

    /// The look: it binds what it saw, so the act has something to spend.
    struct DeskShot;

    /// The act: it would move the machine.
    struct DeskClick;

    #[async_trait]
    impl HostCall for DeskShot {
        async fn call(&self, _args: Vec<Value>) -> Result<Value, NsError> {
            Ok(serde_json::json!({"capture": {"rev": 1}, "looked": true}))
        }
    }

    #[async_trait]
    impl HostCall for DeskClick {
        async fn call(&self, _args: Vec<Value>) -> Result<Value, NsError> {
            Ok(serde_json::json!({"clicked": true}))
        }
    }

    #[async_trait]
    impl NamespaceProvider for TestDesk {
        fn namespaces(&self) -> &'static [&'static str] {
            &["desk"]
        }

        fn methods(&self) -> Vec<MethodSpec> {
            vec![
                m(
                    "desk.shot",
                    &[device_capture(&["desktop"])],
                    "look at the desktop; returns a receipt",
                    false,
                )
                .capture(CaptureUse::Mint),
                m(
                    "desk.click",
                    &[device_input(&["desktop"])],
                    "click a point; returns a receipt",
                    false,
                )
                .capture(CaptureUse::Consume),
            ]
        }

        async fn install(&self, sink: &mut dyn WorkerSink) -> Result<(), NsError> {
            sink.register_fn("desk.shot", Box::new(DeskShot))?;
            sink.register_fn("desk.click", Box::new(DeskClick))?;
            sink.eval(DESK_SHIM).await
        }

        fn summarize(&self, _name: &str, _obj: &NsObject) -> Option<String> {
            None
        }
    }

    /// A bridge that records what it was asked and answers from a script —
    /// the posture's counterfactual: what would happen if the human answered
    /// this way.
    struct ScriptedBridge {
        asked: Mutex<Vec<String>>,
        answer: Mutex<PolicyDecision>,
    }

    impl ScriptedBridge {
        fn answering(answer: PolicyDecision) -> Arc<Self> {
            Arc::new(Self {
                asked: Mutex::new(Vec::new()),
                answer: Mutex::new(answer),
            })
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().expect("asked lock").clone()
        }
    }

    #[async_trait]
    impl InteractionBridge for ScriptedBridge {
        async fn consent(&self, req: &ConsentRequest<'_>) -> Result<PolicyDecision, String> {
            self.asked
                .lock()
                .expect("asked lock")
                .push(req.fn_name.to_string());
            Ok(*self.answer.lock().expect("answer lock"))
        }

        async fn ask(&self, _question: &str, _options: &[Value]) -> Result<Value, String> {
            Err("no picker".into())
        }
    }

    fn cells(codes: &[&str]) -> Vec<Action> {
        codes
            .iter()
            .map(|code| Action::CodeCell {
                intent: String::new(),
                code: (*code).to_string(),
            })
            .collect()
    }

    async fn engine(
        dir: &Path,
        bridge: Arc<dyn InteractionBridge>,
    ) -> CodeCellExecutor<PythonWorker> {
        let journal = FileJournal::open(dir.join("j.jsonl")).expect("journal opens");
        let limits = Limits::default();
        ExecutorBuilder::new(JournalHandle::new(Box::new(journal)))
            .with_policy(ConsentPolicy::tool_road())
            .with_limits(limits)
            .with_ns(TestDesk)
            .with_consent(bridge)
            .build(move |registry| PythonWorker::spawn(registry, limits))
            .await
            .expect("engine assembles")
    }

    /// The posture reaches the chokepoint: the look flows, the act asks, and
    /// the human's answer decides whether the act happens.
    #[tokio::test]
    async fn a_look_flows_and_an_act_asks_the_human() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = ScriptedBridge::answering(PolicyDecision::Allow);
        let mut executor = engine(
            dir.path(),
            Arc::clone(&bridge) as Arc<dyn InteractionBridge>,
        )
        .await;

        let obs = executor
            .execute(cells(&["desk.shot()", "desk.click([10, 20])"]))
            .await;
        assert!(obs.error.is_none(), "{obs:?}");
        assert_eq!(
            bridge.asked(),
            vec!["desk.click".to_string()],
            "only the act asks; the look flows"
        );
        assert!(obs.stdout.contains("clicked"), "{obs:?}");
    }

    /// A refusal is a refusal: the act never reaches the device and the model
    /// sees the hold.
    #[tokio::test]
    async fn a_refused_act_never_reaches_the_device() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = ScriptedBridge::answering(PolicyDecision::Deny);
        let mut executor = engine(
            dir.path(),
            Arc::clone(&bridge) as Arc<dyn InteractionBridge>,
        )
        .await;

        let obs = executor
            .execute(cells(&["desk.shot()", "desk.click([10, 20])"]))
            .await;
        assert!(obs.is_policy_hold(), "{obs:?}");
        assert!(
            !obs.stdout.contains("clicked"),
            "a refused action must not reach the device: {obs:?}"
        );
    }

    /// The whole production path: engine → posture → bridge → the product's
    /// own popup → the human's answer → the effect.
    #[tokio::test]
    async fn the_product_popup_gates_the_click() {
        let dir = tempfile::tempdir().unwrap();
        let (gate, _policy, mut queue_rx, _switch) = build_live_approval("ask", None);
        let slot = Arc::new(InteractSlot::new(gate));
        let bridge = Arc::new(ConsentBridge::new(Arc::clone(&slot)));
        let mut executor = engine(
            dir.path(),
            Arc::clone(&bridge) as Arc<dyn InteractionBridge>,
        )
        .await;

        let run = tokio::spawn(async move {
            executor
                .execute(cells(&["desk.shot()", "desk.click([10, 20])"]))
                .await
        });

        let item = queue_rx.recv().await.expect("the popup asks");
        assert_eq!(item.request.title, "desktop action: desk.click");
        assert!(
            item.request.message.contains("desk.click"),
            "{}",
            item.request.message
        );
        item.decision_tx
            .send(ApprovalDecision::AllowOnce)
            .expect("the popup answers");

        let obs = run.await.expect("the step finishes");
        assert!(obs.error.is_none(), "{obs:?}");
        assert!(obs.stdout.contains("clicked"), "{obs:?}");
    }

    /// "Always" is a standing grant for the session — the second click of the
    /// same method rides without a second popup.
    #[tokio::test]
    async fn an_always_grant_stops_asking() {
        let (gate, _policy, mut queue_rx, _switch) = build_live_approval("ask", None);
        let slot = Arc::new(InteractSlot::new(gate));
        let bridge = Arc::new(ConsentBridge::new(Arc::clone(&slot)));

        let first = Arc::clone(&bridge);
        let call = tokio::spawn(async move {
            let args = serde_json::json!([10, 20]);
            let req = ConsentRequest {
                cap: Capability::Device,
                fn_name: "desk.click",
                args: Some(&args),
                args_digest: "digest",
                cell_index: 0,
                step_index: 1,
            };
            first.consent(&req).await
        });
        let item = queue_rx.recv().await.expect("the popup asks");
        item.decision_tx
            .send(ApprovalDecision::AllowAlways)
            .expect("the popup answers");
        assert_eq!(
            call.await.expect("the ask finishes").expect("allowed"),
            PolicyDecision::Allow
        );

        // The second click of the same method: allowed without asking.
        let args = serde_json::json!([30, 40]);
        let req = ConsentRequest {
            cap: Capability::Device,
            fn_name: "desk.click",
            args: Some(&args),
            args_digest: "digest",
            cell_index: 0,
            step_index: 2,
        };
        assert_eq!(
            bridge.consent(&req).await.expect("allowed"),
            PolicyDecision::Allow
        );
        assert!(
            queue_rx.try_recv().is_err(),
            "a standing grant must not ask again"
        );
    }

    /// The consent mount is consent-only: the cell face gains no `ask.user`
    /// (the tool road already has the picker as a tool).
    #[tokio::test]
    async fn the_consent_mount_adds_no_ask_face() {
        let dir = tempfile::tempdir().unwrap();
        let journal = FileJournal::open(dir.path().join("j.jsonl")).expect("journal opens");
        let builder = ExecutorBuilder::new(JournalHandle::new(Box::new(journal)))
            .with_policy(ConsentPolicy::tool_road())
            .with_ns(TestDesk)
            .with_consent(ScriptedBridge::answering(PolicyDecision::Allow));
        let face = builder.prompt_face();
        assert!(face.contains("desk.click"), "{face}");
        assert!(!face.contains("ask.user"), "{face}");
    }
}
