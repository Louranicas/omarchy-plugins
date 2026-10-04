use ask_core::{
    Id, Text,
    launch::{Command as LaunchCommand, FrozenLaunch, Harness},
};
use ask_native::{
    backend::{Command, Handle, View},
    files,
};
use std::time::{Duration, Instant};
fn launch() -> FrozenLaunch {
    FrozenLaunch {
        harness: Harness::Codex,
        cwd: "/".into(),
        command: LaunchCommand::new(vec![
            env!("CARGO_BIN_EXE_ask-native").into(),
            "--fixture-provider".into(),
        ])
        .unwrap(),
        model: None,
        reasoning: None,
    }
}
fn wait(handle: &Handle, predicate: impl Fn(&View) -> bool) -> View {
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        if let Some(v) = handle.views.take()
            && predicate(&v)
        {
            return v;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("backend fixture deadline")
}
fn pending() -> (Handle, View) {
    let h = Handle::start(launch());
    wait(&h, |v| v.ready);
    h.commands
        .try_send(Command::Submit {
            sequence: 1,
            text: Text::new("fixture 日本語").unwrap(),
        })
        .unwrap();
    let v = wait(&h, |v| !v.permissions.is_empty());
    (h, v)
}
#[test]
fn real_provider_permission_roundtrip_and_verified_shutdown() {
    let (h, v) = pending();
    let captured = v.permissions[0].clone();
    assert_eq!(
        captured.choice(true),
        Some(Id::new("fixture-allow").unwrap())
    );
    h.commands
        .try_send(Command::Answer {
            sequence: 2,
            permission: captured,
            allow: true,
        })
        .unwrap();
    let v = wait(&h, |v| !v.busy && v.transcript.len() == 2);
    assert_eq!(
        v.transcript[1].1.as_str(),
        "Fixture completed · Unicode 日本語 😀"
    );
    assert!(v.permissions.is_empty());
    assert!(h.finish());
}
#[test]
fn cancelled_permission_clears_view_and_keeps_provider_usable() {
    let (h, _) = pending();
    h.commands
        .try_send(Command::Cancel { sequence: 2 })
        .unwrap();
    let v = wait(&h, |v| !v.busy && v.permissions.is_empty());
    assert!(v.ready);
    assert!(h.finish());
}
#[test]
fn old_rendered_permission_is_rejected_by_recreated_backend() {
    let (old, v) = pending();
    let captured = v.permissions[0].clone();
    assert!(old.finish());
    let (new, _) = pending();
    new.commands
        .try_send(Command::Answer {
            sequence: 2,
            permission: captured,
            allow: true,
        })
        .unwrap();
    let v = wait(&new, |v| v.status.contains("rejected"));
    assert_eq!(v.permissions.len(), 1);
    assert!(v.busy);
    assert!(v.status.contains("Stale"));
    assert!(new.finish());
}
#[test]
fn pin_keeps_permission_instance_and_submit_ack_survives_other_actions() {
    let (h, v) = pending();
    let captured = v.permissions[0].clone();
    h.commands.try_send(Command::Pin { sequence: 2 }).unwrap();
    let pinned = wait(&h, |v| v.pinned);
    assert_eq!(pinned.permissions[0], captured);
    assert_eq!(pinned.ack.unwrap().sequence, 1);
    assert!(h.finish());
}
#[test]
fn scoped_search_selection_and_text_preview_share_generation_fence() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("fixture.txt"),
        "source-derived preview 日本語",
    )
    .unwrap();
    let h = files::Handle::start(root.path().to_owned());
    let generation = h.submit(files::Kind::Search("fixture".into())).unwrap();
    let end = Instant::now() + Duration::from_secs(2);
    let selected = loop {
        if let Some(update) = h.updates.take() {
            assert_eq!(update.generation, generation);
            let files::Result::Search(rows) = update.result else {
                panic!("search result")
            };
            assert_eq!(rows.rows.len(), 1);
            break rows.rows[0].clone();
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(2));
    };
    let preview_generation = h.submit(files::Kind::Preview(selected)).unwrap();
    assert!(preview_generation > generation);
    loop {
        if let Some(update) = h.updates.take() {
            assert_eq!(update.generation, preview_generation);
            let files::Result::Preview(ask_preview::Preview::Text { text, .. }) = update.result
            else {
                panic!("text result")
            };
            assert_eq!(text, "source-derived preview 日本語");
            break;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(h.finish());
}

fn file_selection(worker: &files::Handle) -> files::Selection {
    worker
        .submit(files::Kind::Search("fixture".into()))
        .unwrap();
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(update) = worker.updates.take() {
            if let files::Result::Search(rows) = update.result {
                return rows.rows[0].clone();
            }
            panic!("expected search results");
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(2));
    }
}
#[test]
fn foreign_worker_selection_cannot_cross_explicit_root() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    std::fs::write(a.path().join("fixture.txt"), "authorized A").unwrap();
    std::fs::write(b.path().join("fixture.txt"), "foreign B").unwrap();
    let first = files::Handle::start(a.path().into());
    let second = files::Handle::start(b.path().into());
    let own = file_selection(&first);
    let foreign = file_selection(&second);
    assert_eq!(
        first.submit(files::Kind::Preview(foreign)),
        Err("Stale or foreign file selection")
    );
    // Rejection must not invalidate a genuine currently displayed selection.
    assert!(first.submit(files::Kind::Preview(own)).is_ok());
    assert!(first.finish());
    assert!(second.finish());
}
#[test]
fn old_rendered_file_selection_rejects_after_new_search() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("fixture.txt"), "current").unwrap();
    let h = files::Handle::start(root.path().into());
    let old = file_selection(&h);
    let current = file_selection(&h);
    assert_eq!(
        h.submit(files::Kind::Preview(old)),
        Err("Stale or foreign file selection")
    );
    assert!(h.submit(files::Kind::Preview(current)).is_ok());
    assert!(h.finish());
}

#[test]
fn same_search_allows_multiple_distinct_previews() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("fixture-a.txt"), "A").unwrap();
    std::fs::write(root.path().join("fixture-b.txt"), "B").unwrap();
    let h = files::Handle::start(root.path().into());
    h.submit(files::Kind::Search("fixture".into())).unwrap();
    let end = Instant::now() + Duration::from_secs(2);
    let rows = loop {
        if let Some(update) = h.updates.take() {
            let files::Result::Search(rows) = update.result else {
                panic!("search");
            };
            break rows.rows;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(rows.len(), 2);
    let mut texts = Vec::new();
    for selected in rows {
        let generation = h.submit(files::Kind::Preview(selected)).unwrap();
        loop {
            if let Some(update) = h.updates.take() {
                assert_eq!(update.generation, generation);
                let files::Result::Preview(ask_preview::Preview::Text { text, .. }) = update.result
                else {
                    panic!("preview");
                };
                texts.push(text);
                break;
            }
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    texts.sort();
    assert_eq!(texts, ["A", "B"]);
    assert!(h.finish());
}
