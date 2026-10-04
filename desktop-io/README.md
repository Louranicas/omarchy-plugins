# Desktop I/O test prerequisites and scope

The executable-continuity regression launches its own Rust test executable and
then `/bin/sleep`, retaining a genuine socket across exec so an absent socket
cannot produce a false success. Python and Perl are not required by this crate's
tests. Fixtures use bounded private Unix readiness channels and hand killed
children to the existing finite process-runner reaper, including on assertion
unwind; tests check actual reap within their test budget. The ignored `peer_child`
entrypoint is subprocess-only and exercised by its three parent tests.

Tests require Linux procfs, pidfd support and same-user access to process
executable links. These are cooperative local fixture bounds, not hard real-time
guarantees for arbitrary filesystem scheduling or a sandbox. Runtime executable
continuity detects changed inodes, not same-inode exec or change-and-return
between observations. Initial process discovery still requires trusted inputs.
