use desktop_io::{Address, Endpoint, Error, Event, MAX_EVENT, MAX_RESPONSE, Query};
use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};
static NEXT: AtomicU64 = AtomicU64::new(1);
struct Runtime {
    path: PathBuf,
}
impl Runtime {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "desktop-io-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(path.join("hypr")).unwrap();
        fs::create_dir(path.join("hypr/test_1")).unwrap();
        Self { path }
    }
    fn endpoint(&self) -> Endpoint {
        Endpoint::discover(&self.path, "test_1", std::process::id()).unwrap()
    }
    fn listener(&self, name: &str) -> UnixListener {
        UnixListener::bind(self.path.join("hypr/test_1").join(name)).unwrap()
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}
const BUDGET: Duration = Duration::from_secs(2);
#[test]
fn authenticated_layers_query_has_closed_read_only_wire() {
    let runtime = Runtime::new();
    let listener = runtime.listener(".socket.sock");
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut b = [0; 8];
        s.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"j/layers");
        s.write_all(b"{\"fixture-output\":{\"levels\":{}}}")
            .unwrap();
    });
    let result = runtime.endpoint().query(Query::Layers, BUDGET).unwrap();
    assert!(result["fixture-output"]["levels"].is_object());
    server.join().unwrap();
}
#[test]
fn authenticated_query_exact_wire_and_json() {
    let runtime = Runtime::new();
    let listener = runtime.listener(".socket.sock");
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut b = [0; 9];
        s.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"j/clients");
        s.write_all(b"[{\"address\":\"0x1\"}]").unwrap();
    });
    let result = runtime.endpoint().query(Query::Clients, BUDGET).unwrap();
    assert_eq!(result[0]["address"], "0x1");
    server.join().unwrap();
}
#[test]
fn rejects_unexpected_process_before_request() {
    let runtime = Runtime::new();
    let listener = runtime.listener(".socket.sock");
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut b = [0; 1];
        assert_eq!(s.read(&mut b).unwrap(), 0);
    });
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("10")
        .spawn()
        .unwrap();
    let endpoint = Endpoint::discover(&runtime.path, "test_1", child.id()).unwrap();
    assert!(matches!(
        endpoint.query(Query::Clients, BUDGET),
        Err(Error::Unauthenticated)
    ));
    server.join().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
}
#[test]
fn fragmented_and_batched_events_preserve_payload_commas() {
    let runtime = Runtime::new();
    let listener = runtime.listener(".socket2.sock");
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.write_all(b"openwin").unwrap();
        s.write_all(b"dow>>1,2,class,title,with,commas\nclosewindow>>1\n")
            .unwrap();
    });
    let mut events = runtime.endpoint().events(BUDGET).unwrap();
    let first = events.next_event(BUDGET).unwrap();
    assert_eq!(first.name, "openwindow");
    assert_eq!(first.payload, "1,2,class,title,with,commas");
    assert_eq!(events.next_event(BUDGET).unwrap().name, "closewindow");
    assert!(matches!(
        events.next_event(BUDGET),
        Err(Error::Disconnected)
    ));
    server.join().unwrap();
}
#[test]
fn malformed_event_poisons_stream_instead_of_skipping() {
    let runtime = Runtime::new();
    let listener = runtime.listener(".socket2.sock");
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.write_all(b"invalid\nclosewindow>>1\n").unwrap();
    });
    let mut events = runtime.endpoint().events(BUDGET).unwrap();
    assert!(matches!(events.next_event(BUDGET), Err(Error::Invalid)));
    assert!(matches!(
        events.next_event(BUDGET),
        Err(Error::Disconnected)
    ));
    server.join().unwrap();
}
#[test]
fn oversized_event_and_response_are_bounded() {
    let runtime = Runtime::new();
    let listener = runtime.listener(".socket2.sock");
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let _ = s.write_all(&vec![b'x'; MAX_EVENT + 4096]);
    });
    let mut events = runtime.endpoint().events(BUDGET).unwrap();
    assert!(matches!(events.next_event(BUDGET), Err(Error::Limit)));
    server.join().unwrap();
    let listener = runtime.listener(".socket.sock");
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut b = [0; 9];
        s.read_exact(&mut b).unwrap();
        let _ = s.write_all(&vec![b' '; MAX_RESPONSE + 1]);
    });
    assert!(matches!(
        runtime.endpoint().query(Query::Clients, BUDGET),
        Err(Error::Limit)
    ));
    server.join().unwrap();
}
#[test]
fn partial_frame_deadline_invalidates_connection() {
    let runtime = Runtime::new();
    let listener = runtime.listener(".socket2.sock");
    let (sent, received) = std::sync::mpsc::channel();
    let (done, stop) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.write_all(b"closewindow>>").unwrap();
        sent.send(()).unwrap();
        stop.recv_timeout(BUDGET).unwrap();
    });
    let mut events = runtime.endpoint().events(BUDGET).unwrap();
    received.recv_timeout(BUDGET).unwrap();
    assert!(matches!(
        events.next_event(Duration::from_millis(20)),
        Err(Error::Deadline)
    ));
    assert!(matches!(
        events.next_event(BUDGET),
        Err(Error::Disconnected)
    ));
    done.send(()).unwrap();
    server.join().unwrap();
}
#[test]
fn stalled_query_has_total_deadline() {
    let runtime = Runtime::new();
    let listener = runtime.listener(".socket.sock");
    let (done, stop) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut b = [0; 9];
        s.read_exact(&mut b).unwrap();
        stop.recv_timeout(BUDGET).unwrap();
    });
    assert!(matches!(
        runtime
            .endpoint()
            .query(Query::Clients, Duration::from_millis(30)),
        Err(Error::Deadline)
    ));
    done.send(()).unwrap();
    server.join().unwrap();
}
#[test]
fn directory_symlink_permissions_and_signature_rejected() {
    let runtime = Runtime::new();
    assert!(Endpoint::discover(&runtime.path, "../test_1", std::process::id()).is_err());
    assert!(Endpoint::discover(&runtime.path, "test_1", 0).is_err());
    fs::set_permissions(&runtime.path, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Endpoint::discover(&runtime.path, "test_1", std::process::id()).is_err());
    fs::set_permissions(&runtime.path, fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink("test_1", runtime.path.join("hypr/link")).unwrap();
    assert!(Endpoint::discover(&runtime.path, "link", std::process::id()).is_err());
}
#[test]
fn pinned_parent_survives_rename_without_redirect() {
    let runtime = Runtime::new();
    let endpoint = runtime.endpoint();
    let listener = runtime.listener(".socket.sock");
    fs::rename(
        runtime.path.join("hypr/test_1"),
        runtime.path.join("hypr/moved"),
    )
    .unwrap();
    fs::create_dir(runtime.path.join("hypr/test_1")).unwrap();
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut b = [0; 9];
        s.read_exact(&mut b).unwrap();
        s.write_all(b"[]").unwrap();
    });
    assert_eq!(
        endpoint.query(Query::Clients, BUDGET).unwrap(),
        serde_json::json!([])
    );
    server.join().unwrap();
}
#[test]
fn parsing_is_strict_and_budgets_bounded() {
    for bytes in [
        b">>x".as_slice(),
        b"x>>a\0b",
        b"x>>a\nb",
        b"x>>\xff",
        b"bad name>>x",
    ] {
        assert!(Event::parse(bytes).is_err());
    }
    assert_eq!(Address::parse("AB").unwrap().canonical(), "0xab");
    let runtime = Runtime::new();
    assert!(matches!(
        runtime.endpoint().query(Query::Clients, Duration::ZERO),
        Err(Error::Invalid)
    ));
    assert!(matches!(
        runtime.endpoint().events(Duration::from_secs(6)),
        Err(Error::Invalid)
    ));
}

#[test]
fn collected_snapshot_is_bound_to_stream_and_detects_events_during_query() {
    for intervening in [false, true] {
        let runtime = Runtime::new();
        let request = runtime.listener(".socket.sock");
        let event = runtime.listener(".socket2.sock");
        let (done, stop) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (mut e, _) = event.accept().unwrap();
            let (mut q, _) = request.accept().unwrap();
            let mut wire = [0; 9];
            q.read_exact(&mut wire).unwrap();
            if intervening {
                e.write_all(b"closewindow>>1\nopenwindow>>1,2,class,new\n")
                    .unwrap();
            }
            q.write_all(br#"[{"address":"0x1"}]"#).unwrap();
            drop(q);
            stop.recv_timeout(BUDGET).unwrap();
        });
        let endpoint = runtime.endpoint();
        let mut events = endpoint.events(BUDGET).unwrap();
        let mut snapshots = desktop_io::Snapshots::new([1; 16]).unwrap();
        let result = snapshots.collect(&endpoint, &mut events, BUDGET);
        if intervening {
            assert!(matches!(result, Err(Error::Stale)));
            assert!(snapshots.identity(Address::parse("1").unwrap()).is_none());
        } else {
            result.unwrap();
            assert!(snapshots.identity(Address::parse("1").unwrap()).is_some());
        }
        done.send(()).unwrap();
        server.join().unwrap();
    }
}
#[test]
fn nested_duplicate_json_is_rejected_over_actual_socket() {
    let runtime = Runtime::new();
    let listener = runtime.listener(".socket.sock");
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut wire = [0; 9];
        s.read_exact(&mut wire).unwrap();
        s.write_all(br#"[{"address":"0x1","address":"0x2"}]"#)
            .unwrap();
    });
    assert!(matches!(
        runtime.endpoint().query(Query::Clients, BUDGET),
        Err(Error::Invalid)
    ));
    server.join().unwrap();
}
#[test]
fn discovery_matches_expected_start_identity() {
    let runtime = Runtime::new();
    let original = runtime.endpoint().process_identity();
    Endpoint::discover_identity(&runtime.path, "test_1", original).unwrap();
    let wrong = desktop_io::ProcessIdentity {
        start_ticks: original.start_ticks + 1,
        ..original
    };
    assert!(matches!(
        Endpoint::discover_identity(&runtime.path, "test_1", wrong),
        Err(Error::Unauthenticated)
    ));
}

#[test]
fn partial_event_revokes_previously_published_snapshot() {
    let runtime = Runtime::new();
    let request = runtime.listener(".socket.sock");
    let event = runtime.listener(".socket2.sock");
    let (send_partial, receive) = std::sync::mpsc::channel();
    let (sent, ready) = std::sync::mpsc::channel();
    let (done, stop) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (mut e, _) = event.accept().unwrap();
        let (mut q, _) = request.accept().unwrap();
        let mut b = [0; 9];
        q.read_exact(&mut b).unwrap();
        q.write_all(br#"[{"address":"0x1"}]"#).unwrap();
        drop(q);
        receive.recv_timeout(BUDGET).unwrap();
        e.write_all(b"closewindow>>").unwrap();
        sent.send(()).unwrap();
        stop.recv_timeout(BUDGET).unwrap();
    });
    let endpoint = runtime.endpoint();
    let mut events = endpoint.events(BUDGET).unwrap();
    let mut snapshots = desktop_io::Snapshots::new([1; 16]).unwrap();
    snapshots.collect(&endpoint, &mut events, BUDGET).unwrap();
    let old = snapshots.identity(Address::parse("1").unwrap()).unwrap();
    send_partial.send(()).unwrap();
    ready.recv_timeout(BUDGET).unwrap();
    assert!(snapshots.poll(&mut events).unwrap().is_none());
    assert!(!snapshots.is_current(&old));
    done.send(()).unwrap();
    server.join().unwrap();
    let deadline = std::time::Instant::now() + BUDGET;
    loop {
        let closed = snapshots.poll(&mut events);
        assert!(!snapshots.is_current(&old));
        match closed {
            Err(Error::Disconnected) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1))
            }
            other => panic!("unexpected close result: {other:?}"),
        }
    }
}

#[test]
fn stable_id_focus_has_exact_closed_wire_and_ack() {
    for reply in [b"ok".as_slice(), b"ok extra", b"Invalid dispatcher"] {
        let runtime = Runtime::new();
        let listener = runtime.listener(".socket.sock");
        let reply = reply.to_vec();
        let success = reply == b"ok";
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let expected = b"/dispatch focuswindow stableid:ab";
            let mut bytes = vec![0; expected.len()];
            s.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes, expected);
            s.write_all(&reply).unwrap();
        });
        let result = runtime.endpoint().focus_stable_id(
            desktop_io::StableId::parse("000AB").unwrap(),
            desktop_io::DispatchDialect::Legacy,
            std::time::Instant::now() + BUDGET,
        );
        assert_eq!(result.is_ok(), success);
        server.join().unwrap();
    }
    assert_eq!(desktop_io::StableId::parse("0").unwrap().canonical(), "0");
    for raw in ["", "0x1", "1;exec x", "1\n", "10000000000000000"] {
        assert!(desktop_io::StableId::parse(raw).is_err());
    }
}
#[test]
fn expired_focus_deadline_never_connects() {
    let runtime = Runtime::new();
    let listener = runtime.listener(".socket.sock");
    listener.set_nonblocking(true).unwrap();
    assert!(matches!(
        runtime.endpoint().focus_stable_id(
            desktop_io::StableId::parse("1").unwrap(),
            desktop_io::DispatchDialect::Legacy,
            std::time::Instant::now()
        ),
        Err(Error::Deadline)
    ));
    assert!(matches!(listener.accept(),Err(e)if e.kind()==std::io::ErrorKind::WouldBlock));
}

#[test]
fn lua_focus_is_closed_table_expression() {
    let runtime = Runtime::new();
    let listener = runtime.listener(".socket.sock");
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let expected = b"/dispatch hl.dsp.focus({window=\"stableid:ab\"})";
        let mut bytes = vec![0; expected.len()];
        s.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, expected);
        s.write_all(b"ok").unwrap();
    });
    runtime
        .endpoint()
        .focus_stable_id(
            desktop_io::StableId::parse("AB").unwrap(),
            desktop_io::DispatchDialect::Lua,
            std::time::Instant::now() + BUDGET,
        )
        .unwrap();
    server.join().unwrap();
}

#[test]
fn closed_modal_submaps_have_exact_dialect_wires() {
    use desktop_io::{DispatchDialect, ModalSubmap};
    for (submap, name) in [
        (ModalSubmap::Vimarchy, "vimarchy"),
        (ModalSubmap::VimarchyDouble, "vimarchy-double"),
        (ModalSubmap::Ask, "omarchy-ask"),
        (ModalSubmap::Reset, "reset"),
    ] {
        for dialect in [DispatchDialect::Legacy, DispatchDialect::Lua] {
            let runtime = Runtime::new();
            let listener = runtime.listener(".socket.sock");
            let wire = match dialect {
                DispatchDialect::Legacy => format!("/dispatch submap {name}"),
                DispatchDialect::Lua => format!("/dispatch hl.dsp.submap(\"{name}\")"),
            };
            let server = thread::spawn(move || {
                let (mut peer, _) = listener.accept().unwrap();
                let mut bytes = vec![0; wire.len()];
                peer.read_exact(&mut bytes).unwrap();
                assert_eq!(bytes, wire.as_bytes());
                peer.write_all(b"ok").unwrap();
            });
            runtime
                .endpoint()
                .set_modal_submap(submap, dialect, std::time::Instant::now() + BUDGET)
                .unwrap();
            server.join().unwrap();
        }
    }
}
#[test]
fn submap_query_is_read_only_json() {
    let runtime = Runtime::new();
    let listener = runtime.listener(".socket.sock");
    let server = thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        let mut bytes = [0; 8];
        peer.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"j/submap");
        peer.write_all(br#"{"submap":"default"}"#).unwrap();
    });
    assert_eq!(
        runtime.endpoint().query(Query::Submap, BUDGET).unwrap()["submap"],
        "default"
    );
    server.join().unwrap();
}

#[test]
fn maximize_closed_lua_wire_set_unset_and_ack_validation() {
    for (set, reply, success) in [
        (true, b"ok".as_slice(), true),
        (false, b"ok".as_slice(), true),
        (true, b"ok extra".as_slice(), false),
    ] {
        let runtime = Runtime::new();
        let listener = runtime.listener(".socket.sock");
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let expected = format!(
                "/dispatch hl.dsp.window.fullscreen({{mode=\"maximized\",action=\"{}\",layout_aware=false,window=\"stableid:ab\"}})",
                if set { "set" } else { "unset" }
            );
            let mut b = vec![0; expected.len()];
            socket.read_exact(&mut b).unwrap();
            assert_eq!(b, expected.as_bytes());
            socket.write_all(reply).unwrap();
        });
        assert_eq!(
            runtime
                .endpoint()
                .set_maximized(
                    desktop_io::StableId::parse("AB").unwrap(),
                    set,
                    desktop_io::DispatchDialect::Lua,
                    std::time::Instant::now() + BUDGET
                )
                .is_ok(),
            success
        );
        server.join().unwrap();
    }
    let runtime = Runtime::new();
    assert!(matches!(
        runtime.endpoint().set_maximized(
            desktop_io::StableId::parse("ab").unwrap(),
            true,
            desktop_io::DispatchDialect::Legacy,
            std::time::Instant::now() + BUDGET
        ),
        Err(Error::Invalid)
    ));
}

#[test]
fn workspace_move_closed_numeric_wire_and_exact_ack() {
    for (destination, follow, reply, success) in [
        (1, true, b"ok".as_slice(), true),
        (10, false, b"ok".as_slice(), true),
        (2, true, b"ok trailing".as_slice(), false),
    ] {
        let runtime = Runtime::new();
        let listener = runtime.listener(".socket.sock");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let expected = format!(
                "/dispatch hl.dsp.window.move({{workspace=\"{destination}\",follow={follow},window=\"stableid:ab\"}})"
            );
            let mut bytes = vec![0; expected.len()];
            stream.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes, expected.as_bytes());
            stream.write_all(reply).unwrap();
        });
        assert_eq!(
            runtime
                .endpoint()
                .move_to_workspace(
                    desktop_io::StableId::parse("ab").unwrap(),
                    destination,
                    follow,
                    desktop_io::DispatchDialect::Lua,
                    std::time::Instant::now() + BUDGET
                )
                .is_ok(),
            success
        );
        server.join().unwrap();
    }
    let runtime = Runtime::new();
    for destination in [0, 11, 255] {
        assert!(matches!(
            runtime.endpoint().move_to_workspace(
                desktop_io::StableId::parse("ab").unwrap(),
                destination,
                false,
                desktop_io::DispatchDialect::Lua,
                std::time::Instant::now() + BUDGET
            ),
            Err(Error::Invalid)
        ));
    }
    assert!(matches!(
        runtime.endpoint().move_to_workspace(
            desktop_io::StableId::parse("ab").unwrap(),
            1,
            false,
            desktop_io::DispatchDialect::Legacy,
            std::time::Instant::now() + BUDGET
        ),
        Err(Error::Invalid)
    ));
}
