use ask_core::{Error, Id, RequestId, Text, config::*, launch::*, permission::*, session::*};
use std::{collections::BTreeSet, path::Path};
fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn text(s: &str) -> Text {
    Text::new(s).unwrap()
}
fn launch() -> FrozenLaunch {
    FrozenLaunch {
        harness: Harness::Codex,
        cwd: "/project".into(),
        command: Command::new(vec!["/bin/installed-adapter".into()]).unwrap(),
        model: None,
        reasoning: None,
    }
}
fn options() -> Vec<Offered> {
    vec![
        Offered {
            id: id("real-allow-id"),
            kind: Kind::AllowOnce,
        },
        Offered {
            id: id("real-deny-id"),
            kind: Kind::RejectOnce,
        },
    ]
}
fn req(n: i64) -> RequestId {
    RequestId::Number(n)
}
fn ready(steering: bool) -> Session {
    let (mut s, _) = Session::new(launch(), 1, 0).unwrap();
    let effect = s.initialized(1, 1, 1, steering, 1).unwrap();
    let Effect::NewSession { operation, .. } = effect else {
        panic!()
    };
    s.created(1, operation, id("provider-session"), vec![], 2)
        .unwrap();
    s
}
fn model_options() -> Vec<ConfigOption> {
    vec![
        ConfigOption {
            id: id("model"),
            category: Some(id("model")),
            values: vec![id("exact-model")],
        },
        ConfigOption {
            id: id("reasoning_effort"),
            category: Some(id("thought_level")),
            values: vec![id("high"), id("low")],
        },
    ]
}

#[test]
fn harness_explicit_precedence_and_read_failure_are_distinct() {
    assert_eq!(
        resolve_harness(Some(" claude "), Err(Error::ReadFailed)),
        Ok(Harness::Claude)
    );
    assert_eq!(
        resolve_harness(None, Ok(Some("codex\n"))),
        Ok(Harness::Codex)
    );
    assert_eq!(resolve_harness(None, Ok(None)), Err(Error::MissingHarness));
    assert_eq!(
        resolve_harness(None, Err(Error::ReadFailed)),
        Err(Error::ReadFailed)
    );
    assert_eq!(
        resolve_harness(Some("gemini"), Ok(Some("codex"))),
        Err(Error::Unsupported)
    );
}
#[test]
fn executable_override_never_falls_back_and_empty_path_entries_are_ignored() {
    let paths = executable_candidates(Harness::Codex, Some("custom"), ":/a::/b:").unwrap();
    assert_eq!(
        paths,
        vec![std::path::PathBuf::from("/a/custom"), "/b/custom".into()]
    );
    assert_eq!(
        choose_executable(&paths, |_| false),
        Err(Error::MissingExecutable)
    );
    let paths = executable_candidates(Harness::Claude, Some("/missing"), "/bin").unwrap();
    assert_eq!(paths, vec![std::path::PathBuf::from("/missing")]);
    assert_eq!(
        choose_executable(&paths, |p| p == Path::new("/bin/claude")),
        Err(Error::MissingExecutable)
    );
}
#[test]
fn acp_specific_override_wins_without_shell_reinterpretation() {
    let default = Command::new(vec!["default".into()]).unwrap();
    let command = acp_command(
        Some(r#"["/custom","$(touch no)","two words"]"#),
        Some(r#"["shared"]"#),
        default.clone(),
    )
    .unwrap();
    assert_eq!(command.args(), ["/custom", "$(touch no)", "two words"]);
    assert_eq!(
        acp_command(Some("  "), Some(r#"["shared"]"#), default.clone()).unwrap(),
        default
    );
    for bad in ["invalid", "[]", "[1]", r#"["a",""]"#, r#"{"command":"a"}"#] {
        assert_eq!(
            acp_command(Some(bad), None, default.clone()),
            Err(Error::InvalidConfig)
        );
    }
}
#[test]
fn malformed_bridge_override_is_not_silent_node_fallback() {
    assert_eq!(
        bridge_prefix(Some("bad"), Command::new(vec!["node".into()]).unwrap()),
        Err(Error::InvalidConfig)
    );
}
#[test]
fn path_operands_preserve_quotes_newlines_and_leading_dash() {
    let c = Command::new(vec!["editor".into(), "--".into()]).unwrap();
    let path = "-quote'\n$(no)雪";
    assert_eq!(
        c.append_operand(path).unwrap().args(),
        ["editor", "--", path]
    );
    assert!(c.append_operand("bad\0path").is_err());
}
#[test]
fn cwd_compatibility_is_explicit_and_absolute() {
    assert_eq!(
        cwd(Some("/chosen"), Some("/home/user"), Path::new("/fallback")).unwrap(),
        Path::new("/chosen")
    );
    assert_eq!(
        cwd(None, Some("/home/user"), Path::new("/fallback")).unwrap(),
        Path::new("/home/user")
    );
    assert!(cwd(Some("relative"), None, Path::new("/fallback")).is_err());
}
#[test]
fn strict_config_rejects_duplicate_nested_keys_and_nonobject() {
    for bad in [
        r#"{"x":1,"x":2}"#,
        r#"{"x":{"y":1,"y":2}}"#,
        "[]",
        "null",
        r#"{"fontScale":null}"#,
        r#"{"permissionMode":"maybe"}"#,
        r#"{"searchDebounceMs":-1}"#,
    ] {
        assert!(Preferences::import(bad.as_bytes()).is_err(), "{bad}");
    }
}
#[test]
fn explicit_legacy_yolo_is_preserved_and_unknown_config_survives() {
    let p=Preferences::import(br#"{"permissionMode":"yolo","fontScale":1.2,"vendor":{"flag":true},"fileOpenCommand":"tool with spaces"}"#).unwrap();
    assert_eq!(p.permission, LegacyPolicy::ExplicitYolo);
    assert_eq!(p.file_open.as_ref().unwrap().args(), ["tool with spaces"]);
    let out = p.export();
    assert_eq!(out["vendor"]["flag"], true);
    assert_eq!(out["permissionMode"], "yolo");
    assert_eq!(
        Preferences::import(b"{}").unwrap().permission,
        LegacyPolicy::Ask
    );
}
#[test]
fn config_size_and_depth_are_bounded() {
    assert_eq!(
        strict_object(&vec![b' '; 65_537]),
        Err(Error::LimitExceeded)
    );
    let nested = format!("{{\"x\":{}0{}}}", "[".repeat(34), "]".repeat(34));
    assert_eq!(strict_object(nested.as_bytes()), Err(Error::LimitExceeded));
}
#[test]
fn debug_diagnostics_do_not_echo_prompt_or_exact_ids() {
    assert!(!format!("{:?} {:?}", text("SECRET"), id("SECRET")).contains("SECRET"));
}

#[test]
fn startup_is_ordered_and_ready_only_after_both_configuration_acks() {
    let mut l = launch();
    l.model = Some(id("exact-model"));
    l.reasoning = Some(id("high"));
    let (mut s, _) = Session::new(l, 1, 0).unwrap();
    assert_eq!(s.created(1, 1, id("p"), vec![], 1), Err(Error::WrongPhase));
    let Effect::NewSession { operation, .. } = s.initialized(1, 1, 1, true, 1).unwrap() else {
        panic!()
    };
    let Effect::SetConfig {
        operation,
        config,
        value,
        ..
    } = s
        .created(1, operation, id("p"), model_options(), 2)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!((config, value), (id("model"), id("exact-model")));
    assert_eq!(s.phase(), Phase::Configuring);
    let Effect::SetConfig {
        operation,
        config,
        value,
        ..
    } = s.configured(1, operation, model_options(), 3).unwrap()
    else {
        panic!()
    };
    assert_eq!((config, value), (id("reasoning_effort"), id("high")));
    assert!(matches!(
        s.configured(1, operation, model_options(), 4).unwrap(),
        Effect::Ready { .. }
    ));
}
#[test]
fn unsupported_protocol_and_missing_model_fail_visibly() {
    let (mut s, _) = Session::new(launch(), 1, 0).unwrap();
    assert_eq!(s.initialized(1, 1, 2, false, 1), Err(Error::Unsupported));
    assert_eq!(s.phase(), Phase::Lost);
    let mut l = launch();
    l.model = Some(id("absent"));
    let (mut s, _) = Session::new(l, 1, 0).unwrap();
    s.initialized(1, 1, 1, false, 1).unwrap();
    assert_eq!(
        s.created(1, 2, id("p"), model_options(), 2),
        Err(Error::UnknownOption)
    );
    assert_eq!(s.phase(), Phase::Lost);
}
#[test]
fn claude_exact_version_override_requires_model_category() {
    let mut l = launch();
    l.harness = Harness::Claude;
    l.model = Some(id("versioned-opus"));
    let (mut s, _) = Session::new(l, 1, 0).unwrap();
    s.initialized(1, 1, 1, false, 1).unwrap();
    assert!(
        matches!(s.created(1,2,id("p"),model_options(),2).unwrap(),Effect::SetConfig{value,..}if value==id("versioned-opus"))
    );
}
#[test]
fn startup_deadline_cannot_be_renewed_by_partial_progress() {
    let (mut s, _) = Session::new(launch(), 1, 0).unwrap();
    s.initialized(1, 1, 1, false, 9999).unwrap();
    assert_eq!(
        s.created(1, 2, id("p"), vec![], 10_000),
        Err(Error::Timeout)
    );
    s.tick(10_000).unwrap();
    assert_eq!(s.phase(), Phase::Closing(CloseStage::Graceful));
}
#[test]
fn stale_startup_operation_is_not_accepted() {
    let (mut s, _) = Session::new(launch(), 1, 0).unwrap();
    assert_eq!(s.initialized(1, 2, 1, false, 1), Err(Error::StaleOperation));
    assert_eq!(s.phase(), Phase::Initializing);
}
#[test]
fn duplicate_busy_submit_does_not_clear_active_turn() {
    let mut s = ready(false);
    s.submit(1, id("first"), text("prompt"), 3).unwrap();
    assert_eq!(
        s.submit(1, id("second"), text("another"), 4),
        Err(Error::Busy)
    );
    assert_eq!(s.active_turn(), Some(&id("first")));
    s.chunk(1, &id("first"), 1, None, text("still alive"), 5)
        .unwrap();
}
#[test]
fn stream_paragraphs_follow_exact_nonempty_message_id_boundaries() {
    let mut s = ready(false);
    s.submit(1, id("turn"), text("human"), 3).unwrap();
    for (seq, message_id, chunk) in [
        (1, Some(id("a")), "one"),
        (2, None, " two"),
        (3, Some(id("b")), "three"),
        (4, Some(id("b")), " four"),
    ] {
        s.chunk(1, &id("turn"), seq, message_id, text(chunk), 3 + seq)
            .unwrap();
    }
    assert_eq!(s.messages().len(), 3);
    assert_eq!(s.messages()[1].body.as_str(), "one two");
    assert_eq!(s.messages()[2].body.as_str(), "three four");
}
#[test]
fn reordered_and_duplicate_chunks_leave_transcript_unchanged() {
    let mut s = ready(false);
    s.submit(1, id("t"), text("human"), 3).unwrap();
    assert_eq!(
        s.chunk(1, &id("t"), 2, None, text("bad"), 4),
        Err(Error::SequenceGap)
    );
    assert!(s.messages()[1].body.is_empty());
    s.chunk(1, &id("t"), 1, None, text("good"), 4).unwrap();
    assert_eq!(
        s.chunk(1, &id("t"), 1, None, text("duplicate"), 5),
        Err(Error::Duplicate)
    );
    assert_eq!(s.messages()[1].body.as_str(), "good");
}
#[test]
fn steering_is_advertised_and_one_pending_operation_only() {
    let mut s = ready(false);
    s.submit(1, id("t"), text("human"), 3).unwrap();
    assert_eq!(
        s.steer(1, &id("t"), text("fix"), 4),
        Err(Error::Unsupported)
    );
    let mut s = ready(true);
    s.submit(1, id("t"), text("human"), 3).unwrap();
    let Effect::Steer { operation, .. } = s.steer(1, &id("t"), text("fix"), 4).unwrap() else {
        panic!()
    };
    assert_eq!(s.steer(1, &id("t"), text("again"), 5), Err(Error::Busy));
    assert_eq!(
        s.steered(1, &id("t"), operation + 1, 6),
        Err(Error::StaleOperation)
    );
    s.steered(1, &id("t"), operation, 6).unwrap();
    assert!(s.steer(1, &id("t"), text("new"), 7).is_ok());
}
#[test]
fn pin_preserves_provider_turn_and_pending_permission() {
    let mut s = ready(true);
    s.submit(1, id("t"), text("human"), 3).unwrap();
    s.permission(1, &id("t"), req(7), options(), None, 4)
        .unwrap();
    s.pin().unwrap();
    assert_eq!(s.provider_session(), Some(&id("provider-session")));
    assert_eq!(s.active_turn(), Some(&id("t")));
    assert_eq!(s.permissions().pending_count(), 1);
    assert_eq!(s.presentation(), Presentation::Pinned);
}
#[test]
fn completion_attention_is_only_unfocused_pinned_and_never_focus() {
    let mut s = ready(false);
    s.pin().unwrap();
    s.set_focused(false);
    s.submit(1, id("t"), text("human"), 3).unwrap();
    assert_eq!(
        s.finished(1, &id("t"), 4).unwrap(),
        vec![Effect::RequestAttention]
    );
    let mut overlay = ready(false);
    overlay.set_focused(false);
    overlay.submit(1, id("t"), text("human"), 3).unwrap();
    assert!(overlay.finished(1, &id("t"), 4).unwrap().is_empty());
}
#[test]
fn cancellation_resolves_permissions_and_waits_for_end_before_new_prompt() {
    let mut s = ready(false);
    s.submit(1, id("t"), text("human"), 3).unwrap();
    s.permission(1, &id("t"), req(1), options(), None, 4)
        .unwrap();
    let out = s.cancel_turn(1, &id("t"), 5).unwrap();
    assert!(out.contains(&Effect::Permission(Response {
        request: req(1),
        outcome: Outcome::Cancelled
    })));
    assert_eq!(s.submit(1, id("next"), text("human"), 6), Err(Error::Busy));
    assert_eq!(
        s.permission(1, &id("t"), req(2), options(), None, 6),
        Err(Error::Busy)
    );
    s.finished(1, &id("t"), 7).unwrap();
    assert!(s.submit(1, id("next"), text("human"), 8).is_ok());
}
#[test]
fn cancellation_timeout_starts_bounded_child_shutdown() {
    let mut s = ready(false);
    s.submit(1, id("t"), text("human"), 3).unwrap();
    s.cancel_turn(1, &id("t"), 4).unwrap();
    assert!(s.tick(5003).unwrap().is_empty());
    assert!(matches!(
        s.tick(5004).unwrap().as_slice(),
        [Effect::CloseSession { .. }]
    ));
    assert_eq!(
        s.tick(5354).unwrap(),
        vec![Effect::SignalTerm { generation: 1 }]
    );
    assert_eq!(
        s.tick(5854).unwrap(),
        vec![Effect::SignalKill { generation: 1 }]
    );
    assert_eq!(
        s.tick(6004).unwrap(),
        vec![Effect::ReapOutstanding { generation: 1 }]
    );
    assert_eq!(s.phase(), Phase::Closing(CloseStage::Kill));
    s.child_exited(1, 6005).unwrap();
    assert_eq!(s.phase(), Phase::Closed);
}
#[test]
fn restart_fences_old_generation_without_replaying_transcript() {
    let mut s = ready(true);
    s.submit(1, id("old-turn"), text("SECRET"), 3).unwrap();
    s.lost(1, 4).unwrap();
    assert_eq!(s.restart(launch(), false, 5), Err(Error::WrongPhase));
    let effect = s.restart(launch(), true, 5).unwrap();
    assert!(matches!(effect, Effect::Initialize { generation: 2, .. }));
    assert!(s.messages().is_empty());
    assert_eq!(
        s.chunk(1, &id("old-turn"), 1, None, text("late"), 6),
        Err(Error::StaleGeneration)
    );
    assert!(s.messages().is_empty());
}
#[test]
fn close_cancels_requests_and_drops_local_transcript() {
    let mut s = ready(false);
    s.submit(1, id("t"), text("SECRET"), 3).unwrap();
    s.permission(1, &id("t"), req(1), options(), None, 4)
        .unwrap();
    let out = s.close(5).unwrap();
    assert!(out.contains(&Effect::Permission(Response {
        request: req(1),
        outcome: Outcome::Cancelled
    })));
    assert!(s.messages().is_empty());
    assert!(s.close(6).unwrap().is_empty());
    assert_eq!(
        s.answer_permission(
            1,
            s.permissions().permission_epoch(),
            &req(1),
            Some(&id("real-allow-id")),
            7
        ),
        Err(Error::WrongPhase)
    );
}
#[test]
fn closing_overlay_does_not_close_pinned_session() {
    let mut sessions = Conversations::default();
    let (first, _) = sessions.create_overlay(launch(), 1, 0).unwrap();
    sessions.pin(first).unwrap();
    let (second, _) = sessions.create_overlay(launch(), 1, 1).unwrap();
    sessions.close_overlay(2).unwrap();
    assert_eq!(sessions.get(first).unwrap().phase(), Phase::Initializing);
    assert_eq!(
        sessions.get(second).unwrap().phase(),
        Phase::Closing(CloseStage::Graceful)
    );
    assert_eq!(sessions.overlay(), None);
}
#[test]
fn transcript_ceiling_prevents_unbounded_stream_growth() {
    let mut s = ready(false);
    s.submit(1, id("t"), text("x"), 3).unwrap();
    let chunk = Text::new("x".repeat(65_536)).unwrap();
    for sequence in 1..=15 {
        s.chunk(1, &id("t"), sequence, None, chunk.clone(), 3 + sequence)
            .unwrap();
    }
    assert_eq!(
        s.chunk(1, &id("t"), 16, None, chunk, 20),
        Err(Error::LimitExceeded)
    );
    assert_eq!(s.messages()[1].body.len(), 15 * 65_536);
}

#[test]
fn permission_exact_offered_ids_and_number_string_requests_remain_distinct() {
    let mut b = Broker::new(1);
    b.request(1, req(7), options(), None, 0).unwrap();
    let string = RequestId::String(id("7"));
    b.request(1, string.clone(), options(), None, 0).unwrap();
    assert_eq!(b.pending_count(), 2);
    assert_eq!(
        b.answer_once(1, b.permission_epoch(), &req(7), true)
            .unwrap()
            .outcome,
        Outcome::Selected(id("real-allow-id"))
    );
    assert_eq!(
        b.answer_once(1, b.permission_epoch(), &string, false)
            .unwrap()
            .outcome,
        Outcome::Selected(id("real-deny-id"))
    );
}
#[test]
fn unknown_or_duplicate_option_never_resolves_request() {
    let mut b = Broker::new(1);
    let duplicate = vec![
        Offered {
            id: id("x"),
            kind: Kind::AllowOnce,
        },
        Offered {
            id: id("x"),
            kind: Kind::RejectOnce,
        },
    ];
    assert_eq!(
        b.request(1, req(1), duplicate, None, 0),
        Err(Error::Duplicate)
    );
    b.request(1, req(1), options(), None, 0).unwrap();
    assert_eq!(
        b.answer(1, b.permission_epoch(), &req(1), Some(&id("invented"))),
        Err(Error::UnknownOption)
    );
    assert_eq!(b.pending_count(), 1);
}
#[test]
fn yolo_selects_allow_once_only_and_cancels_if_absent() {
    let mut b = Broker::from_durable(1, 7, Policy::LegacyYolo, 0).unwrap();
    let offered = vec![Offered {
        id: id("forever"),
        kind: Kind::AllowAlways,
    }];
    assert_eq!(
        b.request(1, req(1), offered, None, 1)
            .unwrap()
            .unwrap()
            .outcome,
        Outcome::Cancelled
    );
    assert_eq!(
        b.request(1, req(2), options(), None, 2)
            .unwrap()
            .unwrap()
            .outcome,
        Outcome::Selected(id("real-allow-id"))
    );
}
#[test]
fn policy_commit_must_be_durable_before_resolving_pending() {
    let mut b = Broker::new(1);
    b.request(1, req(1), options(), None, 0).unwrap();
    let proposal = b.propose(1, 0, Policy::LegacyYolo, 1).unwrap();
    assert_eq!(b.policy(), &Policy::Ask);
    assert_eq!(b.pending_count(), 1);
    assert_eq!(b.request(1, req(2), options(), None, 2).unwrap(), None);
    let responses = b.committed(proposal.token, true, 3).unwrap();
    assert_eq!(responses.len(), 2);
    assert_eq!(b.revision(), 1);
    assert_eq!(b.pending_count(), 0);
}
#[test]
fn persistence_failure_preserves_policy_and_pending_requests() {
    let mut b = Broker::new(1);
    b.request(1, req(1), options(), None, 0).unwrap();
    let proposal = b.propose(1, 0, Policy::LegacyYolo, 1).unwrap();
    assert_eq!(
        b.committed(proposal.token, false, 2),
        Err(Error::PersistenceFailed)
    );
    assert_eq!(b.policy(), &Policy::Ask);
    assert_eq!(b.revision(), 0);
    assert_eq!(b.pending_count(), 1);
    assert_eq!(
        b.committed(proposal.token, true, 3),
        Err(Error::StaleOperation)
    );
}
#[test]
fn policy_cas_and_completion_operation_are_fenced() {
    let mut b = Broker::new(1);
    assert_eq!(
        b.propose(1, 1, Policy::LegacyYolo, 0),
        Err(Error::StaleRevision)
    );
    let p = b.propose(1, 0, Policy::LegacyYolo, 0).unwrap();
    let mut forged = p.token;
    forged.operation += 1;
    assert_eq!(b.committed(forged, true, 1), Err(Error::StaleOperation));
    assert_eq!(b.policy(), &Policy::Ask);
    b.committed(p.token, true, 2).unwrap();
    assert_eq!(b.committed(p.token, true, 3), Err(Error::StaleOperation));
}
#[test]
fn scoped_automation_requires_exact_project_capability_and_unexpired_time() {
    let policy = Policy::Scoped {
        project: "/project".into(),
        capabilities: BTreeSet::from([id("read")]),
        expires_ms: 10,
    };
    let mut b = Broker::from_durable(1, 1, policy, 0).unwrap();
    let yes = Some(Context {
        project: "/project".into(),
        capability: id("read"),
    });
    assert!(
        b.request(1, req(1), options(), yes.clone(), 1)
            .unwrap()
            .is_some()
    );
    assert!(
        b.request(
            1,
            req(2),
            options(),
            Some(Context {
                project: "/project/other".into(),
                capability: id("read")
            }),
            2
        )
        .unwrap()
        .is_none()
    );
    assert!(b.request(1, req(3), options(), yes, 10).unwrap().is_none());
}
#[test]
fn lowering_policy_suspends_new_automatic_decisions_during_commit() {
    let mut b = Broker::from_durable(1, 1, Policy::LegacyYolo, 0).unwrap();
    let p = b.propose(1, 1, Policy::Ask, 1).unwrap();
    assert!(b.request(1, req(1), options(), None, 2).unwrap().is_none());
    assert!(b.committed(p.token, true, 3).unwrap().is_empty());
    assert_eq!(b.pending_count(), 1);
}
#[test]
fn closed_session_rejects_late_policy_commit() {
    let mut s = ready(false);
    let p = s.propose_policy(1, 0, Policy::LegacyYolo, 3).unwrap();
    s.close(4).unwrap();
    assert_eq!(s.policy_committed(p.token, true, 5), Err(Error::WrongPhase));
    assert_eq!(s.permissions().policy(), &Policy::Ask);
}
#[test]
fn permission_generation_reset_cancels_and_prevents_old_approvals() {
    let mut b = Broker::new(1);
    b.request(1, req(1), options(), None, 0).unwrap();
    assert_eq!(
        b.reset_generation(2).unwrap(),
        vec![Response {
            request: req(1),
            outcome: Outcome::Cancelled
        }]
    );
    assert_eq!(
        b.answer_once(1, b.permission_epoch(), &req(1), true),
        Err(Error::StaleGeneration)
    );
    assert_eq!(b.reset_generation(1), Err(Error::StaleGeneration));
}
#[test]
fn pending_permission_and_seen_request_budgets_are_bounded() {
    let mut b = Broker::new(1);
    for n in 0..32 {
        b.request(1, req(n), options(), None, 0).unwrap();
    }
    assert_eq!(
        b.request(1, req(33), options(), None, 0),
        Err(Error::LimitExceeded)
    );
    b.cancel_pending();
    assert_eq!(
        b.request(1, req(0), options(), None, 0),
        Err(Error::Duplicate)
    );
    b.next_turn().unwrap();
    assert!(b.request(1, req(0), options(), None, 0).is_ok());
}
#[test]
fn absent_once_option_is_cancellation_not_invented_permission() {
    let mut b = Broker::new(1);
    b.request(
        1,
        req(1),
        vec![Offered {
            id: id("always"),
            kind: Kind::AllowAlways,
        }],
        None,
        0,
    )
    .unwrap();
    assert_eq!(
        b.answer_once(1, b.permission_epoch(), &req(1), true)
            .unwrap()
            .outcome,
        Outcome::Cancelled
    );
}

#[test]
fn launch_precedence_matches_40_cases_executed_by_pinned_javascript() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/legacy-launch.json")).unwrap();
    let commands = fixture["command_cases"].as_array().unwrap();
    assert_eq!(commands.len(), 20);
    for case in commands {
        let result = acp_command(
            case["specific"].as_str(),
            case["shared"].as_str(),
            Command::new(vec!["installed-adapter".into()]).unwrap(),
        );
        if case["error"].as_bool() == Some(true) {
            assert!(result.is_err());
        } else {
            let expected: Vec<_> = case["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            assert_eq!(result.unwrap().args(), expected);
        }
    }
    let harnesses = fixture["harness_cases"].as_array().unwrap();
    assert_eq!(harnesses.len(), 20);
    for case in harnesses {
        let result = resolve_harness(case["override"].as_str(), Ok(case["default"].as_str()));
        if case["error"].as_bool() == Some(true) {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap().name(), case["harness"].as_str().unwrap());
        }
    }
}
#[test]
fn tool_status_shares_ordering_without_recording_thought_payload() {
    let mut s = ready(false);
    s.submit(1, id("t"), text("human"), 3).unwrap();
    s.thinking_update(1, &id("t"), 1, 4).unwrap();
    assert!(s.thinking());
    s.tool_update(
        1,
        &id("t"),
        2,
        Tool {
            id: id("tool-7"),
            title: text("Read file"),
            status: ToolStatus::InProgress,
        },
        5,
    )
    .unwrap();
    s.chunk(1, &id("t"), 3, Some(id("message")), text("result"), 6)
        .unwrap();
    assert!(!s.thinking());
    s.tool_update(
        1,
        &id("t"),
        4,
        Tool {
            id: id("tool-7"),
            title: text("Read file"),
            status: ToolStatus::Completed,
        },
        7,
    )
    .unwrap();
    assert_eq!(s.tools().count(), 1);
    assert_eq!(s.tools().next().unwrap().status, ToolStatus::Completed);
    s.finished_reason(1, &id("t"), id("max_tokens"), 8).unwrap();
    assert_eq!(s.last_stop_reason(), Some(&id("max_tokens")));
}
#[test]
fn tool_cardinality_and_title_bounds_are_enforced() {
    let mut s = ready(false);
    s.submit(1, id("t"), text("human"), 3).unwrap();
    assert_eq!(
        s.tool_update(
            1,
            &id("t"),
            1,
            Tool {
                id: id("big"),
                title: Text::new("x".repeat(4097)).unwrap(),
                status: ToolStatus::Pending
            },
            4
        ),
        Err(Error::LimitExceeded)
    );
    for n in 1..=64 {
        s.tool_update(
            1,
            &id("t"),
            n,
            Tool {
                id: id(&format!("tool-{n}")),
                title: text("bounded"),
                status: ToolStatus::Pending,
            },
            4 + n,
        )
        .unwrap();
    }
    assert_eq!(
        s.tool_update(
            1,
            &id("t"),
            65,
            Tool {
                id: id("extra"),
                title: text("bounded"),
                status: ToolStatus::Pending
            },
            70
        ),
        Err(Error::LimitExceeded)
    );
}
#[test]
fn policy_interaction_advances_session_clock() {
    let mut s = ready(false);
    s.propose_policy(1, 0, Policy::Ask, 100).unwrap();
    assert_eq!(
        s.submit(1, id("t"), text("old"), 99),
        Err(Error::ClockWentBackwards)
    );
}

#[test]
fn delayed_approval_cannot_authorize_reused_wire_id_in_later_turn() {
    let mut b = Broker::new(1);
    b.request(1, req(0), options(), None, 0).unwrap();
    let displayed_epoch = b.permission_epoch();
    b.cancel_pending();
    b.next_turn().unwrap();
    b.request(1, req(0), options(), None, 1).unwrap();
    assert_eq!(
        b.answer_once(1, displayed_epoch, &req(0), true),
        Err(Error::StaleOperation)
    );
    assert_eq!(b.pending_count(), 1);
    assert_eq!(
        b.answer_once(1, b.permission_epoch(), &req(0), true)
            .unwrap()
            .request,
        req(0)
    );
}

#[test]
fn restart_rejects_changed_authority_scope_before_mutation() {
    let mut s = ready(false);
    s.lost(1, 3).unwrap();
    let mut other = launch();
    other.cwd = "/different-project".into();
    assert_eq!(s.restart(other, true, 4), Err(Error::InvalidConfig));
    assert_eq!(s.generation(), 1);
    assert!(s.restart(launch(), true, 4).is_ok());
}
