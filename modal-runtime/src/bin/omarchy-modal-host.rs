//! Controlled configured host. No discovery, installation, or stale-state recovery.
use modal_runtime::{
    backend::PresenterBackend,
    configured::{Admissions, Config, Dialect},
    host::Host,
    native::{NativeCompositor, NativeFocus, SurfaceLifecycle},
    process_identity::Pinned,
};
use std::{
    io,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
static CANCELLED: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    CANCELLED.store(true, Ordering::Release);
}
fn signals() -> io::Result<()> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = stop as *const () as usize;
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    for signal in [libc::SIGINT, libc::SIGTERM] {
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
fn run() -> io::Result<()> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 2 {
        return Err(io::Error::other(
            "usage: omarchy-modal-host --config ABSOLUTE_PRIVATE_JSON | --inspect PID",
        ));
    }
    if arguments[0] == "--inspect" {
        let pid = arguments[1]
            .to_str()
            .ok_or_else(|| io::Error::other("invalid pid"))?
            .parse()
            .map_err(io::Error::other)?;
        let process = Pinned::capture(pid)?;
        println!(
            "{}",
            serde_json::to_string(&process.identity()).map_err(io::Error::other)?
        );
        return Ok(());
    }
    if arguments[0] != "--config" {
        return Err(io::Error::other("unknown operation"));
    }
    let (config, config_digest) = Config::read_with_digest(Path::new(&arguments[1]))?;
    let vimarchy_policy = config
        .vimarchy_settings
        .as_deref()
        .map(modal_runtime::vimarchy_policy::Policy::load)
        .transpose()?;
    let programs = config.pin_presenters()?;
    let compositor = Pinned::pin(config.compositor.process)?;
    let admissions = Admissions::pin(&config)?;
    let dialect = match config.compositor.dialect {
        Dialect::Legacy => desktop_io::DispatchDialect::Legacy,
        Dialect::Lua => desktop_io::DispatchDialect::Lua,
    };
    let identity = compositor.identity();
    let source = NativeFocus::connect(
        &config.compositor.runtime,
        &config.compositor.instance,
        desktop_io::ProcessIdentity {
            pid: identity.pid,
            start_ticks: identity.start_ticks,
        },
        dialect,
        Instant::now() + Duration::from_secs(1),
    )
    .map_err(|e| io::Error::other(format!("native connection refused: {e:?}")))?;
    let lifecycle = SurfaceLifecycle::new(config.runtime.clone(), source.clone());
    let backend = PresenterBackend::new(
        NativeCompositor {
            lifecycle,
            focus: source.clone(),
        },
        config.presenter_specs(),
    )
    .with_controller_endpoints(config.controller_endpoints())
    .with_workspace_move_config(
        config
            .vimarchy_numeric_workspace_moves
            .then(|| (Path::new(&arguments[1]).to_path_buf(), config_digest)),
    )
    .with_vimarchy_policy(vimarchy_policy)
    .with_vimarchy_policy_path(config.vimarchy_settings.clone())
    .with_pinned_programs(programs);
    let mut host = Host::bind(&config.runtime, backend, move |uid, pid| {
        admissions.admit(uid, pid)
    })?;
    let mut observer = source.observer();
    signals()?;
    while !CANCELLED.load(Ordering::Acquire) {
        compositor.check()?;
        observer
            .step(&mut host, Instant::now() + Duration::from_millis(50))
            .map_err(|e| io::Error::other(format!("native authority unavailable: {e:?}")))?;
        std::thread::sleep(Duration::from_millis(2));
    }
    // run with cancellation already set performs explicit shutdown without new admissions.
    host.run(&CANCELLED)
}
fn main() {
    if let Err(error) = run() {
        eprintln!("modal host refused/stopped: {error}");
        std::process::exit(1);
    }
}
