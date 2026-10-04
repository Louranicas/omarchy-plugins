use desktop_io::ProcessIdentity;
use modal_contract::{AuthorityEpoch, ClientId, Fence, Owner};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    num::NonZeroU64,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    thread,
    time::Duration,
};
use vimarchy_runtime::presenter::{Auth, Input, Link};
fn identity() -> ProcessIdentity {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    ProcessIdentity {
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
fn auth() -> Auth {
    Auth::new(
        Fence {
            authority_epoch: AuthorityEpoch([1; 16]),
            owner: Owner {
                client: ClientId([2; 16]),
                client_epoch: NonZeroU64::new(1).unwrap(),
            },
            generation: NonZeroU64::new(1).unwrap(),
        },
        format!("omarchy-modal-{}", "a".repeat(32)),
    )
    .unwrap()
}
#[test]
fn reply_fence_revision_pagination_and_geometry_fail_closed() {
    for mutation in 0..13 {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("modal.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let mut reader = BufReader::new(peer.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let mut reply = json!({"version":1,"request":request["request"],"auth":request["auth"],"revision":"1","result":{"status":"page","rows":[{"kind":"output","name":"HEADLESS-1","origin":[0,0],"size":[1280,720]}],"next":null,"phase":"selecting"}});
            match mutation {
                0 => reply["auth"]["generation"] = json!("2"),
                1 => {
                    reply["auth"]["namespace"] = json!(format!("omarchy-modal-{}", "b".repeat(32)))
                }
                2 => reply["request"] = json!("2"),
                3 => reply["result"]["next"] = json!(0),
                4 => reply["result"]["rows"][0]["size"] = json!([0, 720]),
                5 => {
                    reply["result"]["rows"][0] = json!({"kind":"hint","hint":"a","output":"absent","at":[0,0],"size":[1,1],"focused":false})
                }
                6 => {
                    reply["result"]["rows"] =
                        json!([reply["result"]["rows"][0], reply["result"]["rows"][0]])
                }
                7 => reply["version"] = json!(2),
                8 => reply["result"]["phase"] = json!("radial"), // missing source
                9 => reply["result"]["rows"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"kind":"radial","source_hint":"a","current_workspace":1})), // outside phase
                10 => {
                    reply["result"]["phase"] = json!("radial");
                    reply["result"]["rows"].as_array_mut().unwrap().push(
                        json!({"kind":"radial","source_hint":"absent","current_workspace":1}),
                    );
                }
                11 => {
                    reply["result"]["phase"] = json!("radial");
                    reply["result"]["rows"].as_array_mut().unwrap().extend([json!({"kind":"hint","hint":"a","output":"HEADLESS-1","at":[0,0],"size":[1,1],"focused":false}),json!({"kind":"radial","source_hint":"a","current_workspace":1}),json!({"kind":"radial","source_hint":"a","current_workspace":1})]);
                }
                _ => {
                    reply["result"]["phase"] = json!("radial");
                    reply["result"]["rows"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!({"kind":"radial","source_hint":"a","current_workspace":"1"}));
                }
            }
            writeln!(peer, "{reply}").unwrap();
        });
        let mut link = Link::connect(&path, identity(), auth()).unwrap();
        assert!(link.model().is_err(), "mutation {mutation}");
        assert!(link.model().is_err(), "must remain poisoned");
        assert!(link.input(Input::Cancel).is_err());
        server.join().unwrap();
    }
}
