# Provider and input integration checkpoint

This checkpoint continues implementation from the published deep-hardening baseline. It does not qualify the four-plugin suite for production deployment. The canonical completion register retains the required acceptance scenarios; the seven post-completion review generations have not started.

## Shared input transport

`modal-runtime::socket::Connection::read_ready` exposes a nonconsuming, zero-timeout readiness probe. Live idle returns false; input and closure/error readiness return true so a caller can resolve them through the existing bounded frame reader. A readiness result neither authenticates current process identity nor promises a complete frame. Existing frame deadlines and sticky failure remain authoritative.

The focused package gate passed 75 library tests and strict all-target Clippy. Independent external tests checked repeated probes, two queued frames followed by EOF, an expired deadline despite ready data, and failed-write poisoning. This enables the experimental input receiver to remain idle without treating silence as a key release.

## Ask

[Explicit provider profiles](../ask-native/PROVIDER-PROFILES.md) define exact adapter arguments, harness executable, project directory, optional model/reasoning and selected environment names. Private profile reads reject symlink traversal, hardlinks, public file permissions and observed file changes. Check-only mode launches nothing and explicitly leaves authentication and environment availability unchecked. Diagnostics redact arguments, paths and environment values.

Independent private-GTK testing found an initial implementation bug: the adapter could start before the modal host granted activation. The corrected configured-provider path waits for the matching authenticated active acknowledgement, rechecks the observed launch inputs at actual launch, and refuses late admission after stop or timeout. Existing interactive permission decisions remain unchanged; no durable automatic approval is added.

Five independent private-GTK scenarios passed on stable source and binary: denied acknowledgement, wrong full-fence acknowledgement, parent loss before acknowledgement, changed profile while pending, and a matching acknowledgement. The first four launch nothing; the positive case launches a local sentinel only after admission. Profile, Worker and native tests passed, including four compiled semantic mutation controls. No real credentials or paid provider were used.

Observed replacement checks are not descriptor-bound execution: the residual path-to-exec race remains documented. Authentication, real vendor/session acceptance, installed launcher/keybinding provisioning and durable policy remain incomplete.

## Vimarchy

The new [experimental input bridge](../vimarchy-runtime/INPUT-BRIDGE.md) binds frames to the full session, presenter, bridge epoch, source revision and exact admitted device/code. It assigns press IDs, refuses replay and stale input, suppresses release of a preheld key, and permanently revokes invalid sessions. Returned effects remain intents subject to the existing Host checks.

Development review reproduced and corrected two defects before publication: a pre-expiry event arriving after lease expiry, and a presenter namespace inconsistent with the session context. The package passed 22 tests, formatting and strict Clippy; three compiled semantic mutations were detected.

Separately, four isolated real-Hyprland diagnostic runs demonstrated matched press/release, preheld-release suppression, later cancellation and reentrant invalidation. A compiled mutation defeating preheld protection failed the same oracle. These runs used a synthetic virtual keyboard and diagnostic counters. At that initial checkpoint the compositor diagnostic and Rust receiver were not connected; the later isolated join is described below. This is not production input provenance, live desktop activation, or hardware attestation.

## Agentd

The new [owned-process integration tests](../agentd/tests/OWNED-PROCFS.md) capture ordinary Linux process metadata, replay it through the scanner, compare it with the actual isolated daemon's CLI output, and require witnessed owned identities to disappear after exit. A real SIGTERM must remove the private daemon socket. Synthetic vendor process names are explicit; no authenticated provider is launched.

Review tightened the disappearance assertion so an environment where every fixture process collapses into a live ancestor cannot pass vacuously. Replay faults also cover foreign UID, malformed process metadata and start-time identity replacement. Existing real-vendor acceptance remains separately ignored and incomplete.

## Combined verification

The integrated source passed `tools/check.sh --native --offline`: 12 commands, 554 passing test executions, zero failures and two ignored entries, with formatting and strict Clippy clean. Tracked non-Markdown inputs stayed unchanged. Counts overlap across workspaces. Four standard compiled semantic controls also passed their baselines and detected their intended mutations. Independent lane checks are bounded as described above; the pending authenticated vendor fixture is not reclassified.

## Agentd CI fixture correction

The first hosted run at `4ea0ff7` failed the existing flood test because it observed no admitted clients. A client connecting to a kernel socket backlog does not prove daemon admission. Local controls reproduced early observation before worker creation; a separate low-file-descriptor control demonstrated that the former fixture could also conceal an actual daemon exit. The exact mechanism of the historical hosted failure remains unproven.

The corrective change is confined to the integration-test harness. It retains child cleanup custody before startup assertions, captures private bounded diagnostic reads, verifies daemon liveness and an actual startup response, and waits for a named admitted worker before admission-dependent assertions. Startup reads share an absolute deadline. Flood limits and production timeouts are unchanged. Shutdown is checked with a freshly admitted incomplete request and must close without a normal request-expiry error frame. Regression cases cover delayed admission, assertion/startup cleanup, and byte-drip deadlines; delayed resume uses a retained pidfd.

The package gate passed 88 tests with one ignored entry. Independent review passed all 18 integration tests. Three compiled semantic controls across owner and independent checks detected missing shutdown cancellation, unconditional admission, and removed worker-cap enforcement. The final combined local gate passed 535 executions with two ignored entries. These controls do not establish a global mutation score. The low-file-descriptor production behavior remains a separate follow-up, and hosted CI requires its own successful run.

## Isolated input join and resource-pressure follow-up

The ABI-pinned compositor fixture now feeds the actual Rust Ingress, Gate and Session through a private bounded socket, ending at counted intents. It does not dispatch Host effects. Eight isolated native scenarios passed; independent review rebuilt the observer and repeated matched, preheld and later-cancel scenarios. The join exposed two timing defects: receipt-time consumption overtook queued source events, and repeatedly sampling the clock offset could reverse equal source timestamps. The correction separates source gesture time from receipt authority time and anchors conversion once on Ready. Arrival expiry, source ordering, future rejection and sticky invalidation remain enforced. Tests cover queued edges, intervening timer/host events, deadline preservation and suspend divergence. Callback-export time is not hardware event time. Synthetic admission, event-loop scheduling and production Host integration remain unqualified. See [fixture scope](../tools/native-input-join/README.md).

Agentd production behavior is unchanged. New [resource-exhaustion tests](../agentd/RESOURCE-EXHAUSTION.md) apply a child-only descriptor limit, witness an admitted subscriber, require terminal failure and socket cleanup, drain buffered data to explicit EOF/reset within fixed budgets, and demonstrate a manual fresh-instance restart. Nonblocking pressure connections cannot wait indefinitely on a full backlog. The package passed 91 tests with one existing ignored entry; independent review passed all 21 integration tests, extra deadline probes and a compiled buffered-data bypass control. Manual restart does not qualify systemd recovery.

The final combined local gate passed 544 overlapping test executions, zero failures and two ignored entries, with 191 tracked non-Markdown inputs stable. The canonical candidate separately passed 587 executions, zero failures and five ignored entries with formatting, strict Clippy and compilation clean. These totals overlap and are not additive. Owner and independent semantic controls are scoped tests, not a global mutation score. Hosted CI remains a separate gate.

## Configured recovery and source reconnection

Ask adds [synthetic configured-adapter recovery tests](../ask-runtime/RECOVERY.md): cooperative cancellation rejects late permissions/text, ignored cancellation and TERM escalate to actual reap, an unrelated worker remains usable, and a fresh worker refuses an old instance's permission even when generation and wire identifiers repeat. Independent review reproduced cleanup on panic and stale-instance rejection. This changes tests and documentation only; real vendor acceptance and managed UI restart remain open.

Yoohoo adds a [nonmodal source supervisor](../yoohoo-runtime/RECONNECT.md) with bounded retry/backoff/jitter, retained endpoint identity, cleared selection/pending activation and fresh observation snapshots. Explicit transfer provides source state only; callers must acquire new modal authority. Independent review reproduced a late idle poll being accepted after its deadline. The correction checks completion and final transfer deadlines; the original delayed-poll reproduction now refuses transfer. The package passed 44 tests; independent backlog and EOF controls passed. No controller/UI reconnection or production modal recovery is claimed.

The final combined gate passed 554 overlapping executions, zero failures and two ignored entries; 195 non-Markdown inputs remained stable. The canonical candidate separately passed 597 executions, zero failures and five ignored entries, with formatting, strict Clippy and compilation clean. Hosted portable CI is checked separately. Prior failures and receipts remain historical evidence; no release gates advance from these partial scenarios.

## Remaining integration order

1. Qualify configured Ask sessions with real supported adapters and explicit authentication, cancellation, model and recovery scenarios; finish installed launcher/profile provisioning.
2. Extend the isolated counted-intent join to qualified production admission, event-loop scheduling and Host authority; complete hardware, device, modifier and lifecycle acceptance.
3. Complete real-provider, native lifecycle, accessibility and recovery acceptance with current source-bound evidence.
4. Complete transactional deployment and installed-state readback before production qualification.

Public portable CI remains intentionally narrower than the local Arch/native gate. Its preview and standalone GTK exclusions must not be represented as full native acceptance. No live desktop configuration, service installation, credential transfer or paid provider execution is part of this checkpoint.
