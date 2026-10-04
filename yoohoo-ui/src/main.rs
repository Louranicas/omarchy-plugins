//! Isolated executable GTK sidebar; no live shell bindings or automatic activation.
use gtk::{glib, prelude::*};
use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
mod input;
mod managed;
use input::{Action, DisplayedCommand, Keys};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use yoohoo_runtime::{Command, Request, Response, Settings, View, ipc};
type RenderSignature = (u64, u64, bool, Vec<(u64, bool)>);
fn main() -> glib::ExitCode {
    let managed = std::env::var_os("OMARCHY_MODAL_CONTROLLER_SOCKET").is_some();
    if std::env::var("YOOHOO_ISOLATED_DISPLAY").as_deref() != Ok("yes")
        && !(managed && std::env::var("OMARCHY_TEST_ISOLATED_DISPLAY").as_deref() == Ok("yes"))
    {
        eprintln!("refused: explicitly isolated display required");
        return glib::ExitCode::FAILURE;
    }
    let args: Vec<_> = std::env::args().collect();
    if !managed && args.len() != 3 {
        eprintln!("usage: yoohoo-ui SOCKET OWNED_DAEMON_PID");
        return glib::ExitCode::FAILURE;
    }
    let socket = if managed {
        PathBuf::new()
    } else {
        PathBuf::from(&args[1])
    };
    let pid = if managed {
        0
    } else {
        match args[2].parse::<u32>() {
            Ok(pid) => pid,
            Err(_) => return glib::ExitCode::FAILURE,
        }
    };
    let parent = if managed {
        match modal_runtime::readiness::ParentWatch::from_environment() {
            Ok(p) => Some(Arc::new(p)),
            Err(_) => return glib::ExitCode::FAILURE,
        }
    } else {
        None
    };
    let (mapped_tx, mapped_rx) = mpsc::sync_channel(1);
    let (commands, rx) = mpsc::sync_channel::<DisplayedCommand>(8);
    let (updates, results) = mpsc::sync_channel::<Result<Response, String>>(4);
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let parent_worker = parent.clone();
    let worker = thread::spawn(move || {
        if managed {
            managed::run(rx, updates, worker_stop, parent_worker.unwrap(), mapped_rx);
            return;
        }
        let mut request_id = 0u64;
        let mut last_generation = None;
        let mut next = Some(Command::Open);
        let mut current: Option<View> = None;
        while !worker_stop.load(Ordering::Acquire) {
            let command = if let Some(command) = next.take() {
                command
            } else {
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(input) if current.as_ref().is_some_and(|v| input.matches(v)) => {
                        input.command
                    }
                    Ok(_) => {
                        let _ =
                            updates.try_send(Err("Displayed state changed; choose again".into()));
                        continue;
                    }
                    Err(_) => Command::List,
                }
            };
            let Some(id) = request_id.checked_add(1) else {
                break;
            };
            request_id = id;
            let response = ipc::call(
                &socket,
                pid,
                Request {
                    version: 1,
                    request_id,
                    command,
                },
            )
            .map_err(|_| "Runtime unavailable; actions disabled".into());
            if let Ok(value) = &response {
                current = Some(value.view.clone());
                if value.view.open {
                    last_generation = Some(value.view.generation);
                } else {
                    last_generation = None;
                }
            }
            let _ = updates.try_send(response);
        }
        if let (Some(generation), Some(request_id)) = (last_generation, request_id.checked_add(1)) {
            let _ = ipc::call(
                &socket,
                pid,
                Request {
                    version: 1,
                    request_id,
                    command: Command::Close { generation },
                },
            );
        }
    });
    let app = gtk::Application::builder()
        .application_id("org.omarchy.YoohooFixture")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let results = Rc::new(RefCell::new(Some(results)));
    let stop_app = stop.clone();
    app.connect_activate(move |app| {
        let Some(results) = results.borrow_mut().take() else {
            return;
        };
        let view: Rc<RefCell<Option<View>>> = Rc::new(RefCell::new(None));
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Yoohoo — isolated native sidebar")
            .default_width(560)
            .default_height(680)
            .build();
        if managed {
            if !gtk4_layer_shell::is_supported() {
                app.quit();
                return;
            }
            window.init_layer_shell();
            window.set_namespace(std::env::var("OMARCHY_MODAL_NAMESPACE").ok().as_deref());
            window.set_layer(Layer::Overlay);
            window.set_keyboard_mode(KeyboardMode::None);
            window.set_exclusive_zone(0);
            window.connect_realize(|w| {
                if let Some(s) = w.surface() {
                    s.set_input_region(Some(&gtk::cairo::Region::create()));
                }
            });
            let tx = mapped_tx.clone();
            window.connect_map(move |w| {
                gtk::prelude::WidgetExt::display(w).flush();
                let _ = tx.try_send(());
            });
        }
        let column = gtk::Box::new(gtk::Orientation::Vertical, 10);
        column.set_sensitive(!managed);
        {
            let margin = &column;
            margin.set_margin_top(16);
            margin.set_margin_bottom(16);
            margin.set_margin_start(16);
            margin.set_margin_end(16);
        }
        let title = gtk::Label::new(Some("Yoohoo · Attention"));
        title.add_css_class("title-1");
        title.set_xalign(0.0);
        let health = gtk::Label::new(Some("Connecting to isolated fixture…"));
        health.set_xalign(0.0);
        health.set_wrap(true);
        health.update_property(&[gtk::accessible::Property::Label("Attention source health")]);
        let status = gtk::Label::new(Some("No activation is performed automatically"));
        status.set_wrap(true);
        status.set_xalign(0.0);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.update_property(&[gtk::accessible::Property::Label(
            "Windows requesting attention",
        )]);
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .min_content_height(180)
            .child(&list)
            .build();
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let previous = gtk::Button::with_label("Previous");
        let next = gtk::Button::with_label("Next");
        let accept = gtk::Button::with_label("Request activation");
        let clear = gtk::Button::with_label("Clear");
        for b in [&previous, &next, &accept, &clear] {
            buttons.append(b);
        }
        let settings_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let heading = gtk::Label::new(Some(
            "Settings · volatile until storage adapter is connected",
        ));
        heading.set_wrap(true);
        heading.set_xalign(0.0);
        let sound = gtk::CheckButton::with_label("Sound requested (backend unavailable)");
        let history = gtk::CheckButton::with_label("Keep history in memory (off by default)");
        let motion = gtk::CheckButton::with_label("Reduced motion");
        let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
        volume.set_value(0.35);
        volume.update_property(&[gtk::accessible::Property::Label("Requested sound volume")]);
        let save = gtk::Button::with_label("Apply settings");
        settings_box.append(&heading);
        settings_box.append(&sound);
        settings_box.append(&history);
        settings_box.append(&motion);
        settings_box.append(&volume);
        settings_box.append(&save);
        let close = gtk::Button::with_label("Close isolated sidebar");
        let quit = app.clone();
        close.connect_clicked(move |_| quit.quit());
        for widget in [
            title.clone().upcast::<gtk::Widget>(),
            health.clone().upcast(),
            status.clone().upcast(),
            scroll.upcast(),
            buttons.upcast(),
            settings_box.upcast(),
            close.upcast(),
        ] {
            column.append(&widget);
        }
        window.set_child(Some(&column));
        for (button, direction) in [(previous, false), (next, true)] {
            let state = view.clone();
            let tx = commands.clone();
            button.connect_clicked(move |_| {
                if let Some(v) = state.borrow().as_ref() {
                    let command = if direction {
                        Command::MoveSelection {
                            generation: v.generation,
                            revision: v.revision,
                            next: true,
                        }
                    } else {
                        Command::MoveSelection {
                            generation: v.generation,
                            revision: v.revision,
                            next: false,
                        }
                    };
                    if let Some(input) = DisplayedCommand::capture(v, command) {
                        let _ = tx.try_send(input);
                    }
                }
            });
        }
        let state = view.clone();
        let tx = commands.clone();
        let selected = list.clone();
        accept.connect_clicked(move |_| {
            if let Some(v) = state.borrow().as_ref()
                && let Some(row) = selected
                    .selected_row()
                    .and_then(|r| v.rows.get(r.index() as usize))
            {
                let command = Command::Activate {
                    generation: v.generation,
                    revision: v.revision,
                    id: row.id,
                };
                if let Some(input) = DisplayedCommand::capture(v, command) {
                    let _ = tx.try_send(input);
                }
            }
        });
        let state = view.clone();
        let tx = commands.clone();
        let selected = list.clone();
        clear.connect_clicked(move |_| {
            if let Some(v) = state.borrow().as_ref()
                && let Some(row) = selected
                    .selected_row()
                    .and_then(|r| v.rows.get(r.index() as usize))
            {
                let command = Command::Clear {
                    revision: v.revision,
                    id: row.id,
                };
                if let Some(input) = DisplayedCommand::capture(v, command) {
                    let _ = tx.try_send(input);
                }
            }
        });
        let state = view.clone();
        let tx = commands.clone();
        list.connect_row_activated(move |_, row| {
            if let Some(v) = state.borrow().as_ref()
                && let Some(item) = v.rows.get(row.index() as usize)
            {
                let command = Command::Activate {
                    generation: v.generation,
                    revision: v.revision,
                    id: item.id,
                };
                if let Some(input) = DisplayedCommand::capture(v, command) {
                    let _ = tx.try_send(input);
                }
            }
        });
        let state = view.clone();
        let tx = commands.clone();
        let sound_copy = sound.clone();
        let history_copy = history.clone();
        let motion_copy = motion.clone();
        let volume_copy = volume.clone();
        save.connect_clicked(move |_| {
            if let Some(v) = state.borrow().as_ref() {
                let command = Command::Settings {
                    revision: v.revision,
                    settings: Settings {
                        sound: sound_copy.is_active(),
                        history: history_copy.is_active(),
                        reduced_motion: motion_copy.is_active(),
                        volume: volume_copy.value(),
                    },
                };
                if let Some(input) = DisplayedCommand::capture(v, command) {
                    let _ = tx.try_send(input);
                }
            }
        });
        // Capture every physical press before interpreting symbols, modifiers or state.
        let keys = Rc::new(RefCell::new(Keys::default()));
        let controller = gtk::EventControllerKey::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let held = keys.clone();
        let state = view.clone();
        let tx = commands.clone();
        let selected = list.clone();
        let parent_key = parent.clone();
        let stop_key = stop_app.clone();
        let status_key = status.clone();
        controller.connect_key_pressed(move |_, key, code, modifiers| {
            let fresh = held.borrow_mut().press(code);
            use gtk::gdk::{Key, ModifierType};
            let action = match key {
                Key::Down | Key::KP_Down => Some(Action::Next),
                Key::Up | Key::KP_Up => Some(Action::Previous),
                Key::Tab | Key::ISO_Left_Tab => Some(
                    if modifiers.contains(ModifierType::SHIFT_MASK) || key == Key::ISO_Left_Tab {
                        Action::Previous
                    } else {
                        Action::Next
                    },
                ),
                Key::Return | Key::KP_Enter => Some(Action::Activate),
                Key::Escape => Some(Action::Close),
                Key::r | Key::R => Some(Action::Refresh),
                _ => None,
            };
            let Some(action) = action else {
                return glib::Propagation::Proceed;
            };
            // Consume recognized rejected keys as well: GTK defaults must not activate.
            if !fresh
                || modifiers.intersects(
                    ModifierType::CONTROL_MASK | ModifierType::ALT_MASK | ModifierType::SUPER_MASK,
                )
                || stop_key.load(Ordering::Acquire)
                || parent_key.as_ref().is_some_and(|p| p.check().is_err())
            {
                return glib::Propagation::Stop;
            }
            if let Some(v) = state.borrow().as_ref() {
                let id = selected
                    .selected_row()
                    .and_then(|r| v.rows.get(r.index() as usize))
                    .map(|r| r.id);
                if let Some(input) = input::command(v, action, id)
                    && tx.try_send(input).is_err()
                {
                    status_key.set_text("Input queue full; action not submitted");
                }
            }
            glib::Propagation::Stop
        });
        controller.connect_key_released(move |_, _, code, _| keys.borrow_mut().release(code));
        window.add_controller(controller);
        println!("native_list_role={:?}", list.accessible_role());
        let mut rendered: Option<RenderSignature> = None;
        let mut settings_revision = None;
        let parent_tick = parent.clone();
        let window_tick = window.clone();
        let app_tick = app.clone();
        let stop_tick = stop_app.clone();
        glib::timeout_add_local(Duration::from_millis(16), move || {
            if parent_tick.as_ref().is_some_and(|p| p.check().is_err()) {
                window_tick.set_keyboard_mode(KeyboardMode::None);
                if let Some(s) = window_tick.surface() {
                    s.set_input_region(Some(&gtk::cairo::Region::create()));
                }
                stop_tick.store(true, Ordering::Release);
                println!("yoohoo_parent_lost=true");
                window_tick.close();
                app_tick.quit();
                return glib::ControlFlow::Break;
            }
            for _ in 0..4 {
                let update = match results.try_recv() {
                    Ok(update) => update,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if managed {
                            window_tick.set_keyboard_mode(KeyboardMode::None);
                            if let Some(surface) = window_tick.surface() {
                                surface.set_input_region(Some(&gtk::cairo::Region::create()));
                            }
                            window_tick.close();
                            app_tick.quit();
                            return glib::ControlFlow::Break;
                        }
                        break;
                    }
                };
                match update {
                    Err(message) => {
                        status.set_text(&message);
                        list.set_sensitive(false);
                        *view.borrow_mut() = None;
                        if managed {
                            window_tick.set_keyboard_mode(KeyboardMode::None);
                            if let Some(s) = window_tick.surface() {
                                s.set_input_region(Some(&gtk::cairo::Region::create()));
                            }
                            window_tick.close();
                            app_tick.quit();
                            return glib::ControlFlow::Break;
                        }
                    }
                    Ok(response) => {
                        let v = response.view;
                        if !v.open {
                            stop_tick.store(true, Ordering::Release);
                            app_tick.quit();
                            return glib::ControlFlow::Break;
                        }
                        if managed {
                            if !v.open || v.stale {
                                app_tick.quit();
                                return glib::ControlFlow::Break;
                            }
                            column.set_sensitive(true);
                            window_tick.set_keyboard_mode(KeyboardMode::Exclusive);
                            if let Some(s) = window_tick.surface() {
                                s.set_input_region(None);
                            }
                        }
                        health.set_text(&format!(
                            "{} · native {} · notifications {}\nAudio {} · storage {}{}",
                            v.origin,
                            v.native_health,
                            v.notification_health,
                            v.audio_health,
                            v.storage_health,
                            if v.stale { " · STALE" } else { "" }
                        ));
                        status.set_text(response.error.as_deref().unwrap_or(
                            if v.pending_activation {
                                "Activation intent queued; modal backend pending"
                            } else {
                                &v.effect_backend
                            },
                        ));
                        list.set_sensitive(!v.stale);
                        let signature = (
                            v.revision,
                            v.generation,
                            v.open,
                            v.rows.iter().map(|r| (r.id, r.selected)).collect(),
                        );
                        if rendered.as_ref() != Some(&signature) {
                            while let Some(child) = list.first_child() {
                                list.remove(&child);
                            }
                            for (index, row) in v.rows.iter().enumerate() {
                                let label = gtk::Label::new(Some(&format!(
                                    "{}\n{} · workspace {} · {} {}",
                                    row.title,
                                    row.class,
                                    row.workspace,
                                    row.count,
                                    if row.count == 1 { "alert" } else { "alerts" }
                                )));
                                label.set_xalign(0.0);
                                label.set_wrap(true);
                                label.set_margin_top(8);
                                label.set_margin_bottom(8);
                                label.update_property(&[gtk::accessible::Property::Label(
                                    &format!(
                                        "{}, {} {}, workspace {}",
                                        row.title,
                                        row.count,
                                        if row.count == 1 { "alert" } else { "alerts" },
                                        row.workspace
                                    ),
                                )]);
                                list.append(&label);
                                if row.selected {
                                    list.select_row(list.row_at_index(index as i32).as_ref());
                                }
                            }
                            println!(
                                "rendered rows={} generation={} revision={} source={}",
                                v.rows.len(),
                                v.generation,
                                v.revision,
                                v.origin
                            );
                            rendered = Some(signature);
                        }
                        if settings_revision != Some(v.revision) {
                            sound.set_active(v.settings.sound);
                            history.set_active(v.settings.history);
                            motion.set_active(v.settings.reduced_motion);
                            volume.set_value(v.settings.volume);
                            settings_revision = Some(v.revision);
                        }
                        *view.borrow_mut() = Some(v);
                    }
                }
            }
            glib::ControlFlow::Continue
        });
        window.present();
        let deadline = app.clone();
        glib::timeout_add_local_once(Duration::from_secs(8), move || {
            println!("bounded_lifecycle=quit");
            deadline.quit();
        });
    });
    let exit = app.run_with_args::<&str>(&[]);
    stop.store(true, Ordering::Release);
    for window in app.windows() {
        if managed {
            window.set_keyboard_mode(KeyboardMode::None);
            if let Some(s) = window.surface() {
                s.set_input_region(Some(&gtk::cairo::Region::create()));
            }
        }
        window.destroy();
    }
    let _ = worker.join();
    exit
}
