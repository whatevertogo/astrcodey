# Plugin services

Plugin services are versioned, unary JSON interfaces between extensions. Bundled extensions,
Rust workers and Python workers share dependency, authorization and invocation semantics.
They do not depend on one another's implementation crates or call management HTTP routes.

## Ownership and publication

`ExtensionManifest` owns dependencies and extra invoke permissions. `Registrar` binds service
keys to handlers; `ExtensionRegistrations` is the only declaration of provided services.
`ResolvedExtensionManifest` joins these validated declarations. `ExtensionRunner` owns
instances, tasks, failure observation and retirement. The service map in `HandlerIndex` is
an immutable derived lookup, not an independent lifecycle owner.

The loader computes the changed Required closure before choosing retained instances. Pure
`DependencyPlan` analysis resolves exact `name@major` keys, rejects ambiguous providers and
identifies cycle members separately from downstream blocked plugins. Optional edges grant
invoke permission but never determine startup or cascade stops. Missing/conflicting/cyclic
services produce inspectable blocked declarations without creating placeholder instances.
Malformed declarations and failed activation still abort candidate configuration preparation.

Providers activate before Required consumers. Activation means initialization has succeeded;
external admission also requires batch publication. Service calls during `start`/`on_activate`
are forbidden. Candidate services stay private until the existing configuration transaction
publishes the runtime snapshot; managed background tasks start after publication.

Calls pin a runtime snapshot. Old in-flight work may finish against its old provider, never a
replacement with the same ID. Retiring providers wait for their Required consumers' cleanup.
This preserves existing rolling-generation semantics: it does not promise that old and new
instances never overlap or that arbitrary plugin-owned file I/O can be forcibly cancelled.

A worker's terminal driver failure reports its instance identity to the runner. The runner
serializes withdrawal with source reconciliation, rejects stale failure reports, closes the
Required closure, publishes blocked diagnostics and transfers cleanup to the existing retirement
owner. The server publication callback advances the session's extension generation without
changing its configuration or models, then emits the existing registry-change notification.
Explicit reload/re-enable can recover the still-enabled dependency closure; there is no retry loop.

## Calls and authority

`host.services().invoke(key, input)` uses the existing `HostClientTransport` and the canonical
`astrcode.service.invoke` host operation. Effective permission is the union of dependency keys
and extra invoke keys. Permission does not propagate along dependency edges.

The dispatcher validates caller instance and key permission in its bound snapshot, checks the
trusted invocation chain, acquires provider admission and constructs a provider-owned context.
Provider identity, capabilities and private paths belong to the provider. Session/workspace
scope, cancellation and the resource lease come from the parent call. The tool plan must include
`HostResource::ExtensionService`; nested host operations remain subject to the same resource
lease. Native plugin private I/O is still a trusted-plugin responsibility, not an OS sandbox.

Caller attribution is never taken from business JSON. S5R nested calls resolve through the
host's active parent-invoke map; unknown parents cannot fall back to detached authority. Re-entry
into an instance already on the service chain fails immediately. Timeouts include admission.
Host errors retain code, message, hint, retryable and details across native/worker boundaries.

## Protocol and compatibility

S5R remains 3.0. A worker that declares services, dependencies or extra service permissions
requires `extension_services_v1`. Handshake manifests add `services`, `service_dependencies`
and `service_permissions`; empty fields are omitted for compatibility with strict older hosts.
`handler.invoke` uses `<extension>:service:<name>@<major>` and returns an `ok` effect carrying
JSON data. Service responses cannot carry continuations. Runtime context is carried separately
from business input. Rust and Python provide the same contract and background invocation entry.

## Management and first provider

Plugin management includes provided keys, dependencies, effective permissions and structured
blocked reasons. `enabled` remains user intent; `loaded` identifies actual runtime presence.
Blocked disk plugins retain their source and declaration. Public DTOs are mapped at the HTTP
boundary and TypeScript bindings are generated from Rust.

`astrcode.memory` provides `memory.entries.list@1` with optional `query` and `limit` and responds
with `{ "entries": [...] }`. It shares the existing listing/search implementation and limits with
`memory_list`. Entries retain the existing user/project labels. Workspace scope comes from the
host context; unscoped requests and extra input fields (including paths) are rejected. The global
user-preference store is intentionally shared while project memory stays workspace scoped.

## Extension boundary

The initial version has no dynamic publish/withdraw API, streaming services, service discovery
endpoint, persistent registry or automatic restart policy. Dynamic publication can later enter
through the instance owner and replace the derived snapshot. No speculative revision counters,
second configuration model or alternate lifecycle coordinator are needed today.
