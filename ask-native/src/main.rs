mod permission_keys;
mod transcript;
use ask_core::{
    Text,
    launch::{Command as LaunchCommand, FrozenLaunch, Harness},
};
use ask_native::{
    Permission,
    backend::{Command, Handle, View},
    files,
};
use gtk::{glib, prelude::*};
use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
    time::{Duration, Instant},
};

struct Presentation {
    parent: Option<modal_runtime::readiness::ParentWatch>,
    alive: Rc<Cell<bool>>,
    active: Rc<Cell<bool>>,
    window: glib::WeakRef<gtk::ApplicationWindow>,
    column: glib::WeakRef<gtk::Box>,
}
impl Presentation {
    fn check_lifetime(&self) -> bool {
        if !self.alive.get() {
            return false;
        }
        if self
            .parent
            .as_ref()
            .is_some_and(|parent| parent.check().is_err())
        {
            self.alive.set(false);
            self.active.set(false);
            if let Some(column) = self.column.upgrade() {
                column.set_sensitive(false);
            }
            if let Some(window) = self.window.upgrade() {
                window.set_keyboard_mode(KeyboardMode::None);
                if let Some(surface) = window.surface() {
                    surface.set_input_region(Some(&gtk::cairo::Region::create()));
                }
                println!("managed_parent_lost=true actions_revoked=true");
                window.close();
            }
            return false;
        }
        true
    }
    fn allows_action(&self) -> bool {
        self.check_lifetime() && self.active.get()
    }
}

fn enqueue(
    presentation: &Presentation,
    tx: &mpsc::SyncSender<Command>,
    sequence: &Cell<u64>,
    make: impl FnOnce(u64) -> Command,
    status: &gtk::Label,
) -> Option<u64> {
    if !presentation.allows_action() {
        return None;
    }
    let Some(next) = sequence.get().checked_add(1) else {
        status.set_text("Action sequence exhausted");
        return None;
    };
    match tx.try_send(make(next)) {
        Ok(()) => {
            sequence.set(next);
            Some(next)
        }
        Err(_) => {
            status.set_text("Agent command queue busy; retry");
            None
        }
    }
}
fn fixture_point(
    widget: &impl IsA<gtk::Widget>,
    window: &gtk::ApplicationWindow,
    tag: &'static str,
) {
    let widget = widget.as_ref().clone();
    let observed = widget.clone();
    let window = window.clone();
    widget.connect_map(move |_| {
        let widget = observed.clone();
        let window = window.clone();
        glib::timeout_add_local_once(Duration::from_millis(80), move || {
            if let Some(point) = widget.compute_point(
                &window,
                &gtk::graphene::Point::new(widget.width() as f32 / 2., widget.height() as f32 / 2.),
            ) {
                println!(
                    "fixture_point={tag} x={} y={}",
                    point.x().round() as i32,
                    point.y().round() as i32
                );
            }
        });
    });
}
fn main() -> glib::ExitCode {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--fixture-provider") {
        return if ask_native::fixture::serve().is_ok() {
            glib::ExitCode::SUCCESS
        } else {
            glib::ExitCode::FAILURE
        };
    }
    if args.len() == 2
        && args[1] == "--transcript-check"
        && std::env::var("OMARCHY_TEST_ISOLATED_DISPLAY").as_deref() == Ok("yes")
    {
        return if transcript::fixture_check().is_ok() {
            glib::ExitCode::SUCCESS
        } else {
            glib::ExitCode::FAILURE
        };
    }
    if std::env::var("OMARCHY_TEST_ISOLATED_DISPLAY").as_deref() != Ok("yes")
        || args.len() != 3
        || args[1] != "--fixture-ui"
    {
        eprintln!("usage: isolated display required; ask-native --fixture-ui EXPLICIT_ROOT");
        return glib::ExitCode::FAILURE;
    }
    let root = PathBuf::from(&args[2]);
    if !root.is_absolute() || !root.is_dir() {
        eprintln!("explicit absolute fixture root required");
        return glib::ExitCode::FAILURE;
    }
    let executable = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return glib::ExitCode::FAILURE,
    };
    let launch = FrozenLaunch {
        harness: Harness::Codex,
        cwd: root.clone(),
        command: LaunchCommand::new(vec![
            executable.to_string_lossy().into_owned(),
            "--fixture-provider".into(),
        ])
        .unwrap(),
        model: None,
        reasoning: None,
    };
    let parent = if std::env::var_os("OMARCHY_MODAL_NAMESPACE").is_some() {
        match modal_runtime::readiness::ParentWatch::from_environment() {
            Ok(watch) => Some(watch),
            Err(_) => {
                eprintln!("managed_parent_invalid=true");
                return glib::ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    let backend = Rc::new(RefCell::new(Some(Handle::start_guarded(launch, parent))));
    let files = Rc::new(RefCell::new(Some(files::Handle::start(root))));
    let app = gtk::Application::builder()
        .application_id("org.omarchy.AskFixture")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let worker = backend.clone();
    let file_worker = files.clone();
    app.connect_activate(move |app| {
        let backend_ref = worker.borrow();
        let backend = backend_ref.as_ref().unwrap();
        let tx = backend.commands.clone();
        let views = backend.views.clone();
        drop(backend_ref);
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Ask · native fixture")
            .default_width(960)
            .default_height(640)
            .build();
        let column = gtk::Box::new(gtk::Orientation::Vertical, 10);
        for side in [0, 1, 2, 3] {
            match side {
                0 => column.set_margin_top(16),
                1 => column.set_margin_bottom(16),
                2 => column.set_margin_start(16),
                _ => column.set_margin_end(16),
            }
        }
        let heading = gtk::Label::new(Some("Ask"));
        heading.add_css_class("title-1");
        heading.set_xalign(0.0);
        column.append(&heading);
        let status = gtk::Label::new(Some("Starting fixture agent…"));
        status.set_wrap(true);
        status.set_xalign(0.0);
        status.update_property(&[gtk::accessible::Property::Label("Agent session status")]);
        column.append(&status);
        let policy = gtk::Label::new(Some(
            "Interactive approval · fixture provider · no stored auto-policy",
        ));
        policy.set_xalign(0.0);
        column.append(&policy);
        let transcript = Rc::new(RefCell::new(transcript::Transcript::new()));
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .min_content_height(180)
            .child(transcript.borrow().widget())
            .build();
        let tail_follow = transcript::TailFollow::new(&scroll.vadjustment());
        column.append(&scroll);
        let permission_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        permission_box.add_css_class("card");
        column.append(&permission_box);
        let composer = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .build();
        composer.update_property(&[gtk::accessible::Property::Label("Message to agent")]);
        {
            let status = status.clone();
            composer
                .buffer()
                .connect_insert_text(move |buffer, _, inserted| {
                    let current = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
                    if current.len().saturating_add(inserted.len()) > 65_536 {
                        buffer.stop_signal_emission_by_name("insert-text");
                        status.set_text("Message limit is 64 KiB");
                    }
                });
        }
        let editor_scroll = gtk::ScrolledWindow::builder()
            .min_content_height(85)
            .max_content_height(160)
            .child(&composer)
            .build();
        column.append(&editor_scroll);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let send = gtk::Button::with_label("Send · Ctrl+Enter");
        send.set_sensitive(false);
        let cancel = gtk::Button::with_label("Cancel turn");
        cancel.set_sensitive(false);
        let pin = gtk::Button::with_label("Pin conversation");
        let close = gtk::Button::with_label("Close");
        for b in [&send, &cancel, &pin, &close] {
            buttons.append(b)
        }
        column.append(&buttons);
        let file_entry = gtk::Entry::builder()
            .placeholder_text("Search only the explicit fixture root")
            .build();
        file_entry.update_property(&[gtk::accessible::Property::Label("Scoped file query")]);
        let search = gtk::Button::with_label("Search files");
        let searchbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        file_entry.set_hexpand(true);
        searchbar.append(&file_entry);
        searchbar.append(&search);
        column.append(&searchbar);
        let file_rows = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let file_scroll = gtk::ScrolledWindow::builder()
            .min_content_height(65)
            .max_content_height(100)
            .child(&file_rows)
            .build();
        column.append(&file_scroll);
        let preview = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let preview_scroll = gtk::ScrolledWindow::builder()
            .max_content_height(160)
            .propagate_natural_height(true)
            .child(&preview)
            .build();
        column.append(&preview_scroll);
        window.set_child(Some(&column));
        fixture_point(&composer, &window, "composer");
        fixture_point(&send, &window, "send");
        fixture_point(&file_entry, &window, "file_query");
        fixture_point(&search, &window, "file_search");
        let sequence = Rc::new(Cell::new(0u64));
        let pending_submit = Rc::new(RefCell::new(None::<(u64, String)>));
        let last_view = Rc::new(RefCell::new(None::<View>));
        let displayed_permissions = Rc::new(RefCell::new(Vec::<Permission>::new()));
        let permission_buttons = Rc::new(RefCell::new(Vec::<(bool, gtk::Button)>::new()));
        {
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            let buttons = permission_buttons.clone();
            let held = Rc::new(RefCell::new(permission_keys::DecisionKeys::default()));
            let pressed = held.clone();
            keys.connect_key_pressed(move |_, key, keycode, mods| {
                let fresh = pressed.borrow_mut().press(keycode);
                let allow = match key {
                    gtk::gdk::Key::y | gtk::gdk::Key::Y => true,
                    gtk::gdk::Key::n | gtk::gdk::Key::N => false,
                    _ => return glib::Propagation::Proceed,
                };
                if mods.intersects(
                    gtk::gdk::ModifierType::CONTROL_MASK
                        | gtk::gdk::ModifierType::ALT_MASK
                        | gtk::gdk::ModifierType::SUPER_MASK,
                ) || buttons.borrow().is_empty()
                {
                    return glib::Propagation::Proceed;
                }
                if fresh
                    && let Some((_, button)) =
                        buttons.borrow().iter().find(|(choice, _)| *choice == allow)
                    && button.is_sensitive()
                {
                    button.emit_clicked();
                }
                glib::Propagation::Stop
            });
            keys.connect_key_released(move |_, _, keycode, _| {
                held.borrow_mut().release(keycode);
            });
            window.add_controller(keys);
        }
        let file_generation = Rc::new(Cell::new(0u64));
        let alive = Rc::new(Cell::new(true));
        let managed = std::env::var("OMARCHY_MODAL_NAMESPACE").ok();
        let authority = Rc::new(Cell::new(managed.is_none()));
        let parent = if managed.is_some() {
            match modal_runtime::readiness::ParentWatch::from_environment() {
                Ok(parent) => Some(parent),
                Err(_) => {
                    eprintln!("managed_parent_invalid=true");
                    app.quit();
                    return;
                }
            }
        } else {
            None
        };
        let presentation = Rc::new(Presentation {
            parent,
            alive: alive.clone(),
            active: authority.clone(),
            window: window.downgrade(),
            column: column.downgrade(),
        });
        {
            let tx = tx.clone();
            let seq = sequence.clone();
            let status = status.clone();
            let buffer = composer.buffer();
            let pending = pending_submit.clone();
            let presentation = presentation.clone();
            send.connect_clicked(move |button| {
                if !button.is_sensitive() {
                    return;
                }
                let raw = buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string();
                let text = match Text::new(raw.clone()) {
                    Ok(t) if !t.is_empty() => t,
                    _ => {
                        status.set_text("Enter a message up to 64 KiB");
                        return;
                    }
                };
                if let Some(n) = enqueue(
                    &presentation,
                    &tx,
                    &seq,
                    |sequence| Command::Submit { sequence, text },
                    &status,
                ) {
                    *pending.borrow_mut() = Some((n, raw));
                    button.set_sensitive(false);
                    println!("ui_submit_queued=true");
                }
            });
        }
        {
            let key = gtk::EventControllerKey::new();
            let send = send.clone();
            key.connect_key_pressed(move |_, key, _, mods| {
                if key == gtk::gdk::Key::Return
                    && mods.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                {
                    if send.is_sensitive() {
                        send.emit_clicked();
                    }
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
            composer.add_controller(key);
        }
        {
            let tx = tx.clone();
            let seq = sequence.clone();
            let status = status.clone();
            let presentation = presentation.clone();
            cancel.connect_clicked(move |_| {
                enqueue(
                    &presentation,
                    &tx,
                    &seq,
                    |sequence| Command::Cancel { sequence },
                    &status,
                );
                println!("ui_cancel_queued=true");
            });
        }
        {
            let tx = tx.clone();
            let seq = sequence.clone();
            let status = status.clone();
            let presentation = presentation.clone();
            pin.connect_clicked(move |_| {
                enqueue(
                    &presentation,
                    &tx,
                    &seq,
                    |sequence| Command::Pin { sequence },
                    &status,
                );
            });
        }
        {
            let app = app.clone();
            close.connect_clicked(move |_| app.quit());
        }
        {
            let tx = tx.clone();
            window.connect_is_active_notify(move |w| {
                let _ = tx.try_send(Command::Focus(w.is_active()));
            });
        }
        {
            let files = file_worker.clone();
            let generation = file_generation.clone();
            let status = status.clone();
            let entry = file_entry.clone();
            let presentation = presentation.clone();
            search.connect_clicked(move |_| {
                if !presentation.allows_action() {
                    return;
                }
                if let Some(worker) = files.borrow().as_ref() {
                    match worker.submit(files::Kind::Search(entry.text().to_string())) {
                        Ok(g) => generation.set(g),
                        Err(e) => status.set_text(e),
                    }
                }
            });
        }
        {
            let button = search.clone();
            file_entry.connect_activate(move |_| button.emit_clicked());
        }
        {
            let alive = alive.clone();
            window.connect_close_request(move |_| {
                alive.set(false);
                glib::Propagation::Proceed
            });
        }
        column.set_sensitive(managed.is_none());
        if let Some(namespace) = managed {
            if !gtk4_layer_shell::is_supported() {
                status.set_text("Managed presentation unavailable");
                app.quit();
                return;
            }
            window.init_layer_shell();
            window.set_namespace(Some(&namespace));
            window.set_layer(Layer::Overlay);
            window.set_keyboard_mode(KeyboardMode::None);
            window.connect_realize(|window| {
                if let Some(surface) = window.surface() {
                    surface.set_input_region(Some(&gtk::cairo::Region::create()));
                }
            });
            window.set_exclusive_zone(0);
            pin.set_sensitive(false);
            let socket = std::env::var_os("OMARCHY_MODAL_SOCKET").map(PathBuf::from);
            let parent = std::env::var("OMARCHY_MODAL_PARENT_PID")
                .ok()
                .and_then(|s| s.parse::<u32>().ok());
            let hello = std::env::var("OMARCHY_MODAL_READY").ok();
            let (grant_tx, grant_rx) = mpsc::sync_channel(1);
            let sent = Rc::new(Cell::new(false));
            window.connect_map(move |w| {
                if sent.replace(true) {
                    return;
                }
                if let Some(display) = gtk::gdk::Display::default() {
                    display.flush()
                }
                let (socket, parent, hello) = (socket.clone(), parent, hello.clone());
                let tx = grant_tx.clone();
                glib::idle_add_local_once(move || {
                    std::thread::spawn(move || {
                        let allowed = match (socket, parent, hello) {
                            (Some(socket), Some(parent), Some(hello)) => {
                                modal_runtime::readiness::report_mapped(
                                    &socket,
                                    parent,
                                    &hello,
                                    Instant::now() + Duration::from_secs(2),
                                )
                                .is_ok()
                            }
                            _ => false,
                        };
                        let _ = tx.try_send(allowed);
                    });
                });
                println!(
                    "mapped_managed=true authority_pending=true size={}x{}",
                    w.width(),
                    w.height()
                );
            });
            let window = window.clone();
            let composer = composer.clone();
            let alive = alive.clone();
            let authority = authority.clone();
            let column = column.clone();
            let status = status.clone();
            let presentation = presentation.clone();
            glib::timeout_add_local(Duration::from_millis(10), move || {
                if !presentation.check_lifetime() || !alive.get() {
                    return glib::ControlFlow::Break;
                }
                match grant_rx.try_recv() {
                    Ok(true) => {
                        if window.is_mapped() {
                            authority.set(true);
                            if let Some(surface) = window.surface() {
                                surface.set_input_region(None);
                            }
                            column.set_sensitive(true);
                            window.set_keyboard_mode(KeyboardMode::Exclusive);
                            composer.grab_focus();
                            println!("authenticated_modal_grant=true");
                        }
                        glib::ControlFlow::Break
                    }
                    Ok(false) => {
                        status.set_text("Presentation authority denied");
                        window.close();
                        glib::ControlFlow::Break
                    }
                    Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
                    Err(_) => glib::ControlFlow::Continue,
                }
            });
        }
        let focus_composer = composer.clone();
        let ui_window = window.clone();
        let ui_files = file_worker.clone();
        let ui_alive = alive.clone();
        let ui_presentation = presentation.clone();
        glib::timeout_add_local(Duration::from_millis(16), move || {
            if !ui_presentation.check_lifetime() || !ui_alive.get() {
                return glib::ControlFlow::Break;
            }
            if let Some(view) = views.take() {
                status.set_text(&view.status);
                send.set_sensitive(view.ready && !view.busy && pending_submit.borrow().is_none());
                cancel.set_sensitive(view.busy);
                if let Some(ack) = &view.ack {
                    let mut pending = pending_submit.borrow_mut();
                    if pending.as_ref().is_some_and(|(n, _)| *n == ack.sequence) {
                        if ack.accepted {
                            let raw = composer.buffer().text(
                                &composer.buffer().start_iter(),
                                &composer.buffer().end_iter(),
                                false,
                            );
                            if pending
                                .as_ref()
                                .is_some_and(|(_, sent)| sent == raw.as_str())
                            {
                                composer.buffer().set_text("");
                            }
                        }
                        *pending = None;
                        send.set_sensitive(view.ready && !view.busy);
                    }
                }
                if last_view
                    .borrow()
                    .as_ref()
                    .is_none_or(|old| old.transcript != view.transcript)
                {
                    let lifetime = ui_presentation.clone();
                    tail_follow.schedule(move || lifetime.check_lifetime());
                    transcript.borrow_mut().update(&view.transcript);
                    if view
                        .transcript
                        .iter()
                        .any(|(_, body)| body.as_str() == "Fixture completed · Unicode 日本語 😀")
                    {
                        println!("fixture_answer_rendered=true");
                    }
                    if view
                        .transcript
                        .iter()
                        .any(|(_, body)| body.as_str() == "Tool declined; no resource was read.")
                    {
                        println!("fixture_denied_rendered=true");
                    }
                    if view
                        .transcript
                        .iter()
                        .any(|(_, body)| body.as_str() == "Tool cancelled; no option selected.")
                    {
                        println!("fixture_cancelled_rendered=true");
                    }
                    println!(
                        "transcript_messages={} busy={}",
                        view.transcript.len(),
                        view.busy
                    );
                }
                if *displayed_permissions.borrow() != view.permissions {
                    permission_buttons.borrow_mut().clear();
                    while let Some(child) = permission_box.first_child() {
                        permission_box.remove(&child)
                    }
                    if let Some(permission) = view.permissions.first() {
                        let title = gtk::Label::new(Some(permission.title.as_str()));
                        title.set_wrap(true);
                        title.set_xalign(0.0);
                        permission_box.append(&title);
                        let queued = gtk::Label::new(Some(&format!(
                            "{} more requests queued",
                            view.permissions.len().saturating_sub(1)
                        )));
                        queued.set_xalign(0.0);
                        permission_box.append(&queued);
                        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                        for (allow, label) in [(true, "Y · Allow once"), (false, "N · Deny once")]
                        {
                            if allow && permission.choice(true).is_none() {
                                continue;
                            }
                            let label = if !allow && permission.choice(false).is_none() {
                                "N · Cancel request"
                            } else {
                                label
                            };
                            let button = gtk::Button::with_label(label);
                            fixture_point(
                                &button,
                                &ui_window,
                                if allow {
                                    "permission_allow"
                                } else {
                                    "permission_deny"
                                },
                            );
                            permission_buttons
                                .borrow_mut()
                                .push((allow, button.clone()));
                            let choice_row = row.downgrade();
                            let captured = permission.clone();
                            let tx = tx.clone();
                            let seq = sequence.clone();
                            let status = status.clone();
                            button.set_sensitive(true);
                            let presentation = ui_presentation.clone();
                            button.connect_clicked(move |button| {
                                if !button.is_sensitive() {
                                    return;
                                }
                                if enqueue(
                                    &presentation,
                                    &tx,
                                    &seq,
                                    |sequence| Command::Answer {
                                        sequence,
                                        permission: captured.clone(),
                                        allow,
                                    },
                                    &status,
                                )
                                .is_some()
                                {
                                    if let Some(row) = choice_row.upgrade() {
                                        row.set_sensitive(false);
                                    }
                                    println!("ui_permission_answer_queued=true allow={allow}");
                                }
                            });
                            row.append(&button);
                        }
                        permission_box.append(&row);
                        println!("permission_rendered=true queue={}", view.permissions.len());
                    }
                    *displayed_permissions.borrow_mut() = view.permissions.clone();
                }
                if view.pinned {
                    ui_window.set_title(Some("Ask · pinned fixture conversation"));
                    pin.set_sensitive(false)
                }
                if view.attention {
                    status.set_text("Response ready · pinned conversation");
                }
                *last_view.borrow_mut() = Some(view);
            }
            if let Some(update) = ui_files
                .borrow()
                .as_ref()
                .and_then(|worker| worker.updates.take())
                && update.generation == file_generation.get()
            {
                match update.result {
                    files::Result::Search(rows) => {
                        while let Some(child) = file_rows.first_child() {
                            file_rows.remove(&child)
                        }
                        for selected in rows.rows {
                            let label = selected
                                .path()
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string();
                            let button = gtk::Button::with_label(&label);
                            fixture_point(&button, &ui_window, "file_result");
                            let files = ui_files.clone();
                            let generation = file_generation.clone();
                            let status = status.clone();
                            let presentation = ui_presentation.clone();
                            button.connect_clicked(move |_| {
                                if !presentation.allows_action() {
                                    return;
                                }
                                if let Some(worker) = files.borrow().as_ref() {
                                    match worker.submit(files::Kind::Preview(selected.clone())) {
                                        Ok(g) => generation.set(g),
                                        Err(e) => status.set_text(e),
                                    }
                                }
                            });
                            file_rows.append(&button);
                        }
                        status.set_text(if rows.complete {
                            "Scoped file search complete"
                        } else {
                            "Scoped file search partial · budget reached"
                        });
                        println!("file_search_rendered=true");
                    }
                    files::Result::Preview(value) => {
                        while let Some(child) = preview.first_child() {
                            preview.remove(&child)
                        }
                        match value {
                            ask_preview::Preview::Text { text, truncated } => {
                                let label = gtk::Label::new(Some(&text));
                                label.set_wrap(true);
                                label.set_selectable(true);
                                label.set_max_width_chars(90);
                                preview.append(&label);
                                if truncated {
                                    preview.append(&gtk::Label::new(Some("Preview truncated")));
                                }
                            }
                            ask_preview::Preview::Rgba {
                                width,
                                height,
                                pixels,
                            } => {
                                let bytes = glib::Bytes::from_owned(pixels);
                                let texture = gtk::gdk::MemoryTexture::new(
                                    width as i32,
                                    height as i32,
                                    gtk::gdk::MemoryFormat::R8g8b8a8,
                                    &bytes,
                                    width as usize * 4,
                                );
                                let picture = gtk::Picture::for_paintable(&texture);
                                picture.set_size_request(160, 160);
                                picture.set_can_shrink(true);
                                preview.append(&picture);
                            }
                            ask_preview::Preview::Unsupported => {
                                preview.append(&gtk::Label::new(Some("Preview format unavailable")))
                            }
                        }
                        println!("file_preview_rendered=true");
                    }
                    files::Result::Failed(error) => status.set_text(&error),
                }
            }
            glib::ControlFlow::Continue
        });
        window.present();
        if std::env::var_os("OMARCHY_MODAL_NAMESPACE").is_none() {
            focus_composer.grab_focus();
        }
        println!("gtk_fixture_mapped=true installed_desktop_acceptance=false");
        let app = app.clone();
        glib::timeout_add_local_once(Duration::from_secs(8), move || {
            println!("fixture_lifecycle=closing");
            app.quit();
        });
    });
    let result = app.run_with_args::<&str>(&[]);
    let clean = backend.borrow_mut().take().is_some_and(Handle::finish);
    let files_clean = files.borrow_mut().take().is_some_and(files::Handle::finish);
    println!("agent_reaped={clean} file_worker_joined={files_clean}");
    if clean && files_clean {
        result
    } else {
        glib::ExitCode::FAILURE
    }
}
