//! Controlled fixture controller. Startup configuration comes only from the
//! launcher-owned stdin pipe; no presenter or modal wire supplies endpoints.
use desktop_io::{Endpoint, ProcessIdentity};
use serde::Deserialize;
use std::{
    io::{Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
};
use vimarchy_runtime::{native::Native, service::Service};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Startup {
    modal_root: PathBuf,
    settings: Option<PathBuf>,
    native_root: PathBuf,
    native_instance: String,
    host_pid: u32,
    host_start_ticks: u64,
    native_pid: u32,
    native_start_ticks: u64,
}
fn startup() -> Result<Startup, Box<dyn std::error::Error>> {
    // The fixture launcher owns this pipe. Do not accept an interactive terminal
    // or indefinitely block on an unfinished/malicious startup frame.
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(0, stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if unsafe { stat.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFIFO {
        return Err("startup requires a launcher-owned stdin pipe".into());
    }
    let end = Instant::now() + Duration::from_secs(3);
    let mut bytes = Vec::new();
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("startup deadline".into());
        }
        let mut fd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut fd, 1, remaining.as_millis().max(1) as i32) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if result == 0 {
            return Err("startup deadline".into());
        }
        let mut byte = [0];
        if unsafe { libc::read(0, byte.as_mut_ptr().cast(), 1) } != 1 {
            return Err("startup closed".into());
        }
        if byte[0] == b'\n' {
            break;
        }
        if bytes.len() == 4096 {
            return Err("startup frame limit".into());
        }
        bytes.push(byte[0]);
    }
    let config: Startup = serde_json::from_slice(&bytes)?;
    if !config.modal_root.is_absolute()
        || !config.native_root.is_absolute()
        || config.native_instance.len() > 128
        || config.host_pid == 0
        || config.host_pid > i32::MAX as u32
        || config.native_pid == 0
        || config.native_pid > i32::MAX as u32
        || config.host_start_ticks == 0
        || config.native_start_ticks == 0
    {
        return Err("invalid startup configuration".into());
    }
    Ok(config)
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("OMARCHY_TEST_ISOLATED_DISPLAY").as_deref() != Ok("yes") {
        return Err("isolated fixture required".into());
    }
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("data root required")?;
    if !root.is_absolute() {
        return Err("absolute data root required".into());
    }
    let listener = Service::bind(&root)?;
    println!("controller_ready=true pid={}", std::process::id());
    std::io::stdout().flush()?;
    let config = startup()?;
    let policy = match config.settings.as_ref() {
        Some(path) => vimarchy_runtime::policy::Policy::load(path)?,
        None => vimarchy_runtime::policy::Policy::default(),
    };
    let endpoint = Endpoint::discover_identity(
        &config.native_root,
        &config.native_instance,
        ProcessIdentity {
            pid: config.native_pid,
            start_ticks: config.native_start_ticks,
        },
    )?;
    let mut epoch = [0; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut epoch)?;
    let mut native = Native::connect(endpoint, epoch).map_err(|e| format!("native {e:?}"))?;
    let observation = native.observe().map_err(|e| format!("observe {e:?}"))?;
    let mut client = modal_client::Client::connect(
        &config.modal_root,
        ProcessIdentity {
            pid: config.host_pid,
            start_ticks: config.host_start_ticks,
        },
    )
    .map_err(|e| format!("connect {e:?}"))?;
    let lease = client
        .acquire(Duration::from_secs(5))
        .map_err(|e| format!("acquire {e:?}"))?;
    let presenter = lease.presenter.ok_or("missing presenter identity")?;
    println!(
        "controller_presenter_verified=true pid={} namespace={}",
        presenter.process().pid,
        presenter.namespace()
    );
    let mut service = Service::attach_with_policy(listener, client, native, observation, policy)?;
    let end = Instant::now() + Duration::from_secs(4);
    while !service.is_closed() && Instant::now() < end {
        service.step()?;
        std::thread::sleep(Duration::from_millis(1));
    }
    let completed = service.is_closed();
    service.close();
    println!("controller_finished=true service_closed={completed}");
    if !completed {
        return Err("fixture interaction deadline".into());
    }
    Ok(())
}
