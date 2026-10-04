//! Real executable preflight controls; no desktop, live provider or credentials required.
use std::{fs, os::unix::fs::PermissionsExt, sync::atomic::AtomicBool, time::Duration};
fn run(args: &[&str]) -> omarchy_process_runner::Output {
    omarchy_process_runner::run(
        &omarchy_process_runner::Request {
            program: env!("CARGO_BIN_EXE_ask-native").into(),
            arguments: args.iter().map(Into::into).collect(),
            directory: "/".into(),
            environment: vec![("OPENAI_API_KEY".into(), "synthetic-not-a-credential".into())],
            timeout: Duration::from_secs(3),
            output_limit: 16_384,
        },
        &AtomicBool::new(false),
    )
    .unwrap()
}
#[test]
fn check_provider_is_redacted_and_never_executes_or_requires_auth() {
    let t = tempfile::tempdir().unwrap();
    let sentinel = t.path().join("must-not-exist");
    let profile = t.path().join("profile.json");
    let config = serde_json::json!({"version":1,"harness":"codex","adapter_argv":["/usr/bin/python3","-c",format!("open({:?}, 'w').write('executed')",sentinel.to_str().unwrap())],"harness_executable":"/usr/bin/python3","cwd":t.path(),"model":"private-model","inherit_env":["HOME"]});
    fs::write(&profile, serde_json::to_vec(&config).unwrap()).unwrap();
    fs::set_permissions(&profile, fs::Permissions::from_mode(0o600)).unwrap();
    let out = run(&["--check-provider", profile.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let status: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(status["authentication"], "not_checked");
    assert_eq!(status["environment_availability"], "not_checked");
    assert_eq!(status["policy"], "interactive_only");
    for secret in [
        "synthetic-not-a-credential",
        "private-model",
        profile.to_str().unwrap(),
        sentinel.to_str().unwrap(),
    ] {
        assert!(!text.contains(secret));
    }
    assert!(out.stderr.is_empty());
    assert!(!sentinel.exists());
}
#[test]
fn configured_provider_cannot_bypass_managed_presentation() {
    let out = run(&["--provider-config", "/this/profile/is/not/read"]);
    assert!(!out.status.success());
    assert_eq!(
        String::from_utf8(out.stderr).unwrap().trim(),
        "managed presentation required for configured provider"
    );
}
