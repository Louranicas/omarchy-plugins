# Explicit fresh-authority integration recipe

The `authority_join` cases in `tests/reconnect.rs` exercise the existing interfaces over real private Unix source, Host and presenter-data connections. Presenter creation/dismissal and compositor focus/readback are simulated. No controller executable behavior changes, actual GTK child, physical input, live service or automatic reacquisition is introduced.

The sequence is intentionally explicit:

1. Bind/run a trusted Host with the Yoohoo policy and authoritative target registry. Construct Sources using an explicitly discovered compositor identity and the Runtime clock origin. No modal client belongs to Sources.
2. After source readiness and attention observation, call take_ready to consume and transfer Native/Runtime/origin. Bind the data endpoint, connect a new modal Client, acquire a new lease and derive presenter Auth from that grant. Service::attach loads the new registry view and enters Yoohoo; opening alone emits no focus.
3. On source EOF, a real service request observes failure and closes its connection/release path. Join/drop that entire service before binding another data endpoint. The old Link remains poisoned. Shutdown is not proof of a successful uncertain effect.
4. Explicit launcher policy may construct another Sources from the trusted configuration. The tests exercise another actual EOF/backoff/resnapshot cycle there; previous attention is absent until a fresh event arrives. take_ready exposes no selection or pending activation. A new Client/acquire supplies a different full fence and presenter namespace; Service::attach obtains new targets. No old source, target view, selection command, lease or uncertain intent is blindly retried.

The tests independently challenge old target View against the fresh client, an old authenticated presenter envelope carrying a fully current valid selection, and current presenter Auth carrying the old captured selection. Requests decode correctly; malformed input is not the refusal oracle. A fresh model activation succeeds exactly once. The test selection IDs/revisions intentionally differ after the new observation epoch. Raw Command counters are session-local and are not globally unique capabilities; the full authenticated presenter envelope is required across controller/process lifetimes.

In the uncertainty case the simulated backend records one possible effect then returns EffectUnconfirmed. Link/Service close, a second attempt fails and the unavailable Host cannot issue a new lease. Backend count remains one; there is no blind replay or automatic new Host creation. This demonstrates protocol behavior for explicit backend uncertainty, not actual compositor readback or lost-reply injection.

Owned source/Host/service threads have stop-on-drop custody, bounded socket operations and finite service lifetime; teardown joins the service while Host is still available, then stops Host/source workers. Assertions do not detach a worker. These are userspace polling/I/O bounds, not cancellable filesystem/kernel scheduling or hard realtime guarantees.

Four end-to-end cases extend partial Y-014 evidence. They do not close production startup, real presenter lifecycle, compositor replacement discovery, automatic controller orchestration or release gates. No source recovery policy grants authority by itself.
