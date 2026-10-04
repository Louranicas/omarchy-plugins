//! Controlled fixture data-link presenter. No production display launch is allowed.
use gtk::{glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::{
    cell::Cell,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use vimarchy_runtime::presenter::{self, Counter, Input, Link, Phase, Row};
#[derive(Clone)]
struct Config {
    passive: bool,
    socket: PathBuf,
    parent: u32,
    ready: String,
    namespace: String,
    data: PathBuf,
    controller: desktop_io::ProcessIdentity,
}
// Default is retained solely for existing explicitly isolated interactive fixtures.
// This switch grants no input authority; passive mode has no GTK input adapter.
fn passive_mode(value: Option<&str>) -> Option<bool> {
    match value {
        Some("passive") => Some(true),
        None | Some("interactive-fixture") => Some(false),
        Some(_) => None,
    }
}
fn config() -> Option<Config> {
    let presentation = match std::env::var("OMARCHY_VIMARCHY_PRESENTATION") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => return None,
    };
    Some(Config {
        passive: passive_mode(presentation.as_deref())?,
        socket: std::env::var_os("OMARCHY_MODAL_SOCKET")?.into(),
        parent: std::env::var("OMARCHY_MODAL_PARENT_PID")
            .ok()?
            .parse()
            .ok()?,
        ready: std::env::var("OMARCHY_MODAL_READY").ok()?,
        namespace: std::env::var("OMARCHY_MODAL_NAMESPACE").ok()?,
        data: std::env::var_os("OMARCHY_MODAL_CONTROLLER_SOCKET")?.into(),
        controller: desktop_io::ProcessIdentity {
            pid: std::env::var("OMARCHY_MODAL_CONTROLLER_PID")
                .ok()?
                .parse()
                .ok()?,
            start_ticks: std::env::var("OMARCHY_MODAL_CONTROLLER_START_TICKS")
                .ok()?
                .parse()
                .ok()?,
        },
    })
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum HeldKey {
    Other,
    Enter,
    Workspace(char),
}
#[derive(Default)]
struct PhysicalInput {
    held: std::collections::BTreeMap<u32, HeldKey>,
    exhausted: bool,
    pointer: bool,
    active: u8,
}
impl PhysicalInput {
    // Every physical key is recorded before symbol, modifier or phase filtering.
    fn press(&mut self, code: u32) -> bool {
        if self.exhausted || self.held.contains_key(&code) {
            return false;
        }
        if self.held.len() == 256 {
            self.exhausted = true;
            return false;
        }
        self.held.insert(code, HeldKey::Other);
        true
    }
    fn key_down(&mut self, code: u32) -> bool {
        let first = self.active == 0 && !self.held.values().any(|k| *k == HeldKey::Enter);
        *self.held.get_mut(&code).expect("physical press recorded") = HeldKey::Enter;
        if first {
            self.active = 1;
        }
        first
    }
    fn release(&mut self, code: u32) -> bool {
        let removed = self.held.remove(&code);
        let release = removed == Some(HeldKey::Enter)
            && self.active == 1
            && !self.held.values().any(|k| *k == HeldKey::Enter);
        if release {
            self.active = 0;
        }
        release
    }
    fn pointer_down(&mut self) -> bool {
        let first = !self.exhausted
            && !self.pointer
            && self.active == 0
            && !self.held.values().any(|k| *k == HeldKey::Enter);
        self.pointer = true;
        if first {
            self.active = 2;
        }
        first
    }
    fn pointer_up(&mut self) -> bool {
        self.pointer = false;
        let release = self.active == 2;
        if release {
            self.active = 0;
        }
        release
    }
    fn workspace_down(&mut self, code: u32, key: char) -> bool {
        let first = !self.held.values().any(|k| *k == HeldKey::Workspace(key));
        *self.held.get_mut(&code).expect("physical press recorded") = HeldKey::Workspace(key);
        first
    }
}
enum Action {
    Down(String, bool),
    Up,
    Workspace(char, bool),
    Cancel,
    Close,
}
enum Update {
    Model(Vec<Row>),
    Applied(Phase, Option<(String, i64)>),
    Fault,
}
fn phase_update(phase: Phase, rows: &[Row]) -> std::io::Result<Update> {
    let radial = rows.iter().find_map(|r| {
        if let Row::Radial {
            source_hint,
            current_workspace,
        } = r
        {
            Some((source_hint.clone(), *current_workspace))
        } else {
            None
        }
    });
    if (phase == Phase::Radial) != radial.is_some() {
        return Err(std::io::Error::other("radial metadata unavailable"));
    }
    Ok(Update::Applied(phase, radial))
}
fn work(
    config: Config,
    commands: mpsc::Receiver<Action>,
    updates: mpsc::SyncSender<Update>,
    stop: Arc<AtomicBool>,
    parent_watch: Arc<modal_runtime::readiness::ParentWatch>,
) {
    let result = (|| -> std::io::Result<()> {
        parent_watch.check()?;
        let auth = presenter::auth_from_ready(&config.ready)
            .map_err(|_| std::io::Error::other("invalid authority"))?;
        if auth.namespace() != config.namespace {
            return Err(std::io::Error::other("namespace mismatch"));
        }
        modal_runtime::readiness::report_mapped(
            &config.socket,
            config.parent,
            &config.ready,
            Instant::now() + Duration::from_millis(500),
        )?;
        let mut link = Link::connect(&config.data, config.controller, auth)?;
        let (rows, phase) = link.model()?;
        parent_watch.check()?;
        if phase != Phase::Selecting {
            return Err(std::io::Error::other("selection is not active"));
        }
        updates
            .try_send(Update::Model(rows))
            .map_err(|_| std::io::Error::other("UI queue full"))?;
        println!("managed_data_authenticated=true activation_received=true");
        let mut press = 0u64;
        let mut held = None;
        let mut phase = phase;
        while !stop.load(Ordering::Acquire) {
            parent_watch.check()?;
            if held.is_some_and(|(_, since): (Counter, Instant)| {
                since.elapsed() >= Duration::from_secs(2)
            }) {
                let _ = link.input(Input::Cancel);
                return Err(std::io::Error::other("physical release deadline"));
            }
            match commands.recv_timeout(Duration::from_millis(40)) {
                Ok(
                    action @ (Action::Down(_, _)
                    | Action::Up
                    | Action::Workspace(_, _)
                    | Action::Cancel),
                ) => {
                    parent_watch.check()?;
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let input = match action {
                        Action::Down(hint, alt) => {
                            if held.is_some() {
                                return Err(std::io::Error::other("overlapping press"));
                            }
                            press = press
                                .checked_add(1)
                                .ok_or_else(|| std::io::Error::other("press exhausted"))?;
                            let p = Counter::new(press).unwrap();
                            held = Some((p, Instant::now()));
                            Input::Down {
                                hint,
                                press: p,
                                alt,
                                repeat: false,
                            }
                        }
                        Action::Up => {
                            let Some((p, _)) = held.take() else {
                                continue;
                            };
                            Input::Up { press: p }
                        }
                        Action::Workspace(key, alt) => Input::Workspace { key, alt },
                        Action::Cancel => Input::Cancel,
                        Action::Close => unreachable!(),
                    };
                    phase = link.input(input)?;
                    let rows = if phase == Phase::Radial {
                        let (rows, current) = link.model()?;
                        if current != phase {
                            return Err(std::io::Error::other("phase changed"));
                        }
                        rows
                    } else {
                        Vec::new()
                    };
                    updates
                        .try_send(phase_update(phase, &rows)?)
                        .map_err(|_| std::io::Error::other("UI queue full"))?;
                    println!("managed_input_acknowledged=true phase={phase:?}");
                    if phase == Phase::Closed {
                        break;
                    }
                }
                Ok(Action::Close) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let (rows, current) = link.model()?;
                    if current != phase {
                        phase = current;
                        updates
                            .try_send(phase_update(phase, &rows)?)
                            .map_err(|_| std::io::Error::other("UI queue full"))?;
                    }
                    if phase == Phase::Closed {
                        break;
                    }
                }
            }
        }
        let _ = link.close();
        Ok(())
    })();
    if result.is_err() {
        println!("managed_link_closed=true");
        let _ = updates.try_send(Update::Fault);
    }
}
fn focus_cancellation(
    tx: mpsc::SyncSender<Action>,
    phase: Rc<Cell<Phase>>,
    stop: Arc<AtomicBool>,
) -> gtk::EventControllerFocus {
    let focus = gtk::EventControllerFocus::new();
    focus.connect_leave(move |_| {
        if phase.get() != Phase::Mapping && tx.try_send(Action::Cancel).is_err() {
            stop.store(true, Ordering::Release);
        }
    });
    focus
}

/// Isolated real-GTK oracle for the exact cancellation controller installed below.
/// It grants no modal authority and performs no compositor dispatch.
pub fn verify_focus_loss() -> glib::ExitCode {
    let passed = Rc::new(Cell::new(false));
    let result = passed.clone();
    let app = gtk::Application::builder()
        .application_id("org.omarchy.VimarchyFocusOracle")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        let window = gtk::ApplicationWindow::builder().application(app).build();
        let entry = gtk::Entry::new();
        window.set_child(Some(&entry));
        let (tx, rx) = mpsc::sync_channel(4);
        let phase = Rc::new(Cell::new(Phase::Mapping));
        let stop = Arc::new(AtomicBool::new(false));
        window.add_controller(focus_cancellation(tx, phase.clone(), stop.clone()));
        window.present();
        let win = window.clone();
        glib::timeout_add_local_once(Duration::from_millis(100), move || {
            entry.grab_focus();
            phase.set(Phase::Radial);
            glib::timeout_add_local_once(Duration::from_millis(100), move || {
                gtk::prelude::GtkWindowExt::set_focus(&win, None::<&gtk::Widget>);
            });
        });
        let app = app.clone();
        let passed = result.clone();
        glib::timeout_add_local_once(Duration::from_millis(350), move || {
            let ok = matches!(rx.try_recv(), Ok(Action::Cancel))
                && rx.try_recv().is_err()
                && !stop.load(Ordering::Acquire);
            passed.set(ok);
            println!("gtk_focus_loss_cancel_observed={ok}");
            window.destroy();
            app.quit();
        });
    });
    app.run_with_args::<&str>(&[]);
    if passed.get() {
        glib::ExitCode::SUCCESS
    } else {
        glib::ExitCode::FAILURE
    }
}

pub fn run() -> glib::ExitCode {
    let Some(config) = config() else {
        return glib::ExitCode::FAILURE;
    };
    if presenter::auth_from_ready(&config.ready).is_err() {
        return glib::ExitCode::FAILURE;
    }
    let Ok(parent_watch) = modal_runtime::readiness::ParentWatch::from_environment() else {
        return glib::ExitCode::FAILURE;
    };
    let parent_watch = Arc::new(parent_watch);
    let stop = Arc::new(AtomicBool::new(false));
    let worker = Arc::new(Mutex::new(None::<std::thread::JoinHandle<()>>));
    let app = gtk::Application::builder()
        .application_id("org.omarchy.VimarchyManagedFixture")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let worker_app = worker.clone();
    let stopping = stop.clone();
    app.connect_activate(move|app|{
  let Some(display)=gtk::gdk::Display::default()else{app.quit();return;};if !gtk4_layer_shell::is_supported(){app.quit();return;}
  let Some(monitor)=display.monitors().item(0).and_downcast::<gtk::gdk::Monitor>()else{app.quit();return;};
  let passive=config.passive;
  let window=gtk::ApplicationWindow::builder().application(app).title("Vimarchy managed fixture").build();window.init_layer_shell();window.set_namespace(Some(&config.namespace));window.set_monitor(Some(&monitor));window.set_layer(Layer::Overlay);window.set_keyboard_mode(KeyboardMode::None);window.set_exclusive_zone(0);for edge in [Edge::Top,Edge::Bottom,Edge::Left,Edge::Right]{window.set_anchor(edge,true);}
  let fixed=gtk::Fixed::new();fixed.set_size_request(1,1);window.set_child(Some(&fixed));
  let loading=gtk::Label::new(Some("Loading hints…"));loading.set_can_target(false);loading.set_can_focus(false);loading.set_visible(passive);fixed.put(&loading,16.,16.);
  let controls=gtk::Box::new(gtk::Orientation::Vertical,8);controls.add_css_class("controller");controls.set_sensitive(false);controls.set_visible(!passive);controls.set_can_focus(false);
  controls.append(&gtk::Label::new(Some("Managed fixture · authenticated daemon-provided hints")));
  let entry=gtk::Entry::builder().placeholder_text("Window hint").max_length(2).build();entry.update_property(&[gtk::accessible::Property::Label("Managed fixture window hint")]);controls.append(&entry);
  let submit=gtk::Button::with_label("Select hint");controls.append(&submit);let status=gtk::Label::new(Some("Waiting for authenticated activation and data"));controls.append(&status);
  fixed.put(&controls,60.,350.);
  let css=gtk::CssProvider::new();css.load_from_string("window { background: transparent; } .workspace-badge { background: #24283b; color: #7aa2f7; font-size: 28px; border: 2px solid #7aa2f7; border-radius: 24px; } .workspace-badge.chosen { background: #7aa2f7; color: #24283b; } .hint { background: #24283b; color: #7aa2f7; font-size: 32px; padding: 12px; border: 2px solid #7aa2f7; } .controller { background: #24283b; color: #c0caf5; padding: 16px; }");gtk::style_context_add_provider_for_display(&display,&css,gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
  window.connect_realize(|w|{if let Some(surface)=w.surface(){surface.set_input_region(Some(&gtk::cairo::Region::create()));}});
  let (commands_tx,commands_rx)=mpsc::sync_channel(8);let (updates_tx,updates_rx)=mpsc::sync_channel(4);let commands_rx=Rc::new(std::cell::RefCell::new(Some(commands_rx)));
  let mapped=Rc::new(Cell::new(false));let config_map=config.clone();let worker_map=worker_app.clone();let stop_map=stopping.clone();let watch_map=parent_watch.clone();
  window.connect_map(move|w|{if mapped.replace(true){return;}gtk::prelude::WidgetExt::display(w).flush();let config=config_map.clone();let tx=updates_tx.clone();let Some(rx)=commands_rx.borrow_mut().take()else{return;};let stop=stop_map.clone();let watch=watch_map.clone();let thread=std::thread::Builder::new().name("vimarchy-data".into()).spawn(move||work(config,rx,tx,stop,watch));if let Ok(thread)=thread{*worker_map.lock().unwrap()=Some(thread);}println!("managed_surface_mapped=true keyboard_none=true input_region_empty=true");});
  let hints=Rc::new(std::cell::RefCell::new(std::collections::BTreeSet::<String>::new()));
  let phase=Rc::new(Cell::new(Phase::Mapping));
  let modifier_alt=Rc::new(Cell::new(false));let selected=Rc::new(Cell::new(None::<char>));
  let hint_labels=Rc::new(std::cell::RefCell::new(std::collections::BTreeMap::<String,gtk::Label>::new()));
  let geometry=Rc::new(std::cell::RefCell::new(std::collections::BTreeMap::<String,[f64;2]>::new()));
  let radial=Rc::new(std::cell::RefCell::new(None::<crate::radial::View>));

  // One physical lifetime: aliases stay held until every physical Enter is released.
  // Pointer and keyboard cannot create overlapping presses in the worker.
  let input=Rc::new(std::cell::RefCell::new(PhysicalInput::default()));
  let send_down:Rc<dyn Fn(bool)>= {
    let entry=entry.clone();let tx=commands_tx.clone();let hints=hints.clone();let status=status.clone();let phase=phase.clone();let stop=stopping.clone();
    Rc::new(move|alt|{if !matches!(phase.get(),Phase::Selecting|Phase::Exiting){return;}let hint=entry.text().to_string();if !hints.borrow().contains(&hint){status.set_text("Choose a displayed hint");return;}
      if tx.try_send(Action::Down(hint,alt)).is_err(){stop.store(true,Ordering::Release);}else{status.set_text("Physical press sent; release to tap, Alt-hold for workspace");}
    })
  };
  let keys=gtk::EventControllerKey::new();keys.set_propagation_phase(gtk::PropagationPhase::Capture);
  {let alt=modifier_alt.clone();keys.connect_modifiers(move|_,state|{alt.set(state.contains(gtk::gdk::ModifierType::ALT_MASK));glib::Propagation::Proceed});}

  {let input=input.clone();let down=send_down.clone();let tx=commands_tx.clone();let phase=phase.clone();let selected=selected.clone();let stop=stopping.clone();keys.connect_key_pressed(move|_,key,code,state|{
    if !input.borrow_mut().press(code) {if input.borrow().exhausted {stop.store(true,Ordering::Release);}return glib::Propagation::Stop;}

    if key==gtk::gdk::Key::Escape {if tx.try_send(Action::Cancel).is_err(){stop.store(true,Ordering::Release);}return glib::Propagation::Stop;}
    if key==gtk::gdk::Key::Return || key==gtk::gdk::Key::KP_Enter {if input.borrow_mut().key_down(code){down(state.contains(gtk::gdk::ModifierType::ALT_MASK));}return glib::Propagation::Stop;}
    if let Some(c)=key.to_unicode().filter(|c|c.is_ascii_digit()||*c=='s') {let first=input.borrow_mut().workspace_down(code,c);if phase.get()==Phase::Radial {if first {selected.set(Some(c));if tx.try_send(Action::Workspace(c,state.contains(gtk::gdk::ModifierType::ALT_MASK))).is_err(){stop.store(true,Ordering::Release);}}return glib::Propagation::Stop;}}
    glib::Propagation::Proceed
  });}
  {let input=input.clone();let tx=commands_tx.clone();let stop=stopping.clone();keys.connect_key_released(move|_,_,code,_|{
    let release=input.borrow_mut().release(code);
    if release && tx.try_send(Action::Up).is_err(){stop.store(true,Ordering::Release);}
  });}if !passive {window.add_controller(keys);}
  let click=gtk::GestureClick::new();click.set_propagation_phase(gtk::PropagationPhase::Capture);click.set_button(1);
  {let input=input.clone();let down=send_down;click.connect_pressed(move|gesture,_,_,_|{if input.borrow_mut().pointer_down(){down(gesture.current_event_state().contains(gtk::gdk::ModifierType::ALT_MASK));}});}
  {let input=input.clone();let tx=commands_tx.clone();let stop=stopping.clone();click.connect_released(move|_,_,_,_|{if input.borrow_mut().pointer_up() && tx.try_send(Action::Up).is_err(){stop.store(true,Ordering::Release);}});}
  {let tx=commands_tx.clone();let stop=stopping.clone();click.connect_cancel(move|_,_|{if tx.try_send(Action::Cancel).is_err(){stop.store(true,Ordering::Release);}});}if !passive {submit.add_controller(click);}
  // Focus loss cancels the reducer; a later release cannot be recycled as a new press.
  if !passive {window.add_controller(focus_cancellation(commands_tx.clone(),phase.clone(),stopping.clone()));}
  let app_tick=app.clone();let window_tick=window.clone();let stop_tick=stopping.clone();let monitor_name=monitor.connector().map(|s|s.to_string());let watch_tick=parent_watch.clone();
  glib::timeout_add_local(Duration::from_millis(16),move||{
   if stop_tick.load(Ordering::Acquire){app_tick.quit();return glib::ControlFlow::Break;}
   if watch_tick.check().is_err(){window_tick.set_keyboard_mode(KeyboardMode::None);if let Some(surface)=window_tick.surface(){surface.set_input_region(Some(&gtk::cairo::Region::create()));}stop_tick.store(true,Ordering::Release);app_tick.quit();return glib::ControlFlow::Break;}
   for _ in 0..4{match updates_rx.try_recv(){
    Ok(Update::Model(rows))=>{
     let outputs=rows.iter().filter_map(|r|if let Row::Output{name,origin,..}=r{Some((name.clone(),*origin))}else{None}).collect::<std::collections::BTreeMap<_,_>>();
     if outputs.len()!=1 {println!("managed_fixture_output_limit=true");app_tick.quit();return glib::ControlFlow::Break;}
     let (name,origin)=outputs.iter().next().unwrap();
     if monitor_name.as_ref()!=Some(name){println!("managed_output_mismatch=true actual={monitor_name:?} expected={name}");app_tick.quit();return glib::ControlFlow::Break;}
     for row in rows{if let Row::Hint{hint,output,at,size,..}=row{if output!=*name{app_tick.quit();return glib::ControlFlow::Break;}geometry.borrow_mut().insert(hint.clone(),[at[0]+size[0]/2.-origin[0],at[1]+size[1]/2.-origin[1]]);let label=gtk::Label::new(Some(&hint));label.add_css_class("hint");label.set_can_target(false);label.set_can_focus(false);fixed.put(&label,at[0]-origin[0],at[1]-origin[1]);hint_labels.borrow_mut().insert(hint.clone(),label);hints.borrow_mut().insert(hint);}}
     loading.set_visible(false);phase.set(Phase::Selecting);
     if passive {
       controls.set_sensitive(false);window_tick.set_keyboard_mode(KeyboardMode::None);
       if let Some(surface)=window_tick.surface(){surface.set_input_region(Some(&gtk::cairo::Region::create()));}
       gtk::prelude::WidgetExt::display(&window_tick).flush();
       println!("managed_passive_ready=true keyboard_none=true input_region_empty=true input_adapter=absent");
     } else {
       controls.set_sensitive(true);window_tick.set_keyboard_mode(KeyboardMode::Exclusive);
       if let Some(surface)=window_tick.surface(){surface.set_input_region(Some(&gtk::cairo::Region::create_rectangle(&gtk::cairo::RectangleInt::new(0,0,window_tick.width(),window_tick.height()))));window_tick.queue_draw();gtk::prelude::WidgetExt::display(&window_tick).flush();}
       entry.set_text("a");entry.grab_focus();
     }
     status.set_text("Authenticated daemon model ready");println!("managed_model_rendered=true hints={}",hints.borrow().len());
     let button=submit.clone();let window=window_tick.clone();glib::timeout_add_local_once(Duration::from_millis(60),move||{if let Some(p)=button.compute_point(&window,&gtk::graphene::Point::new(button.width() as f32/2.,button.height() as f32/2.)){println!("managed_submit_point={},{}",p.x(),p.y());}});
    },
    Ok(Update::Applied(current,source))=>{
      if current==Phase::Radial {
        let Some((hint,workspace))=source else{app_tick.quit();return glib::ControlFlow::Break;};
        let mut active=radial.borrow_mut();
        if let Some(view)=active.as_ref(){if view.source!=hint||view.current!=workspace{app_tick.quit();return glib::ControlFlow::Break;}}
        else{let Some(center)=geometry.borrow().get(&hint).copied()else{app_tick.quit();return glib::ControlFlow::Break;};for (id,label)in hint_labels.borrow().iter(){label.set_visible(*id==hint);}*active=Some(crate::radial::View::new(&fixed,hint.clone(),workspace,center));println!("managed_radial_source={hint} current_workspace={workspace} numeric_badges={} authority=unchanged",crate::radial::destinations(workspace).len());}
      }else if let Some(view)=radial.borrow_mut().take(){view.clear(&fixed);selected.set(None);}
      phase.set(current);submit.set_sensitive(!passive);if current==Phase::Closed {app_tick.quit();return glib::ControlFlow::Break;}status.set_text("Daemon acknowledged gesture");println!("managed_ui_phase={current:?}");
    },
    Ok(Update::Fault)|Err(mpsc::TryRecvError::Disconnected)=>{app_tick.quit();return glib::ControlFlow::Break;},
    Err(mpsc::TryRecvError::Empty)=>break,
   }}
   if let Some(view)=radial.borrow().as_ref(){
     let reduced=!gtk::Settings::for_display(&display).is_gtk_enable_animations();
     if !view.render(&fixed,[window_tick.width() as f64,window_tick.height() as f64],selected.get(),reduced){app_tick.quit();return glib::ControlFlow::Break;}
     let mode=if modifier_alt.get(){"Alt: move without following"}else{"Move and follow"};
     let choice=selected.get().map(|c|format!(" · Selected {c}")).unwrap_or_default();
     status.set_text(&format!("{mode}{choice}\nNumeric destinations checked on selection · Scratchpad unavailable · Escape cancels"));
   }
   glib::ControlFlow::Continue
  });
  let tx=commands_tx;let stop_close=stopping.clone();window.connect_close_request(move|_|{stop_close.store(true,Ordering::Release);let _=tx.try_send(Action::Close);glib::Propagation::Proceed});
  window.present();let app_end=app.clone();glib::timeout_add_local_once(Duration::from_secs(4),move||app_end.quit());
 });
    let result = app.run_with_args::<&str>(&[]);
    stop.store(true, Ordering::Release);
    for window in app.windows() {
        window.set_keyboard_mode(KeyboardMode::None);
        if let Some(surface) = window.surface() {
            surface.set_input_region(Some(&gtk::cairo::Region::create()));
        }
        window.destroy();
    }
    if let Some(join) = worker.lock().unwrap().take() {
        let _ = join.join();
    }
    result
}

#[cfg(test)]
mod physical_tests {
    use super::PhysicalInput;
    fn enter(p: &mut PhysicalInput, code: u32) -> bool {
        p.press(code) && p.key_down(code)
    }
    fn number(p: &mut PhysicalInput, code: u32, key: char) -> bool {
        p.press(code) && p.workspace_down(code, key)
    }
    #[test]
    fn aliases_repeat_and_pointer_never_manufacture_second_press() {
        let mut p = PhysicalInput::default();
        assert!(enter(&mut p, 36));
        assert!(!enter(&mut p, 36));
        assert!(!enter(&mut p, 104));
        assert!(!p.release(104));
        assert!(!enter(&mut p, 104));
        assert!(!p.pointer_down());
        assert!(!p.pointer_up());
        assert!(!p.release(36));
        assert!(p.release(104));
        assert!(!p.release(104));
        assert!(p.pointer_down());
        assert!(!enter(&mut p, 36));
        assert!(!p.release(36));
        assert!(p.pointer_up());
        assert!(!p.pointer_up());
        assert!(enter(&mut p, 36));
    }
    #[test]
    fn numeric_alias_release_does_not_rearm_other_physical_key() {
        let mut p = PhysicalInput::default();
        assert!(number(&mut p, 12, '3'));
        assert!(!number(&mut p, 89, '3'));
        p.release(89);
        assert!(!number(&mut p, 89, '3')); // New keypad press; top-row still held.
        assert!(!number(&mut p, 12, '3')); // Top-row repeat after Radial.
        p.release(12);
        assert!(!number(&mut p, 12, '3')); // Keypad remains held too.
        p.release(12);
        p.release(89);
        assert!(number(&mut p, 89, '3'));
    }
    #[test]
    fn symbol_modifier_phase_changes_never_reclassify_a_repeat() {
        let mut p = PhysicalInput::default();
        assert!(p.press(12)); // Earlier symbol was #, not a workspace key.
        assert!(!number(&mut p, 12, '3')); // Shift released; same physical repeat.
        p.release(99); // Foreign release cannot rearm.
        assert!(!number(&mut p, 12, '3'));
        p.release(12);
        assert!(number(&mut p, 12, '3'));
        p.release(12); // Release is physical even if its symbol has changed.
        assert!(number(&mut p, 12, '3'));
    }
    #[test]
    fn exhaustion_is_bounded_and_sticky_after_all_releases() {
        let mut p = PhysicalInput::default();
        for code in 0..256 {
            assert!(p.press(code));
        }
        assert!(!p.press(999));
        assert!(p.exhausted);
        for code in 0..256 {
            p.release(code);
        }
        assert!(!p.press(999));
        assert!(!p.pointer_down());
    }
}

#[cfg(test)]
mod presentation_mode_tests {
    #[test]
    fn explicit_passive_and_fixture_modes_reject_unknown_values() {
        assert_eq!(super::passive_mode(Some("passive")), Some(true));
        assert_eq!(
            super::passive_mode(Some("interactive-fixture")),
            Some(false)
        );
        assert_eq!(super::passive_mode(None), Some(false));
        for value in ["", "true", "PASSIVE", "passive ", "production"] {
            assert_eq!(super::passive_mode(Some(value)), None);
        }
    }
}
