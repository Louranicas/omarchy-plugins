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

Separately, four isolated real-Hyprland diagnostic runs demonstrated matched press/release, preheld-release suppression, later cancellation and reentrant invalidation. A compiled mutation defeating preheld protection failed the same oracle. These runs used a synthetic virtual keyboard and diagnostic counters. The compositor diagnostic and Rust receiver are not connected. This is not production input provenance, live desktop activation, or hardware attestation.

## Agentd

The new [owned-process integration tests](../agentd/tests/OWNED-PROCFS.md) capture ordinary Linux process metadata, replay it through the scanner, compare it with the actual isolated daemon's CLI output, and require witnessed owned identities to disappear after exit. A real SIGTERM must remove the private daemon socket. Synthetic vendor process names are explicit; no authenticated provider is launched.

Review tightened the disappearance assertion so an environment where every fixture process collapses into a live ancestor cannot pass vacuously. Replay faults also cover foreign UID, malformed process metadata and start-time identity replacement. Existing real-vendor acceptance remains separately ignored and incomplete.

## Combined verification

The integrated source passed `tools/check.sh --native --offline`: 12 commands, 532 passing test executions, zero failures and two ignored entries, with formatting and strict Clippy clean. Tracked non-Markdown inputs stayed unchanged. Counts overlap across workspaces. Four standard compiled semantic controls also passed their baselines and detected their intended mutations. Independent lane checks are bounded as described above; the pending authenticated vendor fixture is not reclassified.

## Remaining integration order

1. Qualify configured Ask sessions with real supported adapters and explicit authentication, cancellation, model and recovery scenarios; finish installed launcher/profile provisioning.
2. Connect the qualified compositor callback exporter to the experimental receiver in an isolated compositor, including queue backpressure, admission, clock mapping and teardown.
3. Complete real-provider, native lifecycle, accessibility and recovery acceptance with current source-bound evidence.
4. Complete transactional deployment and installed-state readback before production qualification.

Public portable CI remains intentionally narrower than the local Arch/native gate. Its preview and standalone GTK exclusions must not be represented as full native acceptance. No live desktop configuration, service installation, credential transfer or paid provider execution is part of this checkpoint.
