//! Worker for the existing sidebar; no alternate owner or native dispatch path.
use crate::input::DisplayedCommand;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use yoohoo_runtime::{Command, Response, presenter::Link};
pub fn run(
    commands: mpsc::Receiver<DisplayedCommand>,
    updates: mpsc::SyncSender<Result<Response, String>>,
    stop: Arc<AtomicBool>,
    parent: Arc<modal_runtime::readiness::ParentWatch>,
    mapped: mpsc::Receiver<()>,
) {
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let ready = std::env::var("OMARCHY_MODAL_READY")?;
        let auth = modal_client::presenter::auth_from_ready(&ready)?;
        if auth.namespace() != std::env::var("OMARCHY_MODAL_NAMESPACE")? {
            return Err("namespace mismatch".into());
        }
        let socket = PathBuf::from(std::env::var_os("OMARCHY_MODAL_SOCKET").ok_or("ready socket")?);
        let parent_pid = std::env::var("OMARCHY_MODAL_PARENT_PID")?.parse()?;
        let data = PathBuf::from(
            std::env::var_os("OMARCHY_MODAL_CONTROLLER_SOCKET").ok_or("controller socket")?,
        );
        let controller = desktop_io::ProcessIdentity {
            pid: std::env::var("OMARCHY_MODAL_CONTROLLER_PID")?.parse()?,
            start_ticks: std::env::var("OMARCHY_MODAL_CONTROLLER_START_TICKS")?.parse()?,
        };
        mapped.recv_timeout(Duration::from_secs(2))?;
        parent.check()?;
        modal_runtime::readiness::report_mapped(
            &socket,
            parent_pid,
            &ready,
            Instant::now() + Duration::from_millis(500),
        )?;
        let mut link = Link::connect(&data, controller, auth)?;
        let mut id = 0u64;
        let mut model = link.model()?;
        while !stop.load(Ordering::Acquire) {
            parent.check()?;
            id = id.checked_add(1).ok_or("counter exhausted")?;
            let note = model
                .metadata_truncated
                .then(|| "Long display metadata shortened; identity remains opaque".to_owned());
            updates.try_send(Ok(Response {
                request_id: id,
                ok: true,
                error: note,
                view: model.view.clone(),
            }))?;
            match commands.recv_timeout(Duration::from_millis(40)) {
                Ok(command) => {
                    parent.check()?;
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    if !command.matches(&model.view) {
                        return Err("stale displayed model".into());
                    }
                    if !matches!(command.command, Command::List) && link.apply(command.command)? {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            model = link.model()?;
        }
        if !stop.load(Ordering::Acquire) && parent.check().is_ok() {
            let _ = link.apply(Command::Close {
                generation: model.view.generation,
            });
        }
        Ok(())
    })();
    let _ = updates.try_send(Err(if result.is_err() {
        "Managed controller unavailable; authority revoked".into()
    } else {
        "Managed session closed".into()
    }));
}
