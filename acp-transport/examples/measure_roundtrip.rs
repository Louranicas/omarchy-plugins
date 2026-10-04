use acp_transport::{Launch, Transport};
use std::{
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
fn main() {
    let mut t = Transport::spawn(&Launch {
        executable: "/usr/bin/python3".into(),
        arguments: vec![
            "-u".into(),
            "-c".into(),
            "import sys\nfor line in sys.stdin: print(line.strip(),flush=True)".into(),
        ],
        directory: "/".into(),
        environment: vec![],
    })
    .unwrap();
    let payload = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"bench","params":{"text":"x".repeat(1024)}});
    let cancel = AtomicBool::new(false);
    let mut samples = vec![];
    for n in 0..210 {
        let start = Instant::now();
        t.send(&payload, Duration::from_secs(1), &cancel).unwrap();
        assert_eq!(t.receive(Duration::from_secs(1), &cancel).unwrap(), payload);
        if n >= 10 {
            samples.push(start.elapsed().as_micros() as u64)
        }
    }
    samples.sort_unstable();
    println!(
        "{}",
        serde_json::json!({"scenario":"local Python stdio echo; 1024-byte text; 10 warmups; debug build","samples":200,"p50_us":samples[99],"p95_us":samples[189],"p99_us":samples[197],"max_us":samples[199]})
    );
}
