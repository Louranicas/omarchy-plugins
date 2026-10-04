//! Authenticated presenter peer fixtures: new model reads and bounded page restart.
use modal_client::presenter::{Auth, Counter};
use modal_runtime::socket::ControlSocket;
use std::{
    os::unix::fs::PermissionsExt,
    thread,
    time::{Duration, Instant},
};
use yoohoo_runtime::{
    Command, Runtime,
    presenter::{Link, Operation, Reply, ResultBody, decode, page},
};
fn identity() -> desktop_io::ProcessIdentity {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    desktop_io::ProcessIdentity {
        pid: std::process::id(),
        start_ticks: stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap(),
    }
}
fn scenario(mode: &str) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = ControlSocket::bind(dir.path()).unwrap();
    let auth:Auth=serde_json::from_value(serde_json::json!({"epoch":vec![1;16],"client":vec![2;16],"client_epoch":"1","generation":"1","namespace":format!("omarchy-modal-{}","a".repeat(32))})).unwrap();
    let peer_auth = auth.clone();
    let mode = mode.to_owned();
    let test_mode = mode.clone();
    let server = thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(2);
        let mut peer = loop {
            match socket.accept() {
                Ok(p) => break p,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < end);
                    thread::sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("{e}"),
            }
        };
        let mut runtime = Runtime::fixture().unwrap();
        runtime.execute(Command::Open, 1).unwrap();
        let mut view = runtime.view();
        let mut version = if mode == "rollback" { 2 } else { 1 };
        let mut restarts = 0;
        let mut completed = 0;
        let mut frames = 0;
        loop {
            let bytes = match peer.read_frame(end) {
                Ok(bytes) => bytes,
                Err(_) if mode == "rollback" && completed == 1 => break,
                Err(error) => panic!("{error}"),
            };
            let request = decode(&bytes).unwrap();
            assert_eq!(request.auth, peer_auth);
            frames += 1;
            let Operation::Read { cursor, revision } = request.command else {
                panic!("unexpected mutation")
            };
            if cursor == 0 {
                assert_eq!(
                    revision, None,
                    "a new read cannot inherit old display revision"
                );
            } else {
                assert_eq!(revision, Counter::new(version));
            }
            let changing = cursor == 1
                && match mode.as_str() {
                    "forever" | "forged" => true,
                    "once" => restarts == 0,
                    _ => false,
                };
            let result = if changing {
                restarts += 1;
                if mode != "forged" {
                    version += 1;
                }
                ResultBody::Changed
            } else {
                let (p, next, cut) = page(view.clone(), cursor).unwrap();
                if next.is_none() {
                    completed += 1;
                }
                ResultBody::Page {
                    view: Box::new(p),
                    next,
                    metadata_truncated: cut,
                }
            };
            let reply = Reply {
                version: 1,
                request: request.request,
                auth: peer_auth.clone(),
                revision: Counter::new(version).unwrap(),
                result,
            };
            peer.write_frame(&serde_json::to_vec(&reply).unwrap(), end)
                .unwrap();
            if (mode == "forged" || (mode == "forever" && restarts == 3)) && changing {
                if mode == "forever" {
                    assert!(
                        peer.read_frame(Instant::now() + Duration::from_millis(80))
                            .is_err(),
                        "restart budget exceeded"
                    );
                }
                break;
            }
            if completed == 1 && mode != "new" && mode != "rollback" {
                break;
            }
            if completed == 2 {
                break;
            }
            if completed == 1 && (mode == "new" || mode == "rollback") {
                version = if mode == "rollback" { 1 } else { 2 };
                view.rows[0].count = 99;
            }
        }
        frames
    });
    let mut link = Link::connect(&dir.path().join("modal.sock"), identity(), auth).unwrap();
    let first = link.model();
    match test_mode.as_str() {
        "forged" | "forever" => {
            assert!(first.is_err());
            assert!(link.model().is_err(), "failed link cannot resume");
        }
        "rollback" => {
            assert!(first.is_ok());
            assert!(
                link.model().is_err(),
                "successful prior revision cannot roll back"
            );
            assert!(link.model().is_err(), "rollback poisons link");
        }
        "new" => {
            assert_eq!(first.unwrap().view.rows[0].count, 1);
            assert_eq!(link.model().unwrap().view.rows[0].count, 99);
        }
        _ => {
            assert_eq!(first.unwrap().view.rows.len(), 3);
        }
    }
    let frames = server.join().unwrap();
    assert_eq!(
        frames,
        match test_mode.as_str() {
            "once" => 5,
            "forged" => 2,
            "rollback" => 4,
            _ => 6,
        }
    );
}
#[test]
fn subsequent_read_accepts_new_revision() {
    scenario("new");
}
#[test]
fn changed_page_discards_partial_model_and_restarts() {
    scenario("once");
}
#[test]
fn changing_pages_exhaust_exact_two_restarts() {
    scenario("forever");
}
#[test]
fn unchanged_revision_cannot_claim_changed() {
    scenario("forged");
}

#[test]
fn fresh_model_cannot_roll_back_previous_revision() {
    scenario("rollback");
}
