//! Explicit daemon/control entrypoint. Host deployment remains a separate task.
use desktop_io::ProcessIdentity;
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use yoohoo_runtime::{
    controller::{Controller, Operation, Phase, Reply, Request, Startup},
    reconnect::Policy,
};
fn startup() -> Result<Startup, Box<dyn std::error::Error>> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(0, st.as_mut_ptr()) } != 0
        || unsafe { st.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFIFO
    {
        return Err("startup requires launcher-owned pipe".into());
    }
    let end = Instant::now() + Duration::from_secs(3);
    let mut bytes = Vec::new();
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("startup deadline".into());
        }
        let mut fd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut fd, 1, left.as_millis().max(1) as i32) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(io::Error::last_os_error().into());
        }
        if n == 0 {
            return Err("startup deadline".into());
        }
        let mut b = 0u8;
        if unsafe { libc::read(0, (&mut b as *mut u8).cast(), 1) } != 1 {
            return Err("startup closed".into());
        }
        if Instant::now() >= end {
            return Err("startup deadline".into());
        }
        if b == b'\n' {
            break;
        }
        if bytes.len() == 4096 {
            return Err("startup limit".into());
        }
        bytes.push(b);
    }
    let config: Startup = serde_json::from_slice(&bytes)?;
    config.validate()?;
    Ok(config)
}
fn exchange(
    path: &Path,
    id: ProcessIdentity,
    request: &Request,
) -> Result<Reply, Box<dyn std::error::Error>> {
    let mut client = modal_client::data::Client::connect(path, id)?;
    let bytes = client.exchange(
        &serde_json::to_vec(request)?,
        Instant::now() + Duration::from_secs(3),
    )?;
    let reply: Reply = serde_json::from_slice(&bytes)?;
    if reply.version != 1 {
        return Err("invalid controller reply".into());
    }
    Ok(reply)
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    let mode = args
        .get(1)
        .and_then(|s| s.to_str())
        .ok_or("mode required")?;
    if mode == "serve" && args.len() == 4 {
        let data = PathBuf::from(&args[2]);
        let control = PathBuf::from(&args[3]);
        use std::os::unix::fs::MetadataExt;
        let d = std::fs::metadata(&data)?;
        let c = std::fs::metadata(&control)?;
        if (d.dev(), d.ino()) == (c.dev(), c.ino()) {
            return Err("distinct data/control directories required".into());
        }
        let socket = modal_runtime::socket::ControlSocket::bind(&control)?;
        println!("controller_ready=true pid={}", std::process::id());
        io::stdout().flush()?;
        let mut daemon = Controller::new(startup()?, &data, Policy::default())?;
        loop {
            daemon.serve_step(&socket)?;
            if daemon.status().phase == Phase::Stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    } else if matches!(mode, "status" | "open" | "quit") && args.len() == 5 {
        let path = PathBuf::from(&args[2]);
        let id = ProcessIdentity {
            pid: args[3].to_str().ok_or("PID")?.parse()?,
            start_ticks: args[4].to_str().ok_or("start ticks")?.parse()?,
        };
        let mut reply = exchange(
            &path,
            id,
            &Request {
                version: 1,
                instance: None,
                revision: 0,
                operation: Operation::Status,
            },
        )?;
        if mode != "status" {
            reply = exchange(
                &path,
                id,
                &Request {
                    version: 1,
                    instance: Some(reply.instance),
                    revision: reply.revision,
                    operation: if mode == "open" {
                        Operation::Open
                    } else {
                        Operation::Quit
                    },
                },
            )?;
        }
        println!("{}", serde_json::to_string(&reply)?);
        if !reply.accepted {
            return Err("request refused; inspect status, no automatic retry".into());
        }
    } else {
        return Err("usage: yoohood serve DATA_ROOT CONTROL_ROOT < trusted-startup-pipe | status|open|quit CONTROL_SOCKET PID START_TICKS".into());
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("yoohood: {error}");
        std::process::exit(1);
    }
}
