//! Disposable synthetic-session receiver, never a Host or compositor effect client.
use desktop_io::ProcessIdentity;
use modal_client::presenter::{Auth, Counter};
use modal_contract::{AuthorityEpoch, ClientId, Fence, Owner};
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    num::NonZeroU64,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    time::{Duration, Instant},
};
use vimarchy_core::{
    gestures::{Effect, Event},
    snapshot::{Client, Monitor, Workspace},
};
use vimarchy_runtime::{
    Context, Session,
    input_bridge::{Edge, Frame, Gate, Ingress, boottime_ms},
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("OMARCHY_INPUT_OBSERVER_ISOLATED").as_deref() != Ok("yes") {
        return Err("isolated fixture only".into());
    }
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: receiver PRIVATE_ROOT COMPOSITOR_PID START_TICKS".into());
    }
    let root = PathBuf::from(&args[1]);
    let peer = ProcessIdentity {
        pid: args[2].parse()?,
        start_ticks: args[3].parse()?,
    };
    let mut random = [0u8; 64];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let context = Context {
        presenter: random[0..16].try_into()?,
        source_revision: 1,
        fence: Fence {
            authority_epoch: AuthorityEpoch(random[16..32].try_into()?),
            owner: Owner {
                client: ClientId(random[32..48].try_into()?),
                client_epoch: NonZeroU64::new(1).unwrap(),
            },
            generation: NonZeroU64::new(1).unwrap(),
        },
    };
    let epoch = random[48..64].try_into()?;
    let namespace = format!(
        "omarchy-modal-{}",
        context
            .presenter
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let auth = Auth::new(context.fence, namespace)?;
    let gate = Gate::new(context, auth.clone(), epoch, 1, 1, "a".into())?;
    let mut ingress = Ingress::bind(&root, peer, gate)?;
    let monitors = [Monitor {
        name: "synthetic-fixture".into(),
        x: 0.,
        y: 0.,
        width: 1280.,
        height: 720.,
        scale: 1.,
        active_workspace: Workspace { id: 1 },
        special_workspace: Workspace { id: 0 },
    }];
    let clients = [Client {
        address: "0x1".into(),
        stable_id: "1".into(),
        mapped: true,
        hidden: false,
        pinned: false,
        workspace: Workspace { id: 1 },
        at: [10., 10.],
        size: [400., 300.],
        grouped: vec![],
        focus_history_id: 0,
    }];
    let (mut session, _) =
        Session::open(context, 4000, 0, &monitors, &clients, &BTreeMap::new(), 280)
            .map_err(|_| "session")?;
    session
        .host_ready(context, 0)
        .map_err(|_| "synthetic readiness")?;
    let mut template = serde_json::to_value(Frame {
        version: 1,
        auth,
        bridge_epoch: epoch,
        source_revision: Counter::new(1).unwrap(),
        sequence: Counter::new(1).unwrap(),
        sent_ms: Counter::new(1).unwrap(),
        device: Counter::new(1).unwrap(),
        code: 1,
        edge: Edge::Ready { held: false },
    })?;
    let obj = template.as_object_mut().ok_or("template")?;
    for k in ["edge", "sent_ms", "sequence"] {
        obj.remove(k);
    }
    let mut prefix = serde_json::to_string(&template)?;
    assert_eq!(prefix.pop(), Some('}'));
    let stat = std::fs::read_to_string("/proc/self/stat")?;
    let start = stat
        .rsplit_once(')')
        .ok_or("stat")?
        .1
        .split_whitespace()
        .nth(19)
        .ok_or("start")?;
    let mut admission = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join("admission"))?;
    writeln!(admission, "{}\n{start}\n{prefix}", std::process::id())?;
    admission.sync_all()?;
    drop(admission);
    let origin = Instant::now();
    let boot = boottime_ms()?;
    println!("{}", serde_json::json!({"receiver_ready":true}));
    io::stdout().flush()?;
    let mut focus = 0;
    let mut frames = 0;
    let mut last_tick = 0;
    loop {
        let monotonic = u64::try_from(origin.elapsed().as_millis())?;
        let now = boottime_ms()?.checked_sub(boot).ok_or("clock regression")?;
        if now.abs_diff(monotonic) > 50 {
            session.invalidate();
            println!(
                "{}",
                serde_json::json!({"revoked":true,"clock_discontinuity":true,"focus_intents":focus})
            );
            break;
        }
        match ingress.receive(
            &mut session,
            now,
            Instant::now() + Duration::from_millis(40),
        ) {
            Ok(Some(batch)) => {
                frames += 1;
                let consumed = session
                    .consume_batch(
                        batch,
                        boottime_ms()?.checked_sub(boot).ok_or("clock regression")?,
                    )
                    .map_err(|_| "consume")?;
                focus += consumed
                    .iter()
                    .filter(|e| matches!(e, Effect::Focus(_)))
                    .count();
                println!(
                    "{}",
                    serde_json::json!({"frames":frames,"focus_intents":focus,"effect_authority":false,"core_ms":now,"boot_ms":boottime_ms()?})
                );
                io::stdout().flush()?;
            }
            Ok(None) => {
                if now.saturating_sub(last_tick) >= 50 {
                    last_tick = now;
                    if session.input(context, Event::Tick, now).is_err() {
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => {
                println!(
                    "{}",
                    serde_json::json!({"revoked":true,"focus_intents":focus,"frames":frames,"valid":session.is_valid()})
                );
                break;
            }
        }
    }
    Ok(())
}
