use std::{path::Path, time::Duration};
use yoohoo_runtime::{
    Request, Runtime,
    ipc::{Server, call},
};
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    match args.get(1).map(String::as_str){Some("fixture")if args.len()==4=>{let lifetime:u64=args[3].parse()?;let runtime=Runtime::fixture().map_err(|_|"fixture initialization failed")?;let mut server=Server::bind(Path::new(&args[2]),runtime)?;println!("fixture_ready pid={} source=fixture effects=pending",std::process::id());server.serve_until(Duration::from_millis(lifetime))?;},Some("control")if args.len()==5=>{let request:Request=serde_json::from_str(&args[4])?;let response=call(Path::new(&args[2]),args[3].parse()?,request)?;println!("{}",serde_json::to_string(&response)?);},_=>return Err("usage: yoohoo-runtime fixture PRIVATE_DIR LIFETIME_MS | control SOCKET DAEMON_PID REQUEST_JSON".into())}
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("yoohoo-runtime: {e}");
        std::process::exit(1);
    }
}
