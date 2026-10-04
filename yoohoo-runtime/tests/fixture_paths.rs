use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    thread,
    time::Duration,
};
use yoohoo_runtime::{
    Command, Request, Runtime,
    ipc::{Server, call},
};
#[test]
fn daemon_control_roundtrip_replay_and_stale_generation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let mut server = Server::bind(dir.path(), Runtime::fixture().unwrap()).unwrap();
    let thread = thread::spawn(move || server.serve_until(Duration::from_millis(400)).unwrap());
    let socket = dir.path().join("yoohoo.sock");
    let open = call(
        &socket,
        std::process::id(),
        Request {
            version: 1,
            request_id: 1,
            command: Command::Open,
        },
    )
    .unwrap();
    assert_eq!(open.view.rows.len(), 3);
    assert!(open.view.open);
    assert!(
        call(
            &socket,
            std::process::id(),
            Request {
                version: 1,
                request_id: 1,
                command: Command::List
            }
        )
        .is_err()
    );
    let stale = call(
        &socket,
        std::process::id(),
        Request {
            version: 1,
            request_id: 2,
            command: Command::Close {
                generation: open.view.generation + 1,
            },
        },
    )
    .unwrap();
    assert!(!stale.ok && stale.view.open);
    let close = call(
        &socket,
        std::process::id(),
        Request {
            version: 1,
            request_id: 3,
            command: Command::Close {
                generation: open.view.generation,
            },
        },
    )
    .unwrap();
    assert!(close.ok && !close.view.open);
    thread.join().unwrap();
    assert!(!socket.exists());
}
#[test]
fn private_socket_singleton_and_closed_protocol() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let mut server = Server::bind(dir.path(), Runtime::fixture().unwrap()).unwrap();
    assert!(Server::bind(dir.path(), Runtime::fixture().unwrap()).is_err());
    let thread = thread::spawn(move || server.serve_until(Duration::from_millis(300)).unwrap());
    let mut socket = UnixStream::connect(dir.path().join("yoohoo.sock")).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    socket
        .write_all(
            b"{\"version\":1,\"request_id\":1,\"command\":{\"op\":\"urgent\",\"address\":\"1\"}}\n",
        )
        .unwrap();
    let mut b = [0; 1];
    assert_eq!(socket.read(&mut b).unwrap(), 0);
    thread.join().unwrap();
}
#[test]
fn duplicate_fields_and_invalid_settings_rejected() {
    for bytes in [
        br#"{"version":1,"version":2,"request_id":1,"command":{"op":"list"}}"#.as_slice(),
        br#"{"version":1,"request_id":1,"command":{"op":"close","generation":1,"generation":2}}"#,
    ] {
        assert!(serde_json::from_slice::<Request>(bytes).is_err());
    }
    let mut runtime = Runtime::fixture().unwrap();
    let before = runtime.view();
    assert!(
        runtime
            .execute(
                Command::Settings {
                    revision: before.revision,
                    settings: yoohoo_runtime::Settings {
                        sound: false,
                        volume: 2.0,
                        history: true,
                        reduced_motion: true
                    }
                },
                10
            )
            .is_err()
    );
    assert_eq!(runtime.view().revision, before.revision);
}
#[test]
fn trusted_native_adapter_handles_only_native_urgent_and_focus() {
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, net::UnixListener},
    };
    use yoohoo_runtime::native::Native;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir_all(dir.path().join("hypr/test")).unwrap();
    let request = UnixListener::bind(dir.path().join("hypr/test/.socket.sock")).unwrap();
    let event = UnixListener::bind(dir.path().join("hypr/test/.socket2.sock")).unwrap();
    let endpoint = desktop_io::Endpoint::discover(dir.path(), "test", std::process::id()).unwrap();
    let mut native = Native::connect(endpoint, [7; 16]).unwrap();
    let (mut events, _) = event.accept().unwrap();
    let requests = thread::spawn(move || {
        for response in [
            r#"[{"address":"0x1","title":"One","class":"Terminal","workspace":{"id":1,"name":"1"}}]"#,
            "{}",
        ] {
            let (mut s, _) = request.accept().unwrap();
            let mut buf = [0; 64];
            assert!(s.read(&mut buf).unwrap() > 0);
            s.write_all(response.as_bytes()).unwrap();
        }
    });
    let mut runtime = Runtime::empty().unwrap();
    native.synchronize(&mut runtime, 1).unwrap();
    requests.join().unwrap();
    events.write_all(b"notification>>1\n").unwrap();
    assert!(native.poll(&mut runtime, 2).unwrap());
    assert!(runtime.view().rows.is_empty());
    events.write_all(b"urgent>>1\n").unwrap();
    assert!(native.poll(&mut runtime, 3).unwrap());
    assert_eq!(runtime.view().rows.len(), 1);
    events.write_all(b"activewindowv2>>1\n").unwrap();
    native.poll(&mut runtime, 4).unwrap();
    assert!(runtime.view().rows.is_empty());
    drop(events);
    assert!(native.poll(&mut runtime, 5).is_err());
    assert!(runtime.view().stale);
}
