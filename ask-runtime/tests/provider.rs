use ask_runtime::{
    Worker,
    provider::{ProfileError, ProviderProfile},
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
fn config() -> Value {
    json!({"version":1,"harness":"codex","adapter_argv":["/usr/bin/python3","-c","private-argument"],"harness_executable":"/usr/bin/python3","cwd":"/","model":"private-model","inherit_env":["HOME"]})
}
fn parse(v: Value) -> Result<ProviderProfile, ProfileError> {
    ProviderProfile::parse(&serde_json::to_vec(&v).unwrap())
}
#[test]
fn profile_is_explicit_frozen_redacted_and_shell_free() {
    let p = parse(config()).unwrap();
    p.check().unwrap();
    assert_eq!(p.launch().command.args()[2], "private-argument");
    let mut requested = vec![];
    let env = p
        .environment(|key| {
            requested.push(key.to_owned());
            Some("synthetic-secret".into())
        })
        .unwrap();
    assert_eq!(requested, vec!["HOME"]);
    assert_eq!(env.len(), 2);
    assert_eq!(env[1].0, "CODEX_PATH");
    assert_eq!(env[1].1, "/usr/bin/python3");
    assert!(!format!("{p:?}").contains("private"));
    let status = p.redacted_status().to_string();
    assert!(!status.contains("private"));
    assert!(status.contains("not_checked"));
}
#[test]
fn malformed_or_implicit_authority_profiles_refused() {
    for (key, value) in [
        ("version", json!(2)),
        ("harness", json!("other")),
        ("adapter_argv", json!("sh -c eval")),
        ("adapter_argv", json!([])),
        ("adapter_argv", json!(["relative"])),
        ("harness_executable", json!("relative")),
        ("cwd", json!("/tmp/../etc")),
        ("model", Value::Null),
        ("inherit_env", json!(["LD_PRELOAD"])),
        ("inherit_env", json!(["HOME", "HOME"])),
        ("permission_mode", json!("yolo")),
    ] {
        let mut v = config();
        v[key] = value;
        assert!(parse(v).is_err(), "{key}");
    }
    assert!(ProviderProfile::parse(br#"{"version":1,"version":1}"#).is_err());
}
#[test]
fn missing_explicit_environment_is_error_and_values_bounded() {
    let p = parse(config()).unwrap();
    assert_eq!(
        p.environment(|_| None),
        Err(ProfileError::MissingEnvironment)
    );
    assert_eq!(
        p.environment(|_| Some("x".repeat(65_537).into())),
        Err(ProfileError::EnvironmentLimit)
    );
    assert_eq!(
        p.environment(|_| Some("x\0y".into())),
        Err(ProfileError::EnvironmentLimit)
    );
}
#[test]
fn private_profile_read_rejects_symlinks_hardlinks_and_public_modes() {
    let t = tempfile::tempdir().unwrap();
    let f = t.path().join("profile.json");
    fs::write(&f, serde_json::to_vec(&config()).unwrap()).unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(ProviderProfile::read(&f).is_ok());
    let alias = t.path().join("alias");
    symlink(&f, &alias).unwrap();
    assert!(ProviderProfile::read(&alias).is_err());
    fs::remove_file(&alias).unwrap();
    fs::hard_link(&f, &alias).unwrap();
    assert!(ProviderProfile::read(&f).is_err());
    fs::remove_file(&alias).unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(ProviderProfile::read(&f).is_err());
}
#[test]
fn profile_read_rejects_directory_symlink_and_fifo_without_blocking() {
    let t = tempfile::tempdir().unwrap();
    let actual = t.path().join("actual");
    fs::create_dir(&actual).unwrap();
    let f = actual.join("profile");
    fs::write(&f, serde_json::to_vec(&config()).unwrap()).unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&actual, t.path().join("alias")).unwrap();
    assert!(ProviderProfile::read(&t.path().join("alias/profile")).is_err());
    let fifo = t.path().join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(ProviderProfile::read(&fifo).is_err());
}
#[test]
fn preflight_checks_explicit_paths_without_running_adapter() {
    let t = tempfile::tempdir().unwrap();
    let sentinel = t.path().join("sentinel");
    let mut v = config();
    v["adapter_argv"] = json!([
        "/usr/bin/python3",
        "-c",
        format!("open({:?},'w').write('unexpected')", sentinel)
    ]);
    parse(v.clone()).unwrap().check().unwrap();
    assert!(!sentinel.exists());
    v["harness_executable"] = json!(t.path().join("missing"));
    assert_eq!(
        parse(v).unwrap().check(),
        Err(ProfileError::MissingExecutable)
    );
}
#[test]
fn configured_worker_preserves_argv_cwd_environment_and_model_handshake() {
    let t = tempfile::tempdir().unwrap();
    let script = t.path().join("provider.py");
    let log = t.path().join("receipt");
    fs::write(&script,r#"import sys,os,json,signal
signal.alarm(5)
assert sys.argv[2]==';literal operand'
assert os.environ['HOME']=='synthetic-home'
assert os.environ['CODEX_PATH']=='/usr/bin/python3'
assert 'ANTHROPIC_API_KEY' not in os.environ
for line in sys.stdin:
 m=json.loads(line)
 if m.get('method')=='initialize':r={'protocolVersion':1}
 elif m.get('method')=='session/new':
  assert m['params']['cwd']==os.getcwd()
  r={'sessionId':'fixture-session','configOptions':[{'id':'model','category':'model','options':[{'value':'fixture-model'}]}]}
 elif m.get('method')=='session/set_config_option':
  assert m['params']=={'sessionId':'fixture-session','configId':'model','value':'fixture-model'}
  open(sys.argv[1],'w').write('configured');r={}
 else:continue
 print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':r}),flush=True)
"#).unwrap();
    let mut v = config();
    v["adapter_argv"] = json!(["/usr/bin/python3", script, log, ";literal operand"]);
    v["cwd"] = json!(t.path());
    v["model"] = json!("fixture-model");
    let p = parse(v).unwrap();
    p.check().unwrap();
    let env = p.environment(|_| Some("synthetic-home".into())).unwrap();
    let mut worker = Worker::spawn(p.launch().clone(), env, 0).unwrap();
    let start = Instant::now();
    let cancel = AtomicBool::new(false);
    while worker.adapter().session().phase() != ask_core::session::Phase::Ready {
        assert!(start.elapsed() < Duration::from_secs(2));
        worker
            .pump(start.elapsed().as_millis() as u64, &cancel)
            .unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(fs::read_to_string(log).unwrap(), "configured");
    worker
        .adapter_mut()
        .close(start.elapsed().as_millis() as u64)
        .unwrap();
    while worker.reap_receipt().state() != acp_transport::ReapState::Reaped {
        assert!(start.elapsed() < Duration::from_secs(3));
        worker
            .pump(start.elapsed().as_millis() as u64, &cancel)
            .unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn preflight_refuses_replaced_executable_directory_and_profile() {
    for target in ["adapter", "harness", "cwd", "profile"] {
        let t = tempfile::tempdir().unwrap();
        let adapter = t.path().join("adapter");
        let harness = t.path().join("harness");
        let cwd = t.path().join("cwd");
        for p in [&adapter, &harness] {
            fs::write(p, b"#!/bin/false\n").unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::create_dir(&cwd).unwrap();
        let profile = t.path().join("profile");
        let value = json!({"version":1,"harness":"claude","adapter_argv":[adapter],"harness_executable":harness,"cwd":cwd});
        fs::write(&profile, serde_json::to_vec(&value).unwrap()).unwrap();
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o600)).unwrap();
        let parsed = ProviderProfile::read(&profile).unwrap();
        let check = parsed.preflight().unwrap();
        check.recheck().unwrap();
        let changed = t.path().join(target);
        fs::rename(&changed, t.path().join("retained-old")).unwrap();
        if target == "cwd" {
            fs::create_dir(&changed).unwrap();
        } else {
            fs::write(&changed, b"replacement").unwrap();
            fs::set_permissions(&changed, fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(check.recheck(), Err(ProfileError::Changed), "{target}");
        if target == "profile" {
            assert!(matches!(parsed.preflight(), Err(ProfileError::Changed)));
        }
    }
}
