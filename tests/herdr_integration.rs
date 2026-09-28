use std::collections::VecDeque;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use herdr_resource_monitor::herdr::{
    correlate, CliHerdrClient, CommandOutput, CommandRunner, HerdrClient, HerdrError,
    PaneProcessInfo, Result,
};
use serde_json::{json, Value};

fn fixture(name: &str) -> Value {
    let source = match name {
        "codex" => include_str!("fixtures/herdr/codex.json"),
        "claude" => include_str!("fixtures/herdr/claude.json"),
        "shell" => include_str!("fixtures/herdr/shell.json"),
        "agent_without_session" => include_str!("fixtures/herdr/agent_without_session.json"),
        "no_foreground" => include_str!("fixtures/herdr/no_foreground.json"),
        _ => panic!("unknown fixture"),
    };
    serde_json::from_str(source).unwrap()
}

fn output(value: &Value) -> CommandOutput {
    CommandOutput {
        success: true,
        code: Some(0),
        stdout: serde_json::to_vec(value).unwrap(),
        stderr: Vec::new(),
    }
}

type Calls = Arc<Mutex<Vec<Vec<String>>>>;

struct FixtureRunner {
    outputs: VecDeque<Result<CommandOutput>>,
    calls: Calls,
}

impl CommandRunner for FixtureRunner {
    fn run(&mut self, binary: &OsStr, args: &[&str], timeout: Duration) -> Result<CommandOutput> {
        assert_eq!(binary, "fixture-herdr");
        assert!(timeout <= Duration::from_secs(2));
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| (*arg).to_owned()).collect());
        self.outputs.pop_front().expect("unexpected extra command")
    }
}

fn client(outputs: Vec<Result<CommandOutput>>) -> (CliHerdrClient<FixtureRunner>, Calls) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    (
        CliHerdrClient::with_runner(
            "fixture-herdr",
            FixtureRunner {
                outputs: outputs.into(),
                calls: Arc::clone(&calls),
            },
        ),
        calls,
    )
}

#[test]
fn five_fixtures_use_pane_identity_without_requiring_native_session() {
    for name in [
        "codex",
        "claude",
        "shell",
        "agent_without_session",
        "no_foreground",
    ] {
        let data = fixture(name);
        let (mut client, calls) = client(
            ["snapshot", "panes", "agents", "process_info"]
                .iter()
                .map(|key| Ok(output(&data[key])))
                .collect(),
        );
        let snapshot = client.snapshot().unwrap();
        let panes = client.panes().unwrap();
        let agents = client.agents().unwrap();
        let pane_id = snapshot.focused_pane_id.as_deref().unwrap();
        let process = client.process_info(pane_id).unwrap();
        let target = snapshot.target(pane_id, process).unwrap();
        assert_eq!(snapshot.panes, panes);
        assert_eq!(snapshot.agents, agents);
        assert_eq!(target.pane_id, pane_id);
        assert_eq!(target.process.pane_id, target.pane_id);
        assert_eq!(target.pane.cwd.as_deref(), Some("/fixture/project"));
        assert_eq!(
            target.pane.foreground_cwd.as_deref(),
            Some("/fixture/project/src")
        );
        match name {
            "codex" | "claude" | "agent_without_session" => {
                let metadata = target.agent.as_ref().unwrap();
                let expected_agent = if name == "claude" { "claude" } else { "codex" };
                assert_eq!(metadata.agent.as_deref(), Some(expected_agent));
                assert_eq!(
                    metadata.agent_session.is_some(),
                    name != "agent_without_session"
                );
                assert_eq!(metadata.tokens["fixture_effort"], "xhigh");
                assert_eq!(
                    metadata.info.as_ref().unwrap().extra["fixture_extra"]["preserved"],
                    true
                );
            }
            _ => assert!(target.agent.is_none()),
        }
        if name == "no_foreground" {
            assert!(target.process.foreground_pids().is_empty());
            assert!(target.process.shell_pid.is_some());
            assert!(target.process.foreground_process_group_id.is_some());
        } else {
            assert_eq!(target.process.foreground_pids().len(), 1);
            assert_ne!(
                target.process.foreground_pids()[0],
                target.process.foreground_process_group_id.unwrap()
            );
            assert_eq!(
                target.process.foreground_processes[0]
                    .argv
                    .as_ref()
                    .unwrap()[1],
                "/fixture/path with spaces"
            );
        }
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                vec!["api", "snapshot"],
                vec!["pane", "list"],
                vec!["agent", "list"],
                vec!["pane", "process-info", "--pane", pane_id],
            ]
        );
    }
}

#[test]
fn historical_session_and_display_metadata_do_not_promote_shell_to_agent() {
    for agent in [serde_json::Value::Null, serde_json::json!("")] {
        let mut data = fixture("codex");
        data["snapshot"]["result"]["snapshot"]["agents"] = serde_json::json!([]);
        data["snapshot"]["result"]["snapshot"]["panes"][0]["agent"] = agent;
        let expected_pane = data["snapshot"]["result"]["snapshot"]["panes"][0].clone();
        assert!(expected_pane["agent_session"].is_object());
        assert!(expected_pane["display_agent"].is_string());
        let (mut client, _) = client(vec![
            Ok(output(&data["snapshot"])),
            Ok(output(&data["process_info"])),
        ]);
        let snapshot = client.snapshot().unwrap();
        let pane_id = snapshot.focused_pane_id.as_deref().unwrap();
        let process = client.process_info(pane_id).unwrap();
        let target = snapshot.target(pane_id, process).unwrap();
        assert!(target.agent.is_none());
        assert_eq!(target.pane_id, pane_id);
        assert_eq!(
            serde_json::to_value(&target.pane.agent_session).unwrap(),
            expected_pane["agent_session"]
        );
        assert_eq!(
            serde_json::to_value(&target.pane.display_agent).unwrap(),
            expected_pane["display_agent"]
        );
    }
}

#[test]
fn same_agent_name_does_not_merge_different_panes() {
    let first = fixture("codex");
    let second = fixture("agent_without_session");
    let mut combined = first["snapshot"].clone();
    combined["result"]["snapshot"]["panes"]
        .as_array_mut()
        .unwrap()
        .push(second["snapshot"]["result"]["snapshot"]["panes"][0].clone());
    combined["result"]["snapshot"]["agents"]
        .as_array_mut()
        .unwrap()
        .push(second["snapshot"]["result"]["snapshot"]["agents"][0].clone());
    let (mut client, _) = client(vec![Ok(output(&combined))]);
    let snapshot = client.snapshot().unwrap();
    let targets: Vec<_> = [first, second]
        .into_iter()
        .map(|data| {
            let process: PaneProcessInfo =
                serde_json::from_value(data["process_info"]["result"]["process_info"].clone())
                    .unwrap();
            snapshot.target(&process.pane_id.clone(), process).unwrap()
        })
        .collect();
    assert_ne!(targets[0].pane_id, targets[1].pane_id);
    assert_ne!(
        targets[0].process.foreground_pids(),
        targets[1].process.foreground_pids()
    );
    assert_eq!(
        targets[0].agent.as_ref().unwrap().name,
        targets[1].agent.as_ref().unwrap().name
    );
}

#[test]
fn mismatched_process_or_agent_pane_is_rejected() {
    let data = fixture("codex");
    let (mut client, _) = client(vec![
        Ok(output(&data["snapshot"])),
        Ok(output(&data["process_info"])),
    ]);
    let snapshot = client.snapshot().unwrap();
    assert!(matches!(
        client.process_info("p:another"),
        Err(HerdrError::PaneMismatch { .. })
    ));
    let pane = &snapshot.panes[0];
    let mut process: PaneProcessInfo =
        serde_json::from_value(data["process_info"]["result"]["process_info"].clone()).unwrap();
    process.pane_id = "p:another".into();
    assert!(matches!(
        correlate(pane, None, process.clone()),
        Err(HerdrError::PaneMismatch { .. })
    ));
    process.pane_id = pane.pane_id.clone();
    let mut agent = snapshot.agents[0].clone();
    agent.pane_id = "p:another".into();
    assert!(matches!(
        correlate(pane, Some(&agent), process.clone()),
        Err(HerdrError::PaneMismatch { .. })
    ));
    assert!(matches!(
        snapshot.target("p:missing", process),
        Err(HerdrError::MissingPane(_))
    ));
}

#[test]
fn missing_and_null_optional_fields_are_not_invented() {
    let data = fixture("shell");
    let mut snapshot_response = data["snapshot"].clone();
    let pane = &mut snapshot_response["result"]["snapshot"]["panes"][0];
    pane["cwd"] = Value::Null;
    pane.as_object_mut().unwrap().remove("foreground_cwd");
    pane["future_metadata"] = json!({"preserved": true});
    let (mut client, _) = client(vec![Ok(output(&snapshot_response))]);
    let snapshot = client.snapshot().unwrap();
    assert!(snapshot.panes[0].cwd.is_none());
    assert!(snapshot.panes[0].foreground_cwd.is_none());
    assert!(snapshot.panes[0].agent_session.is_none());
    assert_eq!(
        snapshot.panes[0].extra["future_metadata"]["preserved"],
        true
    );
    assert!(snapshot.panes[0].tokens.is_empty());
}

#[test]
fn malformed_response_required_fields_and_wrong_result_types_fail() {
    let data = fixture("codex");
    let mut missing = data["snapshot"].clone();
    missing["result"]["snapshot"]["panes"][0]
        .as_object_mut()
        .unwrap()
        .remove("pane_id");
    let mut malformed = output(&Value::Null);
    malformed.stdout = b"not JSON".to_vec();
    let (mut client, _) = client(vec![
        Ok(malformed),
        Ok(output(&missing)),
        Ok(output(&data["panes"])),
        Ok(output(&json!({"id":"fixture"}))),
    ]);
    assert!(matches!(client.snapshot(), Err(HerdrError::Json(_))));
    assert!(matches!(client.snapshot(), Err(HerdrError::Json(_))));
    assert!(matches!(
        client.snapshot(),
        Err(HerdrError::UnexpectedResponse { .. })
    ));
    assert!(matches!(
        client.snapshot(),
        Err(HerdrError::InvalidEnvelope)
    ));
}

#[test]
fn api_errors_cli_failures_and_transport_timeouts_are_distinct() {
    let error = json!({"id":"fixture","error":{"code":"pane_not_found","message":"gone"}});
    let (mut client, _) = client(vec![
        Ok(CommandOutput {
            success: false,
            code: Some(1),
            stdout: vec![],
            stderr: serde_json::to_vec(&error).unwrap(),
        }),
        Ok(output(&error)),
        Ok(CommandOutput {
            success: false,
            code: Some(2),
            stdout: vec![],
            stderr: b"unavailable".to_vec(),
        }),
        Err(HerdrError::Timeout(Duration::from_millis(20))),
    ]);
    assert!(
        matches!(client.panes(), Err(HerdrError::Api { code, .. }) if code == "pane_not_found")
    );
    assert!(matches!(client.panes(), Err(HerdrError::Api { .. })));
    assert!(matches!(
        client.panes(),
        Err(HerdrError::CommandFailed { code: Some(2), .. })
    ));
    assert!(matches!(client.panes(), Err(HerdrError::Timeout(_))));
}

#[test]
fn foreground_pipeline_uses_unique_process_ids_and_passes_pane_as_one_argument() {
    let mut data = fixture("shell")["process_info"].clone();
    let pane_id = "p:fixture; echo not-a-shell-command";
    data["result"]["process_info"]["pane_id"] = pane_id.into();
    data["result"]["process_info"]["foreground_processes"] = json!([
        {"pid": 81, "name": "first"},
        {"pid": 82, "name": "second"},
        {"pid": 81, "name": "first"}
    ]);
    let (mut client, calls) = client(vec![Ok(output(&data))]);
    let process = client.process_info(pane_id).unwrap();
    assert_eq!(process.foreground_pids(), vec![81, 82]);
    assert_eq!(
        calls.lock().unwrap()[0],
        vec!["pane", "process-info", "--pane", pane_id]
    );
}

#[cfg(unix)]
#[test]
fn production_runner_collects_both_streams_and_bounds_execution_time() {
    use herdr_resource_monitor::herdr::ProcessCommandRunner;
    use std::time::Instant;
    let mut runner = ProcessCommandRunner;
    let output = runner
        .run(
            OsStr::new("/bin/sh"),
            &["-c", "printf fixture-out; printf fixture-error >&2"],
            Duration::from_secs(2),
        )
        .unwrap();
    assert!(output.success);
    assert_eq!(output.stdout, b"fixture-out");
    assert_eq!(output.stderr, b"fixture-error");
    let started = Instant::now();
    let error = runner
        .run(
            OsStr::new("/bin/sh"),
            &["-c", "exec sleep 5"],
            Duration::from_millis(25),
        )
        .unwrap_err();
    assert!(matches!(error, HerdrError::Timeout(_)));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[cfg(unix)]
#[test]
fn production_runner_caps_output_before_unbounded_allocation() {
    use herdr_resource_monitor::herdr::ProcessCommandRunner;
    let error = ProcessCommandRunner
        .run(
            OsStr::new("/bin/sh"),
            &["-c", "exec yes fixture"],
            Duration::from_secs(2),
        )
        .unwrap_err();
    assert!(matches!(error, HerdrError::OutputTooLarge));
}
