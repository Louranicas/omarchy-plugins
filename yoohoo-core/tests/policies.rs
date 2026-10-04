use yoohoo_core::{
    Error, config::*, history::*, identity::*, reducer::*, selection::*, transport::*,
};
fn key(n: u64) -> WindowKey {
    WindowKey::new(
        "epoch",
        Address::parse(&format!("0x{n:x}")).unwrap(),
        &format!("stable-{n}"),
    )
    .unwrap()
}
fn client(n: u64) -> Client {
    Client {
        key: key(n),
        process: Some(ProcessKey::new(n as u32, 42).unwrap()),
        title: "SECRET_TITLE".into(),
        class: "SECRET_CLASS".into(),
        workspace: "SECRET_WORKSPACE".into(),
        workspace_id: 2,
    }
}
fn setup(config: Config) -> Attention {
    let mut s = Attention::new(config).unwrap();
    s.resnapshot(
        "epoch",
        0,
        vec![client(1), client(2), client(3)],
        None,
        &[],
        0,
    )
    .unwrap();
    s.set_health(SourceHealth {
        native: Health::Ready,
        notifications: Health::Ready,
        audio: Health::Ready,
        storage: Health::Ready,
    })
    .unwrap();
    s
}
fn send(s: &mut Attention, seq: u64, time: u64, event: Event) -> Result<Vec<Effect>, Error> {
    s.apply(Envelope {
        instance: "epoch".into(),
        sequence: seq,
        event,
        monotonic_ms: time,
        wall_ms: time as i64,
    })
}
fn attend(s: &mut Attention, seq: u64, n: u64, time: u64) -> Vec<Effect> {
    send(
        s,
        seq,
        time,
        Event::Attention {
            key: key(n),
            source: Source::Native,
        },
    )
    .unwrap()
}
fn record(n: u64) -> Record {
    Record {
        schema_version: 1,
        opaque_id: n,
        at_ms: n as i64,
        event: HistoryEvent::Attention,
        count: 1,
        source_bits: 1,
    }
}

#[test]
fn address_rejects_dispatch_injection_and_canonicalizes() {
    for bad in [
        "",
        "0x",
        "0x0",
        "0",
        "1",
        "0x1;exec",
        "0x1\n",
        " 0x1",
        "0x12345678901234567",
        "0X1",
    ] {
        assert_eq!(Address::parse(bad), Err(Error::InvalidIdentity));
    }
    assert_eq!(Address::parse("0x00Ab").unwrap().as_str(), "0xab");
    assert_eq!(format!("{:?}", key(1)), "WindowKey(<redacted>)");
}
#[test]
fn invalid_reload_preserves_last_known_good() {
    let mut s = setup(Config::default());
    let mut bad = s.config().clone();
    bad.volume = f64::NAN;
    assert_eq!(s.reload(bad, 1), Err(Error::InvalidConfig));
    assert_eq!(s.config().volume, 0.35);
}
#[test]
fn settings_never_silently_clamp() {
    for value in [f64::INFINITY, -0.1, 1.1] {
        let c = Config {
            volume: value,
            ..Config::default()
        };
        assert_eq!(c.validate(), Err(Error::InvalidConfig));
    }
    assert!(
        Config {
            streams: 65,
            ..Config::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        Config {
            history_age_ms: HISTORY_DAYS_MS + 1,
            ..Config::default()
        }
        .validate()
        .is_err()
    );
}
#[test]
fn notification_attribution_requires_exact_unique_process() {
    let a = client(1);
    assert_eq!(
        unique_process_window(ProcessKey::new(1, 42).unwrap(), std::slice::from_ref(&a)),
        Some(key(1))
    );
    assert_eq!(
        unique_process_window(ProcessKey::new(1, 43).unwrap(), std::slice::from_ref(&a)),
        None
    );
    let mut b = client(2);
    b.process = a.process;
    assert_eq!(unique_process_window(a.process.unwrap(), &[a, b]), None);
}
#[test]
fn distinct_repeated_sources_count_but_sequence_replays_do_not() {
    let mut s = setup(Config::default());
    attend(&mut s, 1, 1, 1);
    send(
        &mut s,
        2,
        2,
        Event::Attention {
            key: key(1),
            source: Source::Notification,
        },
    )
    .unwrap();
    assert_eq!(
        send(
            &mut s,
            2,
            2,
            Event::Attention {
                key: key(1),
                source: Source::Native
            }
        ),
        Err(Error::Duplicate)
    );
    let snap = s.snapshot(false);
    assert_eq!(snap.windows[0].count, 2);
    assert_eq!(snap.windows[0].sources, 3);
}
#[test]
fn focus_clear_removes_both_owned_tags() {
    let mut s = setup(Config::default());
    attend(&mut s, 1, 1, 1);
    let effects = send(&mut s, 2, 2, Event::Focus(Some(key(1)))).unwrap();
    assert!(effects.contains(&Effect::ClearOwnedTags(key(1))));
    assert!(s.snapshot(false).windows.is_empty());
    let effects = attend(&mut s, 3, 1, 3);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::SetAttention(_) | Effect::PlaySound { .. }))
    );
}
#[test]
fn close_removes_without_dispatch_to_dead_window() {
    let mut s = setup(Config::default());
    attend(&mut s, 1, 1, 1);
    let effects = send(&mut s, 2, 2, Event::Close(key(1))).unwrap();
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::ClearOwnedTags(_)))
    );
    assert!(s.snapshot(false).windows.is_empty());
    assert_eq!(
        send(
            &mut s,
            3,
            3,
            Event::Attention {
                key: key(1),
                source: Source::Native
            }
        ),
        Err(Error::StaleIdentity)
    );
}
#[test]
fn refresh_preserves_attention_age_count_and_source() {
    let mut s = setup(Config::default());
    attend(&mut s, 1, 1, 1);
    let mut c = client(1);
    c.title = "changed".into();
    send(&mut s, 2, 9, Event::Refresh(c)).unwrap();
    let snap = s.snapshot(true);
    let p = &snap.windows[0];
    assert_eq!(
        (p.count, p.first_attention_at_ms, p.last_attention_at_ms),
        (1, 1, 1)
    );
    assert_eq!(p.title.as_deref(), Some("changed"));
}
#[test]
fn wall_clock_rewind_does_not_reorder_signal_priority() {
    let mut s = setup(Config::default());
    attend(&mut s, 1, 1, 100);
    s.apply(Envelope {
        instance: "epoch".into(),
        sequence: 2,
        event: Event::Attention {
            key: key(2),
            source: Source::Native,
        },
        monotonic_ms: 101,
        wall_ms: -1000,
    })
    .unwrap();
    let snap = s.snapshot(false);
    assert_eq!(snap.windows[0].key, key(2));
    assert_eq!(snap.windows[0].last_attention_at_ms, -1000);
}
#[test]
fn tie_order_is_deterministic_and_snapshot_has_no_default_metadata() {
    let mut s = setup(Config::default());
    attend(&mut s, 1, 2, 1);
    attend(&mut s, 2, 1, 1);
    assert_eq!(s.snapshot(false).windows[0].key, key(1));
    let text = serde_json::to_string(&s.snapshot(false)).unwrap();
    assert!(!text.contains("SECRET"));
    assert!(!text.contains("title"));
    assert!(
        serde_json::to_string(&s.snapshot(true))
            .unwrap()
            .contains("SECRET_TITLE")
    );
}
#[test]
fn sequence_gap_marks_stale_until_full_resnapshot() {
    let mut s = setup(Config::default());
    attend(&mut s, 1, 1, 1);
    assert_eq!(
        send(&mut s, 3, 3, Event::Close(key(1))),
        Err(Error::SequenceGap)
    );
    assert!(s.snapshot(false).stale);
    assert_eq!(
        s.activation_target(&key(1), s.revision()),
        Err(Error::SourceUnavailable)
    );
    s.resnapshot("epoch", 3, vec![client(1)], None, &[], 4)
        .unwrap();
    assert!(!s.snapshot(false).stale);
    assert_eq!(s.snapshot(false).windows.len(), 1);
}
#[test]
fn instance_restart_never_restores_old_address_identity() {
    let mut s = setup(Config::default());
    attend(&mut s, 1, 1, 1);
    let mut next = client(1);
    next.key = WindowKey::new("new", Address::parse("0x1").unwrap(), "stable-1").unwrap();
    s.resnapshot("new", 0, vec![next], None, &[key(1)], 2)
        .unwrap();
    assert!(s.snapshot(false).windows.is_empty());
}
#[test]
fn same_instance_address_reuse_prunes_pending_and_cleans_only_live_owned_tags() {
    let mut s = setup(Config::default());
    attend(&mut s, 1, 1, 1);
    let mut replaced = client(1);
    replaced.key = WindowKey::new("epoch", Address::parse("0x1").unwrap(), "replacement").unwrap();
    let k = replaced.key.clone();
    let effects = s
        .resnapshot("epoch", 1, vec![replaced], None, &[k.clone(), key(2)], 2)
        .unwrap();
    assert!(effects.contains(&Effect::ClearOwnedTags(k)));
    assert!(!effects.contains(&Effect::ClearOwnedTags(key(2))));
    assert!(s.snapshot(false).windows.is_empty());
}
#[test]
fn duplicate_address_snapshot_rejected_atomically() {
    let mut s = setup(Config::default());
    attend(&mut s, 1, 1, 1);
    let revision = s.revision();
    assert_eq!(
        s.resnapshot("epoch", 1, vec![client(1), client(1)], None, &[], 2),
        Err(Error::InvalidIdentity)
    );
    assert_eq!(s.revision(), revision);
    assert_eq!(s.snapshot(false).windows.len(), 1);
}
#[test]
fn oversized_metadata_does_not_commit_event_or_revision() {
    let mut s = setup(Config::default());
    let rev = s.revision();
    let mut c = client(1);
    c.title = "x".repeat(MAX_METADATA_BYTES + 1);
    assert_eq!(
        send(&mut s, 1, 1, Event::Refresh(c)),
        Err(Error::InvalidInput)
    );
    assert_eq!(s.revision(), rev);
    attend(&mut s, 1, 1, 1);
}
#[test]
fn source_failure_is_not_healthy_empty() {
    let mut s = setup(Config::default());
    s.set_health(SourceHealth {
        native: Health::Ready,
        notifications: Health::Unavailable,
        audio: Health::Disabled,
        storage: Health::Ready,
    })
    .unwrap();
    assert_eq!(
        send(
            &mut s,
            1,
            1,
            Event::Attention {
                key: key(1),
                source: Source::Notification
            }
        ),
        Err(Error::SourceUnavailable)
    );
    assert_eq!(s.snapshot(false).health.notifications, Health::Unavailable);
}
#[test]
fn activation_requires_matching_revision_identity_and_fresh_state() {
    let mut s = setup(Config::default());
    attend(&mut s, 1, 1, 1);
    let revision = s.revision();
    assert_eq!(s.activation_target(&key(1), revision), Ok(key(1)));
    attend(&mut s, 2, 2, 2);
    assert_eq!(
        s.activation_target(&key(1), revision),
        Err(Error::StaleRevision)
    );
    assert_eq!(
        s.activation_target(&key(3), s.revision()),
        Err(Error::StaleIdentity)
    );
}
#[test]
fn sound_first_entry_only_global_cooldown_and_one_child() {
    let mut s = setup(Config::default());
    let first = attend(&mut s, 1, 1, 100);
    let token = first
        .iter()
        .find_map(|e| {
            if let Effect::PlaySound { operation } = e {
                Some(*operation)
            } else {
                None
            }
        })
        .unwrap();
    let second = attend(&mut s, 2, 2, 2000);
    assert!(!second.iter().any(|e| matches!(e, Effect::PlaySound { .. })));
    s.audio_finished(token, true).unwrap();
    let repeated = attend(&mut s, 3, 1, 3000);
    assert!(
        !repeated
            .iter()
            .any(|e| matches!(e, Effect::PlaySound { .. }))
    );
    let third = attend(&mut s, 4, 3, 3001);
    assert!(third.iter().any(|e| matches!(e, Effect::PlaySound { .. })));
    assert_eq!(s.audio_finished(token, true), Err(Error::StaleIdentity));
}
#[test]
fn sound_token_rejects_late_completion() {
    let mut g = SoundGate::default();
    let c = Config::default();
    let one = g.request(0, &c).unwrap();
    g.finished(one).unwrap();
    assert!(g.request(1499, &c).is_none());
    let two = g.request(1500, &c).unwrap();
    assert_ne!(one, two);
    assert_eq!(g.finished(one), Err(Error::StaleIdentity));
    assert_eq!(g.playing(), Some(two));
}
#[test]
fn pulse_matches_legacy_1600_1600_400_cycle() {
    assert_eq!(pulse_at(0, false), Pulse::Inhale);
    assert_eq!(pulse_at(1599, false), Pulse::Inhale);
    assert_eq!(pulse_at(1600, false), Pulse::Exhale);
    assert_eq!(pulse_at(3199, false), Pulse::Exhale);
    assert_eq!(pulse_at(3200, false), Pulse::Rest);
    assert_eq!(pulse_at(3600, false), Pulse::Inhale);
    assert_eq!(pulse_at(u64::MAX, true), Pulse::Steady);
}
#[test]
fn shutdown_is_idempotent_and_never_dispatches_after_disconnect() {
    let mut s = setup(Config::default());
    attend(&mut s, 1, 1, 1);
    s.disconnected().unwrap();
    let effects = s.shutdown(2, 2).unwrap();
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::ClearOwnedTags(_)))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::StopSound { .. }))
    );
    assert!(s.shutdown(3, 3).unwrap().is_empty());
    assert_eq!(
        send(&mut s, 2, 3, Event::Close(key(1))),
        Err(Error::Stopped)
    );
}
#[test]
fn connected_shutdown_emits_owned_tag_cleanup() {
    let mut s = setup(Config::default());
    attend(&mut s, 1, 1, 1);
    assert!(
        s.shutdown(2, 2)
            .unwrap()
            .contains(&Effect::ClearOwnedTags(key(1)))
    );
    assert!(s.snapshot(false).windows.is_empty());
}
#[test]
fn history_off_by_default_and_redacted_when_enabled() {
    let mut s = setup(Config::default());
    attend(&mut s, 1, 1, 1);
    assert!(s.history().is_empty());
    let mut c = s.config().clone();
    c.history_enabled = true;
    s.reload(c, 2).unwrap();
    attend(&mut s, 2, 1, 3);
    let data = s.history().lines().flatten().copied().collect::<Vec<_>>();
    let text = String::from_utf8(data).unwrap();
    for secret in ["SECRET", "stable-", "0x", "epoch"] {
        assert!(!text.contains(secret));
    }
    assert!(text.contains("opaque_id"));
}
#[test]
fn disabling_history_does_not_silently_delete_existing_records() {
    let mut h = History::default();
    let mut c = Config {
        history_enabled: true,
        ..Config::default()
    };
    h.record(record(1), 1, &c).unwrap();
    c.history_enabled = false;
    h.record(record(2), 2, &c).unwrap();
    assert_eq!(h.len(), 1);
    h.purge();
    assert!(h.is_empty());
}
#[test]
fn history_evicts_at_count_bytes_and_age_boundaries() {
    let mut h = History::default();
    let c = Config {
        history_enabled: true,
        history_entries: 2,
        history_age_ms: 10,
        ..Config::default()
    };
    for i in 1..=3 {
        h.record(record(i), i, &c).unwrap();
    }
    assert_eq!(h.len(), 2);
    h.expire(12, &c).unwrap();
    assert_eq!(h.len(), 1);
    h.expire(13, &c).unwrap();
    assert!(h.is_empty());
    let mut c = c;
    c.history_bytes = 160;
    for i in 20..30 {
        h.record(record(i), i, &c).unwrap();
        assert!(h.bytes() <= 160);
        assert_eq!(h.len(), 1);
    }
}
#[test]
fn impossible_history_budget_is_visible_not_partial_append() {
    let mut h = History::default();
    let c = Config {
        history_enabled: true,
        history_bytes: 1,
        ..Config::default()
    };
    assert_eq!(h.record(record(1), 1, &c), Err(Error::LimitExceeded));
    assert!(h.is_empty());
    let mut s = setup(c);
    attend(&mut s, 1, 1, 1);
    assert_eq!(s.snapshot(false).health.storage, Health::Degraded);
    assert_eq!(s.snapshot(false).windows.len(), 1);
}
#[test]
fn monotonic_rewind_is_rejected() {
    let mut s = setup(Config::default());
    attend(&mut s, 1, 1, 10);
    assert_eq!(
        send(&mut s, 2, 9, Event::Close(key(1))),
        Err(Error::ClockWentBackwards)
    );
    assert_eq!(s.snapshot(false).windows.len(), 1);
}

#[test]
fn exact_legacy_selection_examples() {
    let w = vec![key(1), key(2), key(3)];
    assert_eq!(step(&[], None, Direction::Next), None);
    assert_eq!(step(&w, None, Direction::Next), Some(key(1)));
    assert_eq!(step(&w, None, Direction::Previous), Some(key(3)));
    assert_eq!(step(&w, Some(&key(1)), Direction::Next), Some(key(2)));
    assert_eq!(step(&w, Some(&key(3)), Direction::Next), Some(key(1)));
    assert_eq!(step(&w, Some(&key(1)), Direction::Previous), Some(key(3)));
    assert_eq!(
        reconcile(&w, &[key(3), key(2), key(1)], Some(&key(2))),
        Some(key(2))
    );
    assert_eq!(
        reconcile(&w, &[key(1), key(3)], Some(&key(2))),
        Some(key(3))
    );
    assert_eq!(reconcile(&w, &[key(1)], Some(&key(3))), Some(key(1)));
    assert_eq!(reconcile(&w, &[], Some(&key(3))), None);
}
#[test]
fn selection_property_cycle_is_bijective_and_reconcile_always_survives() {
    for n in 1..=40 {
        let w: Vec<_> = (1..=n).map(key).collect();
        for k in &w {
            let next = step(&w, Some(k), Direction::Next).unwrap();
            assert_eq!(step(&w, Some(&next), Direction::Previous).as_ref(), Some(k));
            let survivors: Vec<_> = w.iter().filter(|x| *x != k).cloned().collect();
            let chosen = reconcile(&w, &survivors, Some(k));
            assert_eq!(chosen.is_none(), survivors.is_empty());
            if let Some(chosen) = chosen {
                assert!(survivors.contains(&chosen));
            }
        }
    }
}
#[test]
fn pending_first_open_steps_start_without_selection_like_panel_qml() {
    let mut s = Selection::default();
    s.open().unwrap();
    s.step(s.generation(), Direction::Next).unwrap();
    s.step(s.generation(), Direction::Next).unwrap();
    s.update(s.generation(), vec![key(1), key(2), key(3)], 1)
        .unwrap();
    assert_eq!(s.selected(), Some(&key(2)));
    s.hover(s.generation(), &key(3)).unwrap();
    assert_eq!(s.selected(), Some(&key(2)));
    s.close(s.generation()).unwrap();
    assert_eq!(s.activation_target(s.generation()), Err(Error::NotOpen));
}
#[test]
fn backward_first_cycle_selects_last() {
    let mut s = Selection::default();
    s.open().unwrap();
    s.step(s.generation(), Direction::Previous).unwrap();
    s.update(s.generation(), vec![key(1), key(2)], 1).unwrap();
    assert_eq!(s.selected(), Some(&key(2)));
}
#[test]
fn ordinary_open_selects_first_and_reconcile_preserves_next_survivor() {
    let mut s = Selection::default();
    s.open().unwrap();
    s.update(s.generation(), vec![key(1), key(2), key(3)], 1)
        .unwrap();
    s.hover(s.generation(), &key(2)).unwrap();
    s.update(s.generation(), vec![key(1), key(3)], 2).unwrap();
    assert_eq!(s.activation_target(s.generation()), Ok((key(3), 2)));
    assert_eq!(
        s.update(s.generation(), vec![], 1),
        Err(Error::StaleRevision)
    );
}
#[test]
fn ordered_stream_exact_legacy_trace() {
    let mut s = OrderedStreams::new(64, 60_000).unwrap();
    s.open("session", 1, 0).unwrap();
    assert!(
        s.enqueue("session", 2, Action::Accept, 1)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        s.enqueue("session", 1, Action::Step(Direction::Next), 2)
            .unwrap(),
        vec![Action::Step(Direction::Next), Action::Accept]
    );
    assert_eq!(
        s.enqueue("session", 2, Action::Accept, 3),
        Err(Error::Duplicate)
    );
    assert!(
        s.enqueue("session", 4, Action::Step(Direction::Previous), 4)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        s.enqueue("session", 3, Action::Step(Direction::Next), 5)
            .unwrap(),
        vec![
            Action::Step(Direction::Next),
            Action::Step(Direction::Previous)
        ]
    );
    assert_eq!(
        s.enqueue("session", 9999, Action::Accept, 6),
        Err(Error::SequenceGap)
    );
}
#[test]
fn duplicate_buffered_sequence_cannot_replace_queued_action() {
    let mut s = OrderedStreams::new(1, 100).unwrap();
    s.open("s", 1, 0).unwrap();
    s.enqueue("s", 2, Action::Cancel, 1).unwrap();
    assert_eq!(s.enqueue("s", 2, Action::Accept, 2), Err(Error::Duplicate));
    assert_eq!(
        s.enqueue("s", 1, Action::Open, 3).unwrap(),
        vec![Action::Open, Action::Cancel]
    );
}
#[test]
fn ordered_stream_budget_ttl_and_counter_exhaustion() {
    let mut s = OrderedStreams::new(1, 10).unwrap();
    s.open("s", 1, 0).unwrap();
    assert_eq!(s.open("b", 1, 1), Err(Error::LimitExceeded));
    for i in 2..=257 {
        assert!(s.enqueue("s", i, Action::Cancel, 1).unwrap().is_empty());
    }
    assert_eq!(s.pending(), 256);
    assert_eq!(
        s.enqueue("s", 258, Action::Cancel, 1),
        Err(Error::SequenceGap)
    );
    assert_eq!(
        s.enqueue("s", 1, Action::Cancel, 11),
        Err(Error::StreamExpired)
    );
    s.open("b", u64::MAX - 1, 11).unwrap();
    assert_eq!(
        s.enqueue("b", u64::MAX - 1, Action::Cancel, 11).unwrap(),
        vec![Action::Cancel]
    );
    assert_eq!(
        s.enqueue("b", u64::MAX, Action::Cancel, 11),
        Err(Error::Exhausted)
    );
}
#[test]
fn ordering_property_all_three_element_permutations_deliver_once() {
    for perm in [
        [1, 2, 3],
        [1, 3, 2],
        [2, 1, 3],
        [2, 3, 1],
        [3, 1, 2],
        [3, 2, 1],
    ] {
        let mut s = OrderedStreams::new(1, 100).unwrap();
        s.open("s", 1, 0).unwrap();
        let actions = [Action::Open, Action::Step(Direction::Next), Action::Accept];
        let mut observed = vec![];
        for seq in perm {
            observed.extend(s.enqueue("s", seq, actions[seq as usize - 1], 1).unwrap());
        }
        assert_eq!(observed, actions);
        assert_eq!(s.pending(), 0);
    }
}
#[test]
fn native_frames_preserve_only_identity_not_notification_or_title_payload() {
    assert_eq!(
        parse_native("urgent>>a\n").unwrap(),
        NativeEvent::Urgent(Address::parse("0xa").unwrap())
    );
    assert_eq!(
        parse_native("windowtitlev2>>a,SECRET_TITLE,more").unwrap(),
        NativeEvent::Refresh(Address::parse("0xa").unwrap())
    );
    assert_eq!(
        parse_native("activewindowv2>>").unwrap(),
        NativeEvent::Focused(None)
    );
    assert!(parse_native("urgent>>a;exec").is_err());
    assert!(parse_native(&"x".repeat(8193)).is_err());
    assert_eq!(
        parse_native("unknown>>SECRET").unwrap(),
        NativeEvent::Ignored
    );
}
#[test]
fn event_overflow_invalidates_stream_until_resync() {
    let mut q = EventQueue::new(2).unwrap();
    q.push(1).unwrap();
    q.push(2).unwrap();
    assert_eq!(q.push(3), Err(Error::QueueOverflow));
    assert!(q.needs_resync());
    assert_eq!(q.pop(), None);
    assert_eq!(q.push(4), Err(Error::SourceUnavailable));
    q.resynchronized();
    q.push(5).unwrap();
    assert_eq!(q.pop(), Some(5));
}
#[test]
fn snapshot_slot_coalesces_and_reconnect_is_bounded() {
    let mut slot = LatestSnapshot::default();
    for i in 0..10_000 {
        slot.offer(i);
    }
    assert_eq!(slot.take(), Some(9999));
    assert_eq!(slot.take(), None);
    let mut r = Reconnect::default();
    assert_eq!(r.delay_ms(0), 250);
    for _ in 0..1000 {
        assert!(r.delay_ms(u16::MAX) <= 10_000);
    }
    r.connected();
    assert_eq!(r.delay_ms(0), 250);
}

#[test]
fn selection_matches_96_fixtures_executed_by_pinned_legacy_javascript() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/legacy-selection.json")).unwrap();
    let parse = |value: &serde_json::Value| -> Option<WindowKey> {
        let value = value.as_str().unwrap();
        if value.is_empty() {
            None
        } else {
            let n = u64::from_str_radix(value.strip_prefix("0x").unwrap(), 16).unwrap();
            Some(key(n))
        }
    };
    let cases = fixtures["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 96);
    for case in cases {
        let previous: Vec<_> = case["previous"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| parse(v).unwrap())
            .collect();
        let windows: Vec<_> = case["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| parse(v).unwrap())
            .collect();
        let selected = parse(&case["selected"]);
        assert_eq!(
            step(&windows, selected.as_ref(), Direction::Next),
            parse(&case["next"])
        );
        assert_eq!(
            step(&windows, selected.as_ref(), Direction::Previous),
            parse(&case["previous_step"])
        );
        assert_eq!(
            reconcile(&previous, &windows, selected.as_ref()),
            parse(&case["reconciled"])
        );
    }
}

#[test]
fn window_limit_applies_to_full_snapshot_before_state_change() {
    let mut s = Attention::new(Config {
        max_windows: 2,
        ..Config::default()
    })
    .unwrap();
    assert_eq!(
        s.resnapshot(
            "epoch",
            0,
            vec![client(1), client(2), client(3)],
            None,
            &[],
            0
        ),
        Err(Error::LimitExceeded)
    );
    assert_eq!(s.revision(), 0);
    s.resnapshot("epoch", 0, vec![client(1), client(2)], None, &[], 0)
        .unwrap();
    assert_eq!(
        s.reload(
            Config {
                max_windows: 1,
                ..Config::default()
            },
            1
        ),
        Err(Error::LimitExceeded)
    );
    assert_eq!(s.config().max_windows, 2);
}
#[test]
fn model_trace_preserves_raw_counts_and_focused_exclusion() {
    let mut s = setup(Config {
        sound_enabled: false,
        ..Config::default()
    });
    let mut expected = std::collections::BTreeMap::<u64, u64>::new();
    let mut focused = None;
    let mut random = 17u64;
    for sequence in 1..=2000 {
        random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
        let n = 1 + (random >> 32) % 3;
        if random.is_multiple_of(5) {
            focused = Some(n);
            expected.remove(&n);
            send(&mut s, sequence, sequence, Event::Focus(Some(key(n)))).unwrap();
        } else {
            if focused != Some(n) {
                *expected.entry(n).or_default() += 1;
            }
            attend(&mut s, sequence, n, sequence);
        }
        let snapshot = s.snapshot(false);
        assert_eq!(snapshot.windows.len(), expected.len());
        for (n, count) in &expected {
            assert_eq!(
                snapshot
                    .windows
                    .iter()
                    .find(|w| w.key == key(*n))
                    .unwrap()
                    .count,
                *count
            );
        }
        assert!(snapshot.windows.len() <= 3);
    }
}

#[test]
fn legacy_accept_requires_cycling_but_explicit_mouse_activation_does_not() {
    let mut s = Selection::default();
    let generation = s.open().unwrap();
    s.update(generation, vec![key(1)], 1).unwrap();
    assert_eq!(s.accept_cycle(generation), Err(Error::NotOpen));
    assert_eq!(s.activation_target(generation), Ok((key(1), 1)));
    s.step(generation, Direction::Next).unwrap();
    assert_eq!(s.accept_cycle(generation), Ok((key(1), 1)));
}
#[test]
fn previous_open_responses_and_commands_cannot_mutate_new_selection() {
    let mut s = Selection::default();
    let old = s.open().unwrap();
    s.update(old, vec![key(1)], 100).unwrap();
    s.close(old).unwrap();
    let current = s.open().unwrap();
    s.update(current, vec![key(2)], 1).unwrap();
    assert_eq!(s.update(old, vec![key(1)], 101), Err(Error::StaleRevision));
    assert_eq!(s.step(old, Direction::Next), Err(Error::StaleRevision));
    assert_eq!(s.hover(old, &key(2)), Err(Error::StaleRevision));
    assert_eq!(s.close(old), Err(Error::StaleRevision));
    assert_eq!(s.activation_target(old), Err(Error::StaleRevision));
    assert_eq!(s.activation_target(current), Ok((key(2), 1)));
}
#[test]
fn audio_failure_changes_revision_and_publishes_new_health() {
    let mut s = setup(Config::default());
    let effects = attend(&mut s, 1, 1, 1);
    let operation = effects
        .iter()
        .find_map(|e| {
            if let Effect::PlaySound { operation } = e {
                Some(*operation)
            } else {
                None
            }
        })
        .unwrap();
    let revision = s.revision();
    assert_eq!(
        s.audio_finished(operation, false).unwrap(),
        vec![Effect::Publish {
            revision: revision + 1
        }]
    );
    assert_eq!(s.snapshot(false).health.audio, Health::Degraded);
    assert_eq!(s.revision(), revision + 1);
    assert_eq!(s.audio_finished(operation, true), Err(Error::StaleIdentity));
    assert_eq!(s.revision(), revision + 1);
}
