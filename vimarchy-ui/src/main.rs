//! Isolated fixture presenter. Mapping is logged, never certified as G4 readiness.
mod managed;
mod radial;
use gtk::{glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use modal_contract::{AuthorityEpoch, ClientId, Fence, Owner};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::Read,
    num::NonZeroU64,
    rc::Rc,
    time::{Duration, Instant},
};
use vimarchy_core::{
    gestures::{Effect, Event},
    snapshot::{Client, Monitor, Workspace},
};
use vimarchy_runtime::{Context, Session};

fn main() -> glib::ExitCode {
    if std::env::var("OMARCHY_TEST_ISOLATED_DISPLAY").as_deref() != Ok("yes") {
        eprintln!("refused: isolated fixture display required");
        return glib::ExitCode::FAILURE;
    }
    if std::env::args().nth(1).as_deref() == Some("--radial-check") {
        return radial::verify();
    }
    if std::env::args().nth(1).as_deref() == Some("--focus-loss-check") {
        return managed::verify_focus_loss();
    }
    if std::env::var_os("OMARCHY_MODAL_CONTROLLER_SOCKET").is_some() {
        return managed::run();
    }
    let mut nonce = [0; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut nonce))
        .is_err()
        || nonce == [0; 16]
    {
        return glib::ExitCode::FAILURE;
    }
    let fixture_namespace = format!(
        "vimarchy-fixture-{}",
        nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    let parent = match (
        std::env::var("OMARCHY_MODAL_SOCKET"),
        std::env::var("OMARCHY_MODAL_PARENT_PID"),
        std::env::var("OMARCHY_MODAL_READY"),
        std::env::var("OMARCHY_MODAL_NAMESPACE"),
    ) {
        (Err(_), Err(_), Err(_), Err(_)) => None,
        (Ok(socket), Ok(pid), Ok(hello), Ok(namespace))
            if namespace.starts_with("omarchy-modal-")
                && namespace.len() == 46
                && namespace[14..]
                    .bytes()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
                && hello.len() <= 4096 =>
        {
            let Ok(pid) = pid.parse::<u32>() else {
                return glib::ExitCode::FAILURE;
            };
            if pid == 0 {
                return glib::ExitCode::FAILURE;
            }
            Some((std::path::PathBuf::from(socket), pid, hello, namespace))
        }
        _ => {
            eprintln!("refused: incomplete parent readiness environment");
            return glib::ExitCode::FAILURE;
        }
    };
    let namespace = parent
        .as_ref()
        .map(|p| p.3.clone())
        .unwrap_or(fixture_namespace);
    let app = gtk::Application::builder()
        .application_id("org.omarchy.VimarchyFixture")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        let Some(display) = gtk::gdk::Display::default() else { app.quit(); return; };
        if !gtk4_layer_shell::is_supported() { eprintln!("layer_shell=unsupported"); app.quit(); return; }
        let Some(output) = display.monitors().item(0).and_downcast::<gtk::gdk::Monitor>() else { app.quit(); return; };
        let g = output.geometry();
        let monitors = [Monitor { name: "fixture-output".into(), x: 0., y: 0., width: g.width() as f64, height: g.height() as f64, scale: 1., active_workspace: Workspace { id: 1 }, special_workspace: Workspace { id: 0 } }];
        let clients = (0..3).map(|i| Client { address: format!("0x{}",i+1), stable_id: format!("{}",i+1), mapped: true, hidden: false, pinned: false, workspace: Workspace { id: 1 }, at: [80.+i as f64*300., 100.], size: [240.,180.], grouped: vec![], focus_history_id: i }).collect::<Vec<_>>();
        let context = Context { presenter: nonce, source_revision: 1, fence: Fence { authority_epoch: AuthorityEpoch(nonce), owner: Owner { client: ClientId(nonce), client_epoch: NonZeroU64::new(1).unwrap() }, generation: NonZeroU64::new(1).unwrap() } };
        let origin = Instant::now();
        let Ok((session, _)) = Session::open(context, 4500, 0, &monitors, &clients, &BTreeMap::new(), 250) else { app.quit(); return; };
        let session = Rc::new(RefCell::new(session));
        let overlay = gtk::ApplicationWindow::builder().application(app).title("Vimarchy fixture passive hints").build();
        overlay.init_layer_shell(); overlay.set_namespace(Some(&namespace)); overlay.set_monitor(Some(&output)); overlay.set_layer(Layer::Overlay); overlay.set_keyboard_mode(KeyboardMode::None); overlay.set_exclusive_zone(0);
        for edge in [Edge::Top,Edge::Bottom,Edge::Left,Edge::Right] { overlay.set_anchor(edge,true); }
        let fixed = gtk::Fixed::new();
        for window in &session.borrow().snapshot().windows {
            let hint = session.borrow().allocation().visible[&window.hint_id].clone();
            let label = gtk::Label::new(Some(&hint)); label.add_css_class("hint");
            label.update_property(&[gtk::accessible::Property::Label(&format!("Window hint {hint}"))]);
            fixed.put(&label, window.at[0], window.at[1]);
        }
        overlay.set_child(Some(&fixed));
        overlay.connect_realize(|w| { if let Some(surface) = w.surface() { surface.set_input_region(Some(&gtk::cairo::Region::create())); } });
        let parent_map = parent.clone(); let reported = std::cell::Cell::new(false);
        overlay.connect_map(move |w| {
            println!("presenter=gtk_mapped readiness=unverified fixture_only=true");
            if reported.replace(true) { return; }
            if let Some((socket,pid,hello,_)) = parent_map.clone() {
                gtk::prelude::WidgetExt::display(w).flush();
                let _ = std::thread::Builder::new().name("vimarchy-ready-report".into()).spawn(move || {
                    let result = modal_runtime::readiness::report_mapped(&socket,pid,&hello,Instant::now()+Duration::from_millis(300));
                    println!("parent_activation_received={} compositor_proof=external",result.is_ok());
                });
            }
        });
        let css = gtk::CssProvider::new(); css.load_from_string("window { background: transparent; } .hint { background: #24283b; color: #7aa2f7; font-size: 32px; padding: 12px; border: 2px solid #7aa2f7; } .controller { background: #24283b; color: #c0caf5; padding: 16px; }");
        gtk::style_context_add_provider_for_display(&display,&css,gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        overlay.present();
        let controller = gtk::ApplicationWindow::builder().application(app).title("Vimarchy isolated fixture controller").default_width(600).default_height(180).build();
        let column = gtk::Box::new(gtk::Orientation::Vertical,10); column.add_css_class("controller");
        column.append(&gtk::Label::new(Some("Fixture only · no compositor authority · passive hint layer")));
        let entry = gtk::Entry::builder().placeholder_text("Type a, s or d and press Enter").max_length(2).build();
        entry.update_property(&[gtk::accessible::Property::Label("Fixture window hint")]);
        entry.set_text("a"); column.append(&entry);
        let status = gtk::Label::new(Some("Waiting for fixture-only ready simulation")); column.append(&status);
        // Explicit fixture simulation; this cannot be used as production readiness.
        if parent.is_none() { session.borrow_mut().host_ready(context, 1).unwrap(); }
        println!("fixture_ready_simulation={} pid={} namespace={namespace} hints=3",parent.is_none(),std::process::id());
        let sequence = Rc::new(RefCell::new(0u64));
        let s = session.clone(); let label = status.clone();
        entry.connect_activate(move |entry| {
            let now = origin.elapsed().as_millis() as u64;
            let Some(next) = sequence.borrow().checked_add(1) else { return; }; *sequence.borrow_mut() = next;
            let hint = entry.text().to_string(); println!("fixture_hint={hint:?}");
            let result = s.borrow_mut().input(context, Event::Down { hint, press_id: next, alt: false, repeat: false }, now);
            if result.is_ok() {
                match s.borrow_mut().input(context, Event::Up { press_id: next }, now) {
                    Ok(batch) => { let focus = batch.effects().iter().any(|e| matches!(e,Effect::Focus(_))); label.set_text(if focus { "Focus intent recorded; effect backend unavailable" } else { "Gesture recorded; effect backend unavailable" }); println!("fixture_input=true focus_intent={focus} executed=false"); },
                    Err(error) => label.set_text(&format!("Rejected: {error:?}")),
                }
            } else { println!("fixture_input_rejected={result:?}"); label.set_text("Rejected stale or invalid hint"); }
            entry.set_text("");
        });
        let submit = gtk::Button::with_label("Record fixture gesture"); let entry_submit = entry.clone(); submit.connect_clicked(move |_| entry_submit.emit_activate()); column.append(&submit);
        let close = gtk::Button::with_label("Close fixture"); let app_close = app.clone(); close.connect_clicked(move |_| app_close.quit()); column.append(&close);
        controller.set_child(Some(&column)); controller.present(); entry.grab_focus();
        let app_end = app.clone(); glib::timeout_add_local_once(Duration::from_millis(4000),move || { session.borrow_mut().invalidate(); println!("lifecycle=fixture_deadline invalidated=true"); app_end.quit(); });
    });
    app.run_with_args::<&str>(&[])
}
