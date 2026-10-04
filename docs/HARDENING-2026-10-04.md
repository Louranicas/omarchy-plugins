# Deep plugin hardening review — 2026-10-04

This review started from public commit `c9ce4a0`. Three independent agents reviewed critical paths in isolated clones and build directories, with root responsible for diff review, integration and the combined gate. It is a focused source and behavioral review, not exhaustive verification or a performance-percentile claim.

## Confirmed findings

| Component | Finding | Correction and evidence |
|---|---|---|
| Ask runtime | Closing or startup timeout could retain unsent prompts and selected permission approvals | Purge outgoing work on the first actual closing transition; preserve repeated-close shutdown effects and rejected-close state. Failing-before regression, actual subprocess protocol sentinel, and four semantic controls. Already transmitted effects cannot be recalled. |
| Yoohoo native source | Invalid active-window JSON could become trusted unfocused state | Require the documented object/address shape; invalidate uncertain state. Real Unix-socket tests cover malformed and valid focus snapshots and subsequent urgent events. |
| Desktop peer test | Blocking readiness could hang, and assertion failures could abandon the fixture child | Rust child fixture, bounded socket readiness and retained child cleanup on unwind. Deliberately stalled readiness and cleanup controls. |
| Yoohoo test fixtures | Failed assertions could abandon host workers; flood input leaked an allocation | Owned worker cleanup during unwind and an owned input buffer. Explicit unwind regression and compiled cleanup mutation. |
| Mutation runner | A normally exiting command could leave helper descendants running | Retain the leader PID with waitid until process-group cleanup, bound reap waits, reject lost custody and nondefault child handling. Independent spawned-helper test confirmed actual kill and reap. |
| Ask preview tests | Content tests and the public quota test raced on the same admission counter | Give content tests separate counters while retaining the unchanged global two-job public limit. Original combined-gate failure is preserved; deterministic saturated-quota proof establishes isolation. |

Vimarchy's reviewed gesture and reply-fence paths did not yield a new confirmed product defect in this pass. Existing native acceptance limits remain open. Agentd's real-harness qualification also remains pending.

## Quality gates and their limits

`tools/check.sh --native --offline` is the complete local formatting, all-target test and strict-Clippy gate for the root and three GTK workspaces. Commands have explicit timeouts. `tools/mutation-check.py` verifies four selected semantic controls with passing baselines, successful compilation, a named failing test and the expected behavioral assertion. A timeout, compiler failure or incidental fixture panic is not a successful mutation kill. Temporary source comes from tracked files and uses a separate target directory.

Hosted CI uses the explicit `--portable` subset: root tests except the Arch-dependent preview suite, workspace-wide lint, and selected mutations. Preview relies on the target Arch loader layout, Bubblewrap, ImageMagick 7 and Poppler. This exclusion is not an acceptance waiver: full local preview and native checks remain necessary. GTK unit tests are also distinct from actual installed compositor, IME and accessibility acceptance.

## Recommended order of work

1. **Require the recorded gates before promotion.** Treat hosted CI as partial. Establish a disposable Arch-based runner for complete decoder/native checks before calling CI a full release gate; do not attach untrusted pull-request jobs to the owner's live desktop.
2. **Finish real integration acceptance.** Prioritize Vimarchy's trusted production input adapter; Ask's actual provider startup, permission and shutdown behavior; Yoohoo's notification attribution, persistence and reconnect; and Agentd's captured real-harness/service checks. Keep ambiguous effect outcomes explicit and avoid automatic replay.
3. **Continue test reliability work at boundaries.** Replace remaining unbounded fake-server accept/read helpers as those tests are touched. Use owned processes and temporary data, avoid shared fixture binaries, and never repair a scheduling failure by retrying until green.
4. **Expand mutations by risk, not by score.** Add mutations for each new authority, lifecycle or recovery invariant. Keep positive controls and behavioral fingerprints; a raw test count or mutation percentage does not establish production correctness.

## Applied guidance

The local p-stack `tdd` and `principle-separate-before-serializing-shared-state` guidance informed failing-before regressions and isolated ownership. Boundary-discipline guidance was adapted: typed internal values reduce redundant validation, but process identity, permissions and compositor observations remain time-sensitive boundaries that need revalidation. No p-stack hooks or automation were installed.

CI structure follows [GitHub's Rust workflow guidance](https://docs.github.com/en/actions/tutorials/build-and-test-code/rust). No production service, host configuration, live provider or deployment gate was changed by this review.
