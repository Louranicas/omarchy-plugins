# Resource exhaustion behavior

The 64-client and 32-subscriber limits are admission ceilings, not promises that every host can allocate that many descriptors or threads. Ordinary excess clients are refused without an additional worker. Requests and writes retain their existing finite deadlines.

If accepting a socket, cloning its cancellation handle, or spawning a client worker fails, the daemon takes its existing error path: it stops admission, shuts down retained client sockets, closes subscriptions, joins workers and the scanner, and removes its own still-matching socket endpoint. The command exits unsuccessfully with a diagnostic. It does not spin retrying an exhausted allocation or turn that failure into a successful daemon return.

The shipped user unit requests `Restart=on-failure`. Actual supervisor timing, rate limiting and successful service recovery depend on the installed environment; the isolated tests do not qualify systemd restart behavior. There is no new in-process recovery/backoff policy. Changing that policy would need an explicit availability contract and tests for bounded attempts, existing subscriber behavior, allocation headroom, fairness and shutdown while retrying.

`descriptor_exhaustion_fails_closed_then_fresh_process_restarts` lowers only a spawned fixture daemon's `RLIMIT_NOFILE` to 96. It first witnesses a real snapshot subscription, then applies bounded incomplete-request pressure, checks unsuccessful EMFILE exit and socket/subscriber closure, preserves an unrelated file, and manually starts a replacement in the same private runtime. The replacement must have a new instance ID. Parent resource limits are checked unchanged. Normal admission-ceiling tests remain separate.

This tests one actual Linux descriptor-exhaustion path, not all memory/thread/kernel failures. It does not install or restart a host service, enlarge limits, run providers, or prove the historical CI failure was caused by resource exhaustion. Filesystem/kernel scheduling can exceed userspace waiting budgets.

The pressure fixture uses one-shot nonblocking Unix connects so a full listener backlog cannot trap the test in connect. Its finite attempt count and absolute pressure deadline remain separate from admission evidence. Subscriber closure is an explicit EOF/reset outcome after draining the existing buffered reader under one deadline and byte/frame budgets; a complete queued update while the peer stays open cannot qualify closure. Tests exercise buffered updates, a live peer, exhausted drain budgets and an actually saturated listener backlog.
