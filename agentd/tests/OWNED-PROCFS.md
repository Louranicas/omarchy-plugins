# Provider-free captured procfs integration

`cargo test -p agentd --test owned_procfs --offline --locked` runs three default Linux tests without a provider, a systemd installation, or a configured tmux server. Prerequisites are readable ordinary procfs metadata, the invoking user's readable process ancestry, and `/bin/sleep`.

Four owned sleep children use synthetic `codex`/`claude` executable names. The fixture captures stat/status/cwd plus the ancestry required by Agentd's collapse algorithm and the boot-time input. It never captures command arguments, environment contents, terminal screens or provider data. On machines already running under Codex/Claude ancestry, those ancestors legitimately collapse matching children. Expected roots are resolved independently through the live scanner and compared with captured replay and the actual daemon's `list --json` CLI output. Exited owned identities must disappear; a still-live ancestor is not expected to disappear with its children.

The daemon has a cleared environment and private runtime/state directories. Its PATH points to an absent private directory to disable tmux probing. Real SIGTERM must exit successfully and remove its private socket. Fixture children reserve reaper custody before spawning, are retained before assertions, and are killed and transferred to the existing bounded reaper on unwind. Normal test completion additionally waits for reaping within three seconds. No orphaning, user service installation, hooks or live configuration changes are involved.

The other tests modify only captured copies: foreign effective UID cannot become an agent, a malformed previously observed stat becomes explicitly unknown, and a new start-time identity cannot inherit activity or accept a stale activity command. These are replay faults, not claims of forcing real PID reuse.

Individual readiness, CLI, exit and cleanup operations have finite three-second budgets, with 1 MiB CLI output bounds; ordinary filesystem syscalls retain their kernel/filesystem scheduling limitations. Temporary metadata is removed by the fixture. A machine with inaccessible required ancestry fails the test rather than silently qualifying incomplete ancestry.

This is actual Linux-process and daemon integration with synthetic vendor labels. It does **not** replace `captured_procfs.rs`'s separately ignored real authenticated-provider smoke, validate real harness hook activation/trust, or establish service deployment, tmux acceptance, syscall privacy completeness or a release gate.
