# Explicit provider profiles (bounded implementation)

`ask-native --provider-config /absolute/private/profile.json` connects the existing interactive Ask session UI to an explicitly configured ACP adapter. Production presentation requires the existing authenticated modal parent/layer handshake and lifetime watch. The configured worker does not spawn its adapter until the matching active acknowledgment reaches the live mapped UI. Close or parent loss before acknowledgment cancels launch; the worker rechecks preflight immediately before spawning. Admission waits are bounded to five seconds. The UI shows the project directory, provider name, authentication uncertainty, and interactive-only permission policy. No durable automatic approval is enabled. `--fixture-ui` remains an isolated-display test mode with its existing automatic timeout; configured sessions have no fixture timeout.

The profile is versioned JSON, with duplicate/unknown keys rejected:

```json
{
  "version": 1,
  "harness": "codex",
  "adapter_argv": ["/absolute/path/to/installed-acp-adapter"],
  "harness_executable": "/absolute/path/to/installed-codex",
  "cwd": "/absolute/project",
  "model": "explicit-provider-model-id",
  "inherit_env": ["HOME", "PATH"]
}
```

`harness` supports `codex` and `claude`. `model` and `reasoning` are optional ACP configuration IDs; unsupported requested settings fail through the existing runtime handshake. `adapter_argv` is an exact array, never a shell expression. An adapter needing arguments receives separate array elements. No bundled adapter, package download, shell evaluation, implicit current directory or home-directory fallback occurs. The selected harness path is injected as `CODEX_PATH` or `CLAUDE_CODE_EXECUTABLE`. This is a provider hint to the chosen adapter, not proof of what the adapter eventually executes.

Profiles must be absolute paths, opened component-by-component without symlinks, regular files owned by the current user, singly linked, inaccessible to group/other, and at most 64KiB. Concurrent changes observed during reading are rejected. Do not store credentials in arguments or profiles. The environment begins empty: only explicitly requested `HOME`, `PATH`, `LANG`, `LC_ALL`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `CODEX_HOME`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `ANTHROPIC_AUTH_TOKEN` may be inherited. Missing requested variables reject launch. Forwarded values are bounded and are never included in profile diagnostics. An explicitly authorized provider can read its own ordinary files and credentials after launch; this feature is not a provider sandbox.

`ask-native --check-provider /absolute/private/profile.json` reads configuration and checks filesystem availability without launching a child or inspecting environment values or credential files. Its redacted JSON explicitly reports `authentication: not_checked` and `environment_availability: not_checked`. A successful check does not establish authentication, ACP compatibility, model availability or a usable provider session.

Preflight records executable and directory identity/metadata and rechecks them plus the private profile before starting the worker. Observed replacement is rejected. **This does not close the path-to-exec race or pin executable/cwd identity through spawn.** Paths may intentionally be user-writable, executable symlinks are supported, and an adapter may spawn another program. No executable attestation or protection against a concurrent same-user writer is claimed. The session's arguments/model/project are frozen; later profile edits cannot silently reconfigure a running session.

## Evidence and limits

Tests use synthetic values and a local ACP fixture, including literal metacharacter argv, cwd, environment isolation, requested model handshake, private-file/alias checks, replaced profile/executable/directory rejection, real CLI redaction and unmanaged-launch refusal. No live provider, paid model, real credentials, installed desktop service, or host configuration was used. This adds partial A006–A010 and A-X01 behavior; it does not complete environment-variable legacy precedence, provider auto-discovery, model-selection UI, multi-session/restart UI, authentication management, persisted policy or installed native acceptance. Existing source contracts and other release gates remain authoritative.
