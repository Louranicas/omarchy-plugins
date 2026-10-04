use ask_core::launch::{Command, FrozenLaunch, Harness};
use ask_core::session::Phase;
use ask_core::{Id, RequestId, Text};
use ask_runtime::{Adapter, Error, Event, Worker};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
fn launch() -> FrozenLaunch {
    FrozenLaunch {
        harness: Harness::Codex,
        cwd: "/".into(),
        command: Command::new(vec!["/usr/bin/python3".into()]).unwrap(),
        model: None,
        reasoning: None,
    }
}
fn reply(a: &mut Adapter, result: Value, now: u64) {
    let calls = a.take_outbox();
    assert_eq!(calls.len(), 1);
    a.incoming(
        a.instance(),
        1,
        json!({"jsonrpc":"2.0","id":calls[0]["id"],"result":result}),
        now,
    )
    .unwrap();
}
fn ready(close: bool) -> Adapter {
    let mut a = Adapter::new(launch(), 0).unwrap();
    reply(
        &mut a,
        json!({"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":if close{json!({"close":{}})}else{json!({})}},"_meta":{"steering":{"supported":true}}}),
        1,
    );
    reply(&mut a, json!({"sessionId":"session-actual"}), 2);
    assert_eq!(a.session().phase(), Phase::Ready);
    a.take_events();
    a
}
fn permission(id: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission","params":{"sessionId":"session-actual","toolCall":{"title":"Read data"},"options":[{"optionId":"actual-option","kind":"allow_once"},{"optionId":"always","kind":"allow_always"}]}})
}
#[test]
fn negotiation_config_groups_and_ordered_ack() {
    let mut l = launch();
    l.model = Some(Id::new("chosen").unwrap());
    let mut a = Adapter::new(l, 0).unwrap();
    reply(&mut a, json!({"protocolVersion":1}), 1);
    reply(
        &mut a,
        json!({"sessionId":"session-actual","configOptions":[{"id":"model-id","category":"model","options":[{"group":"Models","options":[{"value":"chosen"}]}]}]}),
        2,
    );
    let calls = a.take_outbox();
    assert_eq!(calls[0]["method"], "session/set_config_option");
    assert_eq!(calls[0]["params"]["value"], "chosen");
    assert_eq!(a.session().phase(), Phase::Configuring);
    a.incoming(
        a.instance(),
        1,
        json!({"jsonrpc":"2.0","id":calls[0]["id"],"result":{}}),
        3,
    )
    .unwrap();
    assert_eq!(a.session().phase(), Phase::Ready);
}
#[test]
fn permissions_keep_exact_wire_ids_and_epoch() {
    let mut a = ready(false);
    a.submit(Id::new("turn").unwrap(), Text::new("hello").unwrap(), 3)
        .unwrap();
    a.take_outbox();
    a.incoming(a.instance(), 1, permission(json!(42)), 4)
        .unwrap();
    let events = a.take_events();
    let Event::Permission {
        generation,
        epoch,
        request,
        ..
    } = &events[0]
    else {
        panic!()
    };
    assert_eq!(*request, RequestId::Number(42));
    a.answer(
        a.instance(),
        *generation,
        *epoch,
        request,
        Some(&Id::new("actual-option").unwrap()),
        5,
    )
    .unwrap();
    let out = a.take_outbox();
    assert_eq!(out[0]["id"], 42);
    assert_eq!(out[0]["result"]["outcome"]["optionId"], "actual-option");
}
#[test]
fn wrong_session_update_poisoned_without_text_cross_contamination() {
    let mut a = ready(false);
    a.submit(Id::new("turn").unwrap(), Text::new("hello").unwrap(), 3)
        .unwrap();
    let prior = a.session().messages().len();
    assert_eq!(a.incoming(a.instance(), 1,json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"other","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"wrong"}}}}),4),Err(Error::Stale));
    assert_eq!(a.session().messages().len(), prior);
    assert_eq!(a.session().phase(), Phase::Lost);
}
#[test]
fn wrong_generation_is_ignored_without_poisoning_current_session() {
    let mut a = ready(false);
    assert_eq!(a.incoming(a.instance(), 0, json!({}), 3), Err(Error::Stale));
    assert_eq!(a.session().phase(), Phase::Ready);
}
#[test]
fn advertised_close_only() {
    for support in [false, true] {
        let mut a = ready(support);
        a.close(3).unwrap();
        let out = a.take_outbox();
        assert_eq!(out.len(), usize::from(support));
        if support {
            assert_eq!(out[0]["method"], "session/close")
        }
    }
}
#[test]
fn cancellation_uses_notification_and_cancels_pending_before_wire_cancel() {
    let mut a = ready(false);
    a.submit(Id::new("turn").unwrap(), Text::new("hello").unwrap(), 3)
        .unwrap();
    a.take_outbox();
    a.incoming(a.instance(), 1, permission(json!("42")), 4)
        .unwrap();
    a.cancel(5).unwrap();
    let out = a.take_outbox();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0]["id"], "42");
    assert_eq!(out[0]["result"]["outcome"]["outcome"], "cancelled");
    assert_eq!(out[1]["method"], "session/cancel");
    assert!(out[1].get("id").is_none());
}
#[test]
fn unknown_client_method_gets_error_and_no_execution() {
    let mut a = ready(false);
    a.incoming(a.instance(), 1,json!({"jsonrpc":"2.0","id":4,"method":"terminal/create","params":{"command":"do-not-execute"}}),3).unwrap();
    assert_eq!(a.take_outbox()[0]["error"]["code"], -32601);
}
#[test]
fn real_python_provider_handshake_permission_chunk_completion() {
    let script = r#"import sys,json
for line in sys.stdin:
 m=json.loads(line)
 method=m.get('method')
 if method=='initialize': r={'protocolVersion':1}
 elif method=='session/new': r={'sessionId':'session-actual'}
 elif method=='session/prompt':
  print(json.dumps({'jsonrpc':'2.0','id':17,'method':'session/request_permission','params':{'sessionId':'session-actual','options':[{'optionId':'yes-real','kind':'allow_once'}]}}),flush=True)
  answer=json.loads(sys.stdin.readline())
  assert answer['id']==17 and answer['result']['outcome']['optionId']=='yes-real'
  print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'session-actual','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'verified'}}}}),flush=True)
  r={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':r}),flush=True)
"#;
    let mut l = launch();
    l.command = Command::new(vec![
        "/usr/bin/python3".into(),
        "-u".into(),
        "-c".into(),
        script.into(),
    ])
    .unwrap();
    let mut w = Worker::spawn(l, vec![], 0).unwrap();
    let start = Instant::now();
    let cancel = AtomicBool::new(false);
    let mut submitted = false;
    let mut approved = false;
    while start.elapsed() < Duration::from_secs(2) {
        let now = start.elapsed().as_millis() as u64;
        for e in w.pump(now, &cancel).unwrap() {
            match e {
                Event::Ready => {
                    w.adapter_mut()
                        .submit(Id::new("t").unwrap(), Text::new("test").unwrap(), now)
                        .unwrap();
                    submitted = true
                }
                Event::Permission {
                    instance,
                    generation,
                    epoch,
                    request,
                    options,
                    ..
                } => {
                    w.adapter_mut()
                        .answer(
                            instance,
                            generation,
                            epoch,
                            &request,
                            Some(&options[0].id),
                            now,
                        )
                        .unwrap();
                    approved = true
                }
                _ => {}
            }
        }
        if submitted && approved && w.adapter().session().active_turn().is_none() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(submitted && approved);
    assert!(w.adapter().session().active_turn().is_none());
    assert_eq!(
        w.adapter().session().messages()[1].body.as_str(),
        "verified"
    );
    let receipt = w.reap_receipt();
    drop(w);
    let end = Instant::now() + Duration::from_secs(1);
    while receipt.state() == acp_transport::ReapState::Outstanding && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(receipt.state(), acp_transport::ReapState::Reaped);
}

#[test]
fn final_valid_reply_is_delivered_before_immediate_provider_exit() {
    let script = r#"import sys,json
for line in sys.stdin:
 m=json.loads(line)
 if m['method']=='initialize':r={'protocolVersion':1}
 elif m['method']=='session/new':r={'sessionId':'session-actual'}
 elif m['method']=='session/prompt':
  print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'session-actual','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'last reply'}}}}),flush=True)
  print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':{'stopReason':'end_turn'}}),flush=True)
  sys.exit(0)
 else:continue
 print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':r}),flush=True)
"#;
    let mut l = launch();
    l.command = Command::new(vec![
        "/usr/bin/python3".into(),
        "-u".into(),
        "-c".into(),
        script.into(),
    ])
    .unwrap();
    let mut w = Worker::spawn(l, vec![], 0).unwrap();
    let start = Instant::now();
    let mut fault = false;
    let mut ready_seen = false;
    while start.elapsed() < Duration::from_secs(2) {
        let now = start.elapsed().as_millis() as u64;
        for e in w.pump(now, &AtomicBool::new(false)).unwrap() {
            match e {
                Event::Ready => {
                    ready_seen = true;
                    w.adapter_mut()
                        .submit(Id::new("turn").unwrap(), Text::new("go").unwrap(), now)
                        .unwrap()
                }
                Event::Fault(_) => fault = true,
                _ => {}
            }
        }
        if fault {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(ready_seen && fault);
    assert_eq!(
        w.adapter().session().messages()[1].body.as_str(),
        "last reply"
    );
}
#[test]
fn close_stage_distinguishes_term_from_kill() {
    let mut a = ready(false);
    a.close(10).unwrap();
    a.take_events();
    a.tick(360).unwrap();
    assert_eq!(a.take_events(), vec![Event::Terminate]);
    a.tick(860).unwrap();
    assert_eq!(a.take_events(), vec![Event::ReapOutstanding]);
}
#[test]
fn stream_preserves_tool_title_on_partial_status_update() {
    let mut a = ready(false);
    a.submit(Id::new("turn").unwrap(), Text::new("go").unwrap(), 3)
        .unwrap();
    a.take_outbox();
    for (time, update) in [
        (
            4,
            json!({"sessionUpdate":"tool_call","toolCallId":"t","title":"Actual title","status":"in_progress"}),
        ),
        (
            5,
            json!({"sessionUpdate":"tool_call_update","toolCallId":"t","status":"completed"}),
        ),
    ] {
        a.incoming(a.instance(), 1,json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"session-actual","update":update}}),time).unwrap();
    }
    let tool = a.session().tools().next().unwrap();
    assert_eq!(tool.title.as_str(), "Actual title");
    assert_eq!(tool.status, ask_core::session::ToolStatus::Completed);
}

#[test]
fn cancellation_race_drains_late_updates_and_cancels_new_permission() {
    let mut a = ready(false);
    a.submit(Id::new("turn").unwrap(), Text::new("go").unwrap(), 3)
        .unwrap();
    let prompt = a.take_outbox().pop().unwrap();
    a.cancel(4).unwrap();
    a.take_outbox();
    a.incoming(a.instance(), 1, permission(json!(99)), 5)
        .unwrap();
    assert_eq!(
        a.take_outbox()[0]["result"]["outcome"]["outcome"],
        "cancelled"
    );
    a.incoming(a.instance(), 1,json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"session-actual","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"late"}}}}),6).unwrap();
    assert_eq!(a.session().messages()[1].body.as_str(), "");
    assert_eq!(a.session().phase(), Phase::Ready);
    a.incoming(
        a.instance(),
        1,
        json!({"jsonrpc":"2.0","id":prompt["id"],"result":{"stopReason":"cancelled"}}),
        7,
    )
    .unwrap();
    assert!(a.session().active_turn().is_none());
    assert!(
        a.submit(Id::new("next").unwrap(), Text::new("next").unwrap(), 8)
            .is_ok()
    );
}

#[test]
fn idle_mode_and_config_updates_preserve_ready_session() {
    let mut a = ready(false);
    for (now, update) in [
        (
            3,
            json!({"sessionUpdate":"current_mode_update","currentModeId":"plan"}),
        ),
        (
            4,
            json!({"sessionUpdate":"config_option_update","configOptions":[{"id":"model","type":"select","category":"model","currentValue":"m","options":[{"value":"m","name":"M"}]}]}),
        ),
    ] {
        a.incoming(a.instance(),1,json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"session-actual","update":update}}),now).unwrap();
    }
    assert_eq!(a.session().phase(), Phase::Ready);
    assert_eq!(a.current_mode().unwrap().as_str(), "plan");
    assert_eq!(a.config_options()[0].values[0].as_str(), "m");
}
#[test]
fn unsupported_boolean_and_future_controls_do_not_block_session_creation() {
    let mut a = Adapter::new(launch(), 0).unwrap();
    reply(&mut a, json!({"protocolVersion":1}), 1);
    reply(
        &mut a,
        json!({"sessionId":"session-actual","configOptions":[{"id":"notifications","type":"boolean","currentValue":false},{"id":"future","type":"new-control","currentValue":{"opaque":true}}]}),
        2,
    );
    assert_eq!(a.session().phase(), Phase::Ready);
    assert!(a.config_options().is_empty());
}
#[test]
fn permission_callback_from_old_runtime_cannot_approve_new_runtime() {
    let mut old = ready(false);
    old.submit(Id::new("t").unwrap(), Text::new("old").unwrap(), 3)
        .unwrap();
    old.take_outbox();
    old.incoming(old.instance(), 1, permission(json!(42)), 4)
        .unwrap();
    let event = old.take_events().pop().unwrap();
    let Event::Permission {
        instance,
        generation,
        epoch,
        request,
        options,
        ..
    } = event
    else {
        panic!()
    };
    let mut new = ready(false);
    new.submit(Id::new("t").unwrap(), Text::new("new").unwrap(), 3)
        .unwrap();
    new.take_outbox();
    new.incoming(new.instance(), 1, permission(json!(42)), 4)
        .unwrap();
    assert_ne!(old.instance(), new.instance());
    assert_eq!(
        new.answer(
            instance,
            generation,
            epoch,
            &request,
            Some(&options[0].id),
            5
        ),
        Err(Error::Stale)
    );
    assert!(new.take_outbox().is_empty());
    assert_eq!(new.session().permissions().pending_count(), 1);
    assert_eq!(new.incoming(instance, 1, json!({}), 5), Err(Error::Stale));
    assert_eq!(new.session().phase(), Phase::Ready);
}
