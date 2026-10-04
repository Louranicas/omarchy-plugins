# Isolated producer join fixture

This C++ plugin is ABI-pinned test code, not an installable input adapter. It may only be loaded in the disposable nested compositor. It takes one private, trusted harness admission at `/sandbox/join/admission`; this is not a production configuration format. The Rust `input-join-receiver` example uses synthetic readiness/window data and counts reducer intents without contacting Host or dispatching an effect.

One admitted keyboard, raw code 1, one fixed hint. Owned press/release binding pointers, one-use raw/global correlation, a bounded initial census and sticky lifecycle failure constrain export. Initial-held release is a one-use baseline transition, not a tap. Missing matched callback poisons at the next relevant edge or event-loop idle. Reload, keymap, device loss/census change, reentrancy and transport failure revoke. An arbitrary direct dispatcher invocation is not accepted as input.

Frames use fresh receiver-generated epoch/context and strictly increasing sequence with CLOCK_BOOTTIME timestamps. The sender authenticates receiver UID/PID/start and executable inode, retaining pidfd/proc/executable handles. This does not detect same-inode exec or a compromised same-UID process/compositor/plugin, and is not human-input attestation. Admission file/path and prefix are trusted fixture artifacts, not validated public input.

There is no queue: a single nonblocking send must deliver the complete bounded frame. Backpressure, interruption, partial send, peer exit or closed stream poisons and closes without retry/reconnect. A successful send is not proof of receiver acceptance. `transport-test.cpp` checks actual socket-buffer exhaustion, sticky failure, peer close and wrong start identity. Producer peer loss is checked before/after sends; no claim of idle-period immediate notification.

Native harness and source-bound results live outside this repository in the review evidence. No production build/install target includes this plugin. Remaining integration blockers and time semantics must be read before considering reuse. No native/hardware release gate advances from these tests.
