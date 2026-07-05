# mcp-crewd-rs — Bus Protocol Specification

**Spec version:** `0.1`
**Status:** Draft (Fase 0) — normative, to be implemented 1:1 in Fase 1.
**Binding inputs:** audit report `test-report/2026-07-03_AUDIT_idea_gpt55.md`
(findings F1, F3, F5, F9 — and F4 for the adapter contract); README charter.

This document is the normative protocol that the Fase 1 `crewd` daemon and the
per-cell MCP shim MUST implement. Type names, field names, state names, error
codes and tool names defined here are copied verbatim into code; changing them
is a spec-version bump (Section 16).

## Conventions

The key words **MUST**, **MUST NOT**, **REQUIRED**, **SHALL**, **SHALL NOT**,
**SHOULD**, **SHOULD NOT**, **RECOMMENDED**, **MAY**, and **OPTIONAL** in this
document are to be interpreted as described in RFC 2119.

Every normative statement in this document is written to be testable: a future
contract test MUST be able to assert it by observing the daemon's inputs,
outputs, persisted state, and audit events.

---

## 1. Scope & non-goals

`mcp-crewd-rs` (the "bus") is a local, single-host message bus that lets AI
cells (Claude Code, Codex, ACP-speaking agents, and legacy tmux cells)
exchange structured, audited messages through a central daemon (`crewd`)
reached over a Unix domain socket.

Identity, the trust boundary, and message trust are normatively defined in
Sections 17 and 18, and conformance requirements in Section 19. On those
subjects those sections prevail over any earlier wording in this document.

**In scope for spec version `0.1`:**

- A single `crewd` instance running on one host.
- Authenticated message provenance: the sending cell identity (`from_cell`) is
  derived by the daemon from the authenticated connection and is never taken
  from the message payload.
- Message kinds: `send`, `ask`, `reply`, `broadcast` (explicit grant, default-deny), `notice`.
- At-least-once delivery with mandatory consumer-side deduplication.
- Per-recipient delivery state, explicit acknowledge, retry with backoff, and
  lease-based claiming.
- An ask/reply ticket model with bounded wait, reply correlation, and basic
  deadlock prevention.
- A capability-based ACL (TOML), with default-deny for dangerous capabilities.
- An append-only audit log protected by a SHA-256 hash chain.
- Per-sender quotas and backpressure.
- A minimal operational CLI.

**Non-goals (explicitly NOT provided in version `0.1`):**

- **Network operation.** The bus operates on a single host over a Unix socket.
  Multi-host operation is out of scope.
- **Single-delivery guarantee.** The protocol provides **at-least-once**
  delivery. It explicitly does **not** guarantee that any given message is
  delivered precisely once: a message MAY be delivered more than once, and
  consumers MUST deduplicate by `message_id`. No component of the bus may
  claim or imply single-delivery semantics.
- **Tamper-proof provenance against a peer sharing the daemon's Unix UID.**
  Provenance is authenticated and daemon-derived, which raises the bar and
  makes spoofing detectable in the audit chain, but under the shared-UID
  deployment of version `0.1` it is not a forgery-proof guarantee. See
  `THREAT_MODEL.md` for the accepted posture and the upgrade path.
- **Guaranteed wake of an idle cell via the Stop hook.** The hook-based wake
  adapter is an experimental feature flag (`experimental_hook_wake`) and is not
  required for protocol correctness. The protocol is complete without it:
  persistent inbox plus end-of-turn delivery plus explicit `cell_inbox`
  polling.
- **Mid-turn injection of message bodies.** Version `0.1` MUST NOT deliver a
  message body into a cell mid-turn. Only a minimal `notice` signal (no task
  body) MAY be surfaced mid-turn; the body is always retrieved explicitly.
- **Broadcast open to all cells.** `broadcast` requires an explicit capability
  grant (default-deny) in version `0.1`; it is not implicitly tied to
  `admin_registry`.
- **Approval resolution by peer cells.** A bus message can never approve,
  deny, or influence the resolution of a tool permission approval. Approvals
  are resolvable only by a human or a trusted API client.

## 2. Terminology

- **Cell:** an addressed participant on the bus, identified by a registry name
  (e.g. `dev-senior`, `codex-audit`). A cell has an engine (Claude Code, Codex,
  ACP agent, tmux), an ACL grant set, and a delivery adapter.
- **Daemon (`crewd`):** the single central authority on the host that assigns
  identities, sequences, and state; enforces ACL and quotas; persists state;
  appends audit events; and drives delivery adapters. It is the only component
  permitted to assign `message_id`, `ask_id`, per-recipient `seq`, delivery
  state, and ACL decisions.
- **Shim (`crew mcp`):** the per-cell MCP server (stdio) that exposes the
  `cell_*` tool surface to the model. It authenticates to the daemon using the
  cell identity and token; it MUST NOT self-declare `from_cell` or delivery
  state.
- **Envelope:** the normative message record produced and signed-off by the
  daemon (Section 3). The envelope carries provenance, addressing, kind, and
  lifecycle metadata; it is distinct from the `body`.
- **Body:** the UTF-8 text payload of a message (max 64 KiB), delivered as
  untrusted data, never as authoritative instruction.
- **Delivery:** the act of an adapter handing an envelope to a recipient cell.
  Delivery is at-least-once; consumers MUST deduplicate.
- **Adapter:** a runtime-specific component that turns a daemon delivery
  request into an actual handoff to a cell engine (tmux, hook, appserver, acp).
- **Ask ticket:** the correlation record created for a `cell_ask`, identified
  by `ask_id`, holding at most one reply.
- **Taint:** the trust marking carried by every envelope. In version `0.1` the
  taint is the constant `peer_untrusted`: every delivered body is untrusted
  peer data.
- **Capability:** a granular permission in the ACL (Section 11), e.g. `send`,
  `ask`, `broadcast`.
- **Audit event:** an immutable record appended to the audit chain
  (Section 12), hash-linked to the previous event.

---

## 3. Envelope schema

Every message on the bus is represented as an **envelope**. The envelope is the
only normative message record. It is produced and validated by the daemon; the
sending cell supplies the addressing and the body, and the daemon fills in all
identity, ordering, and lifecycle metadata.

Normative JSON shape (field order is not significant; types are normative):

```json
{
  "spec_version": "0.1",
  "message_id": "019644a1-...-uuidv7",
  "ask_id": null,
  "from_cell": "dev-senior",
  "to_cell": "codex-audit",
  "kind": "send",
  "msg_type": "task",
  "principal_capabilities": ["send", "ask", "reply", "read_inbox", "list_cells", "read_audit"],
  "created_at": "2026-07-03T12:00:00Z",
  "expires_at": "2026-07-04T12:00:00Z",
  "seq": 42,
  "idempotency_key": "019644a2-...-uuidv7",
  "body": "Please review PR #12.",
  "file_refs": ["pr-12.diff"],
  "taint": "peer_untrusted",
  "reply_to": null
}
```

All identifiers (`message_id`, `ask_id`, `idempotency_key`) are UUIDv7 strings
(128-bit, time-ordered, lowercase 8-4-4-4-12 with hyphens). Timestamps are
RFC 3339 UTC (`Z`).

### 3.1 Field table

For each field the table states: **Set by** (who is the authority),
**Mutability** (whether it may change after enqueue), and **Limits / rules**.

| Field | Set by | Mutability | Limits / rules |
|---|---|---|---|
| `spec_version` | daemon (constant per daemon build) | immutable | String `MAJOR.MINOR`; version `0.1` for this spec. MUST match a version the daemon accepts (Section 16). |
| `message_id` | daemon-assigned | immutable | UUIDv7. REQUIRED on every envelope. The daemon is the sole assigner; a client-supplied value MUST be rejected with `E_INTERNAL`. |
| `ask_id` | daemon-assigned | immutable | UUIDv7 for `kind = ask` and the matching `reply`; `null` for `send`, `broadcast`, `notice`. |
| `from_cell` | daemon-derived | immutable | Registry name. Derived exclusively from the authenticated connection (token + peer credential). The daemon MUST ignore and reject any `from_cell` present in the client payload. |
| `to_cell` | client-supplied | immutable | Registry name of an existing cell, or `null` only when `kind = broadcast`. Unknown recipient → `E_UNKNOWN_CELL`. |
| `kind` | daemon-derived | immutable | The **transport** kind: one of `send`, `ask`, `reply`, `broadcast`, `notice` (Section 4). Derived from the invoking tool and correlation context, never client-supplied. Determines routing and delivery mechanics. |
| `msg_type` | client-supplied (validated) | immutable | The **semantic** type: one of `note`, `task`, `ask`, `reply`, `evidence`, `admin_request` (Section 4.1). Classifies the body's role for the recipient and the audit causal chain; the daemon validates it against the sender's capabilities. |
| `principal_capabilities` | daemon-derived | immutable | Array of capability strings (Section 11.1) the sender's `CellPrincipal` holds at enqueue (e.g. `["send","ask","read_audit"]`). Visible **to the recipient only**, so it can calibrate trust (e.g. weight an `admin_request`, or distrust a sender that claims `broadcast`). It asserts the sender's capabilities; it never grants any to the recipient. L0-honest information exposure, accepted under the same-UID posture (Section 17); this does not contradict `cell_list` (Section 5.6), which concerns registry listing, not per-message capability exposure. |
| `created_at` | daemon (daemon clock) | immutable | RFC 3339 UTC. Time of enqueue at the daemon. |
| `expires_at` | daemon-computed | immutable | RFC 3339 UTC = `created_at` + TTL. Default TTL: `ask` = 15 minutes, `send`/`reply`/`notice` = 24 hours, `broadcast` = 1 hour. A client MAY request a shorter TTL via the tool; a requested TTL longer than the daemon ceiling (the defaults above) MUST be clamped down to that ceiling, never rejected. |
| `seq` | daemon-assigned | immutable | `u64`, monotonically increasing per ordered pair `(from_cell, to_cell)`. For `broadcast`, assigned per-recipient during fan-out (Section 9). |
| `idempotency_key` | daemon-generated | immutable | UUIDv7 by default. A client MAY supply its own key on `cell_send`/`cell_ask`/`cell_broadcast` to make retries collapse (Section 8); a client-supplied key MUST be ≤ 128 bytes of UTF-8 (longer → `E_BODY_TOO_LARGE`) and MUST be retained verbatim. |
| `body` | client-supplied | immutable | UTF-8 text, max 64 KiB (`E_BODY_TOO_LARGE`). Not parsed as instruction; delivered as untrusted data. |
| `file_refs` | client-supplied | immutable | Array of path strings, each validated by the daemon. Version `0.1`: same-host only, restricted to repo-relative paths or a daemon-designated temp directory; symlinks that escape those roots MUST be rejected at validation. At delivery/read time each path MUST be resolved with `O_NOFOLLOW` on the final component (or opened by the daemon and handed to the recipient as an `fd`), so a same-UID attacker cannot swap the final component for a symlink after validation. Empty array if absent. |
| `taint` | daemon (constant in `0.1`) | immutable | The string `peer_untrusted`. Marks every body as untrusted peer data for the delivery preamble (Section 10). |
| `reply_to` | daemon-derived | immutable | `message_id` of the originating `ask` when `kind = reply`; `null` otherwise. |

### 3.2 Authority invariants

The following invariants are normative and individually testable:

- **I3.1** The daemon is the sole assigner of `message_id`, `ask_id`, `seq`,
  `created_at`, `expires_at`, and the constant `taint`. A shim that attempts to
  set any of them in the payload MUST be rejected.
- **I3.2** `from_cell` is derived from authentication, never from the payload.
  Two envelopes claiming the same `from_cell` MUST trace to the same
  authenticated credential.
- **I3.3** `kind` is derived from the tool path and correlation: `cell_send` →
  `send`; `cell_ask` → `ask`; `cell_reply` → `reply`; `cell_broadcast` →
  `broadcast`; a daemon-generated wake signal → `notice`.
- **I3.4** An envelope with `kind = reply` MUST carry a non-null `reply_to`
  pointing to an `ask` envelope whose `to_cell` equals this sender's
  `from_cell`; otherwise the daemon MUST reject it with `E_ASK_NOT_FOUND`.
- **I3.5** An envelope with `kind = broadcast` MUST have `to_cell = null`;
  any other kind MUST have a non-null `to_cell`.
- **I3.6** `kind` (transport) and `msg_type` (semantic) are independent axes.
  `kind` selects routing/delivery; `msg_type` classifies the body's role.
  `msg_type = admin_request` additionally requires the sender to hold
  `admin_registry`; otherwise the daemon rejects with `E_ACL_DENIED`.
- **I3.7** `principal_capabilities` is daemon-derived from the sender's
  `CellPrincipal` at enqueue; a client-supplied value MUST be ignored. It is
  advisory trust signal to the recipient, never an authorization input the
  daemon consumes.

## 4. Message kinds

Every envelope has exactly one `kind`. The kind is daemon-derived from the
invoking tool and correlation context (Section 3, I3.3).

- **`send`** — Fire-and-forget message from one cell to one recipient. Enqueued
  and delivered at least once; no reply is expected. Produced by `cell_send`.
- **`ask`** — A request that expects exactly one reply. The daemon creates an
  ask ticket (`ask_id`) and an envelope with `kind = ask`. Produced by
  `cell_ask`.
- **`reply`** — The single response to an `ask`. Carries `reply_to` = the ask's
  `message_id` and the matching `ask_id`. Produced by `cell_reply`. At most one
  reply is recorded per `ask_id`.
- **`broadcast`** — Fan-out from one sender to multiple recipients. In version
  `0.1` this kind requires the explicit `broadcast` capability grant
  (default-deny, Section 11.2); it is not implicitly tied to `admin_registry`.
  `to_cell` is `null`; the daemon fans out to every recipient the sender is
  permitted to reach per ACL. Produced by `cell_broadcast`.
- **`notice`** — A daemon-generated minimal signal carrying no task body. It is
  the only kind permitted to be surfaced to a cell mid-turn (Section 10), and
  only via the experimental hook adapter. Its body is a short fixed signal
  (e.g. *"N bus messages pending; call cell_inbox"*), never a task, never an
  instruction, never an approval prompt. The full bodies remain in the inbox
  and are retrieved explicitly.

A bus message of any kind can never approve, deny, or influence a tool
permission approval, change the ACL, registry, or tokens, or override the
recipient's system, developer, or user instructions. See `THREAT_MODEL.md`
(Injection policy) for the normative delivery preamble that prefixes every
delivered body.

### 4.1 Message semantic types (`msg_type`)

Independent of the transport `kind`, every envelope carries a **semantic**
`msg_type` that classifies the body's role. Distinct semantic types carry
distinct affordances, so a recipient runtime and the audit causal chain can
distinguish a task delegation from evidence, a note, or an admin request. This
is the structured-typing control recommended against peer prompt injection
(audit security posture review §3).

| `msg_type` | Meaning | Extra requirement |
|---|---|---|
| `note` | Informational, no action expected (e.g. a status ping). | — |
| `task` | A work delegation: the sender asks the recipient to do something. The body is still untrusted peer input; the recipient's own permission model governs any tool call it induces. | — |
| `ask` | A question expecting a reply (paired with transport `kind = ask`). | capability `ask` |
| `reply` | The response to an `ask` (paired with transport `kind = reply`). | capability `reply` |
| `evidence` | Data the recipient should consider (e.g. a diff, a log excerpt) referenced via `file_refs`; never an instruction. | — |
| `admin_request` | A bus-administration semantic (e.g. coordinated drain/rotate request). The body is still advisory — a bus message can never change ACL/registry/tokens directly (Section 1, §18). | capability `admin_registry` (in addition to the transport capability) |

Normative rules:

- The daemon MUST validate `msg_type` against the sender's capabilities before
  enqueuing; `admin_request` without `admin_registry` → `E_ACL_DENIED`.
- `msg_type` is advisory classification, never authority: regardless of type,
  every body is delivered as untrusted peer data with the visible envelope
  preamble (Section 18).
- The audit causal chain (Section 12.3) records `msg_type` so that, for
  example, a `task` that induced a high-risk tool call is traceable.

## 5. Tool surface (MCP)

The per-cell shim (`crew mcp`) exposes the following MCP tools. Each tool lists
its capability requirement (Section 11), its `read_only_hint` flag, a normative
JSON Schema (2020-12) for `params` and `result`, and its semantics. All string
identifiers (`*_id`) are UUIDv7 unless noted.

The envelope returned inside results is the normative envelope of Section 3.

### 5.1 `cell_send` — send a `send` message

- Capability: `send`.
- `read_only_hint`: `false`.

```jsonc
// params
{
  "to_cell": "codex-audit",            // REQUIRED, registry name
  "body": "Please review PR #12.",     // REQUIRED, UTF-8, max 64 KiB
  "file_refs": ["pr-12.diff"],         // OPTIONAL, same-host validated
  "idempotency_key": "0196...-uuidv7", // OPTIONAL, client retry-dedupe
  "ttl_seconds": 3600                  // OPTIONAL, <= daemon max
}
// result
{
  "message_id": "0196...-uuidv7",
  "seq": 42,
  "status": "enqueued"
}
```

Semantics: enqueue a `send` to `to_cell`. Returns as soon as the envelope is
durably persisted (`queued` state). Delivery happens asynchronously per the
state machine (Section 7). Unknown recipient → `E_UNKNOWN_CELL`; body too large
→ `E_BODY_TOO_LARGE`; quota exceeded → `E_QUOTA`. If `to_cell` is a **protected
cell** (registry flag `protected = true`, Section 11.5), the caller
additionally requires an explicit per-target grant in `send_to_protected`;
absent → `E_ACL_DENIED` and a `protected_access_denied` audit event (Section
12.2).

### 5.2 `cell_ask` — open an ask ticket (non-blocking)

- Capability: `ask`.
- `read_only_hint`: `false`.

```jsonc
// params
{
  "to_cell": "codex-audit",
  "body": "Is PR #12 safe to merge?",
  "file_refs": ["pr-12.diff"],         // OPTIONAL
  "idempotency_key": "0196...-uuidv7", // OPTIONAL
  "ttl_seconds": 900                   // OPTIONAL, default 900 (15m)
}
// result (returned within <= 2s)
{
  "ask_id": "0196...-uuidv7",
  "message_id": "0196...-uuidv7",
  "status": "pending"                  // or "answered" with "reply" if a reply
  , "reply": null                      //   was already available (e.g. idempotent)
}
```

Semantics (F1): `cell_ask` MUST return within **2 seconds**. It creates the ask
ticket and enqueues the `ask` envelope, then returns `status: "pending"`. It
MUST NOT block indefinitely waiting for a reply. If an idempotent reply is
already on record for the supplied `idempotency_key`, it MAY return
`status: "answered"` with the `reply` inline. The caller obtains the reply via
`cell_await` (Section 6). If `to_cell` is a **protected cell** (Section 11.5),
the caller additionally requires an explicit per-target grant in
`ask_protected`; absent → `E_ACL_DENIED` and a `protected_access_denied` audit
event (Section 12.2).

### 5.3 `cell_await` — long-poll one ask reply

- Capability: `ask`.
- `read_only_hint`: `true` (it does not mutate bus state; it only observes the
  ticket).

```jsonc
// params
{
  "ask_id": "0196...-uuidv7",
  "timeout_ms": 60000                  // OPTIONAL, default and max 120000
}
// result
{
  "status": "answered",                // "answered" | "pending" | "expired"
  "reply": { /* envelope of kind=reply, or null */ }
}
```

Semantics: blocks for at most `timeout_ms` (clamped to **120000 ms**) awaiting
the single reply for `ask_id`. Returns `answered` with the reply envelope as
soon as it is recorded, `expired` if the ask TTL has elapsed, or `pending` on
timeout (the caller decides whether to retry `cell_await` or abandon).
`timeout_ms` greater than 120000 MUST be clamped, not rejected. An `ask_id`
that is unknown, expired, already answered, or **not owned by the caller as the
original asker** MUST be rejected **immediately** (before any wait) with
`E_ASK_NOT_FOUND`. The number of concurrent `cell_await` calls is bounded by
the caller's pending-ask cap (Section 14) and a per-connection await limit
(default 10, daemon-configurable); exceeding the connection limit rejects the
call with `E_QUOTA`.

### 5.4 `cell_reply` — post the single reply to an ask

- Capability: `reply`.
- `read_only_hint`: `false`.

```jsonc
// params
{
  "ask_id": "0196...-uuidv7",          // REQUIRED
  "body": "Approved, merge after CI.", // REQUIRED, UTF-8, max 64 KiB
  "file_refs": [],                     // OPTIONAL
  "idempotency_key": "0196...-uuidv7"  // OPTIONAL
}
// result
{
  "message_id": "0196...-uuidv7",      // id of the recorded reply envelope
  "status": "recorded"                 // or "duplicate" for an idempotent re-post
}
```

Semantics (F1): the `reply` capability is **necessary but not sufficient**: it
permits calling the tool, while the binding authorization is the **ownership
check** — the caller MUST be the cell to which the ask was addressed
(`ask.to_cell == caller.from_cell`). A caller that holds `reply` but is not the
addressee is rejected with `E_ASK_NOT_FOUND` (and a `protected_access_denied`
audit event when the originating ask targeted a protected cell). At most one
reply is recorded per `ask_id`. If a reply is already recorded:
- with identical content (or matching `idempotency_key`) → return the existing
  `message_id` with `status: "duplicate"` (idempotent success, no error);
- with different content → reject with `E_REPLY_EXISTS` and append a
  `duplicate_reply` audit event; the conflicting body is dropped.

### 5.5 `cell_inbox` — pull pending messages addressed to this cell

- Capability: `read_inbox`.
- `read_only_hint`: `true`.

```jsonc
// params
{
  "limit": 50                          // OPTIONAL, default 50, daemon-capped
}
// result
{
  "messages": [ /* envelope, ordered by ascending seq */ ],
  "has_more": false,
  "pending_asks": 2                    // count of open asks where this cell is the responder
}
```

Semantics: returns envelopes addressed to the caller (`to_cell ==
caller.from_cell`) that are deliverable, ordered by ascending per-recipient
`seq`. Pulling transitions each returned delivery to the `delivered` state
(Section 7). There is no separate `cell_ack` tool in version `0.1`: for
inbox-pulled messages the pull is the terminal delivery, and consumers MUST
deduplicate by `message_id` (Section 8). `pending_asks` is informational.

### 5.6 `cell_list` — list registered cells

- Capability: `list_cells`.
- `read_only_hint`: `true`.

```jsonc
// params
{ }
// result
{
  "cells": [
    { "name": "dev-senior", "engine": "claude" },
    { "name": "codex-audit", "engine": "codex" }
  ]
}
```

Semantics: returns the registry names and engines of registered cells. It MUST
NOT expose ACL grants, tokens, online/liveness status beyond registry
membership, or any field usable as a security side-channel.

### 5.7 `cell_broadcast` — fan-out (explicit grant)

- Capability: requires the explicit `broadcast` capability grant (default-deny,
  Section 11.2). It is **not** implicitly tied to `admin_registry`.
- `read_only_hint`: `false`.

```jsonc
// params
{
  "body": "Bus maintenance at 18:00 UTC.",
  "file_refs": [],
  "idempotency_key": "0196...-uuidv7",
  "ttl_seconds": 3600
}
// result
{
  "message_id": "0196...-uuidv7",
  "recipients": ["dev-senior", "codex-audit"],
  "status": "enqueued"
}
```

Semantics: enqueues a `broadcast`. `to_cell` is `null`. The daemon fans out one
`send`-shaped delivery per recipient the sender is permitted to reach per ACL,
each receiving its own `seq` in the `(sender, recipient)` space (Section 9).
A caller without the `broadcast` capability → `E_ACL_DENIED`. The fan-out is
logged as one `broadcast_fanned_out` audit event listing the recipients.

For `cell_broadcast`, fan-out permission is evaluated **per recipient**. A
protected recipient (`protected = true`, Section 11.5) MUST be included in the
fan-out only if the sender also has that recipient listed in
`send_to_protected`. Without that per-target grant, the protected recipient
MUST be omitted from the fan-out and a `protected_access_denied` audit event
MUST be appended naming the broadcast `message_id`, sender, and denied
recipient. The broadcast MAY still succeed for the other permitted recipients.
A broadcast MUST never deliver to a protected cell by virtue of the `broadcast`
capability alone.

### 5.8 Tool-surface notes

- No `cell_ack` and no approval-related tool exist in version `0.1`, by design
  (Section 1 non-goals; THREAT_MODEL injection policy).
- Every result that returns an envelope returns the normative Section 3 shape.
- All tools authenticate as the cell via the connection credential; none accept
  `from_cell` as a parameter.
- **Read tools are cheap, bounded, and unaudited on success (documented
  choice).** `cell_inbox`, `cell_list`, and `cell_await` (`read_only_hint =
  true`) are served from bounded scans (capped `limit`, clamped `timeout_ms`,
  connection await limit) and do **not** append an audit event on a successful
  read, to keep the audit chain focused on state-changing and security-relevant
  actions. Rejections that carry security meaning (e.g.
  `protected_access_denied`, `auth_rejected`, `quota_exceeded`) **are** audited
  regardless of which tool produced them.

## 6. Ask/reply ticket model

This section normatively specifies the `ask`/`reply` lifecycle mandated by audit
finding F1.

### 6.1 Ticket creation and bounded return

`cell_ask` (Section 5.2) MUST return within **2 seconds** of the daemon
receiving the call. Within that window the daemon:

1. Validates the recipient exists and the sender holds the `ask` capability
   (else `E_UNKNOWN_CELL` / `E_ACL_DENIED`), and (if the recipient is protected)
   the per-target `ask_protected` grant (Section 11.5).
2. Notes that the **deadlock check happens at `cell_await` time, not at ask
   creation** (Section 6.4): an ask with no active awaiter adds no wait-for
   edge, so creation never fails with `E_WOULD_DEADLOCK`.
3. Assigns `ask_id` (UUIDv7) and `message_id`, computes `expires_at`, persists
   the ticket and the `ask` envelope, appends an `ask_opened` audit event.
4. Returns `{ask_id, message_id, status: "pending"}` (or `answered` with the
   reply, if an idempotent reply already exists).

The daemon MUST NOT hold the tool call open beyond 2 seconds awaiting a reply.

### 6.2 Awaiting a reply

A reply is collected via `cell_await(ask_id, timeout_ms)` (Section 5.3), which
long-polls one ticket for at most **120000 ms**. On timeout it returns
`status: "pending"`; the caller MAY retry. There is no implicit cancellation:
an ask remains live until its TTL elapses or it is answered.

### 6.3 Single reply, duplicates, expiry

- Each `ask_id` accepts **at most one** reply (Section 5.4). The first
  well-formed reply wins; subsequent differing replies are rejected
  (`E_REPLY_EXISTS`) and recorded as `duplicate_reply` audit events.
- When the ask's `expires_at` elapses with no reply, the ticket transitions to
  `expired`; `cell_await` then returns `status: "expired"`, and the daemon
  appends an `ask_expired` audit event. No reply may be recorded for an expired
  or already-answered ticket (`E_ASK_NOT_FOUND`).
- An `idempotency_key` supplied on `cell_ask` makes sender retries collapse: a
  second `cell_ask` with the same key returns the same `ask_id` (and the
  recorded reply, if any) instead of creating a second ticket.

### 6.4 Deadlock prevention

The daemon maintains a directed **wait-for graph** over live asks: an edge
`A → B` exists **only while** cell `A` has an open `ask` awaiting a reply from
cell `B` **and** `A` is currently blocked in an active `cell_await` on that
`ask_id`. An open ask with **no active awaiter** blocks no one and adds no edge;
this keeps the graph proportional to actual waiters, not to the number of open
asks, and is the SPEC-side resolution of cross-review CR-B-02.

Before a caller enters `cell_await` on `ask_id` (`A → B`), the daemon checks
whether `B` (transitively) already waits on `A`. If activating that edge would
close a cycle, the daemon MUST reject the await with **`E_WOULD_DEADLOCK`** and
append a `deadlock_prevented` audit event naming the cells in the would-be
cycle; the ask itself remains open and answerable, only the await is refused.

**Abuse vector — graph poisoning / topology oracle (CR-B-02):** a compromised
cell could try to map the coordination graph by opening many asks and observing
which `cell_await` calls return `E_WOULD_DEADLOCK`. The daemon mitigates this
by: (a) edges exist only for active awaiters, so merely opening asks without
awaiting poisons nothing; (b) the per-sender pending-ask cap (Section 14, 10)
bounds live asks; (c) the per-`(from_cell, ask)` capability ceiling (Section 14,
20/min) bounds probing rate; (d) every `deadlock_prevented` event is audited,
so a probing burst is observable. This is L0-honest: it constrains and makes
probing observable, it does not make recipient existence or wait-for edges
secret (a recipient's existence is already non-secret under `cell_list`,
Section 5.6).

This prevents the A-waits-B / B-waits-A deadlocks identified in F1. Version
`0.1` implements cycle rejection (not degradation-to-async); a future spec
version MAY add an opt-in async-downgrade policy.

## 7. Delivery state machine

Each (envelope, recipient) pair has its own delivery record, persisted by the
daemon, progressing through the following states. The state machine is the
normative answer to audit finding F5.

States:

```
queued ──▶ claimed ──▶ delivered ──▶ acked
   │           │            │
   │           │            └──▶ failed      (retryable budget exhausted, or non-retryable)
   │           └──▶ queued   (lease expired without outcome)
   │
   └──▶ expired              (TTL elapsed before/while pending)
```

Terminal states: `acked`, `failed`, `expired`.

### 7.1 Transition table

| From | To | Trigger | Owner | Bounded by |
|---|---|---|---|---|
| `queued` | `claimed` | Adapter accepts a delivery attempt and is granted a lease. | adapter | lease duration (default 30 s) |
| `claimed` | `delivered` | Adapter reports successful handoff to the cell engine. | adapter | — |
| `claimed` | `queued` | Lease elapsed with no outcome (release for retry). | daemon | lease duration |
| `claimed` | `failed` | Adapter reports a non-retryable failure. | adapter | — |
| `delivered` | `acked` | Adapter reports consumption acknowledgment (v0: adapter-reported only). | adapter | ack grace (default 60 s) |
| `queued` | `failed` | Retryable-failure budget exhausted after the configured max attempts. | daemon | max attempts × backoff |
| `queued` / `claimed` | `expired` | `expires_at` elapsed and no terminal positive outcome. | daemon | envelope `expires_at` |

### 7.2 Retry, backoff, and lease

- On a retryable failure (adapter transient failure, or lease lapse), the
  delivery returns to `queued` and is retried with **exponential backoff**:
  base 1 s, factor 2, cap 60 s, with jitter, up to a default of **10** attempts
  (all daemon-configurable).
- A **lease** on `claimed` prevents the daemon from issuing overlapping
  delivery attempts for the same record within the lease window; it does not
  prevent later redelivery, which is permitted and expected.
- A delivery that exhausts its retry budget transitions to `failed` and the
  daemon appends a `delivery_failed` audit event.
- `expires_at` is a hard ceiling: no transition may extend delivery attempts
  past it; the record goes to `expired` instead.

### 7.3 Delivery guarantee (at-least-once)

The bus provides **at-least-once** delivery. A given message MAY be delivered
to a recipient more than once — for example, when a lease lapses after a
successful handoff but before the outcome is recorded, or when an adapter or
the daemon restarts. Consequently:

- Consumers (recipient cells) MUST deduplicate every delivered envelope by
  `message_id` (Section 8). There is no consumer for which dedupe is optional.
- The daemon SHOULD use idempotent handoff where an adapter supports it, but
  MUST NOT rely on it: correctness rests on consumer-side dedupe, not on any
  adapter's behavior.
- No component of the bus may claim or imply single-delivery semantics.

## 8. Dedupe & idempotency

Two independent dedupe layers are normative: **submission dedupe** (sender
side, collapse duplicate sends) and **consumption dedupe** (recipient side,
tolerate duplicate deliveries).

### 8.1 Submission dedupe (`idempotency_key`)

- The daemon generates an `idempotency_key` (UUIDv7) for every envelope.
- A client MAY supply its own `idempotency_key` on `cell_send`, `cell_ask`, and
  `cell_broadcast` to make retries collapse. Two submissions from the same
  `from_cell` bearing the same key within the envelope's TTL (plus a daemon
  grace window, default 1 hour) MUST resolve to the same `message_id` (and, for
  `ask`, the same `ask_id`) rather than creating distinct envelopes.
- Submission dedupe is scoped to `(from_cell, idempotency_key)`. A key reused
  by a different sender does not collide.
- If a client reuses a key with **different** body/addresses, the daemon MUST
  reject the second submission with `E_DUP` (the key is bound to the first
  submission's content).

### 8.2 Consumption dedupe (mandatory)

- Every recipient MUST track `message_id`s it has already processed, for at
  least the envelope TTL plus the daemon grace window, and skip redelivered
  envelopes. This is the load-bearing dedupe layer of the bus.
- For `ask`/`reply`, the ticket itself provides correlation dedupe: a recipient
  processes a given `ask_id` once; the daemon records at most one reply.
- `file_refs` are immutable per envelope; redelivery never alters them.

### 8.3 Duplicate-reply handling

A second reply to an `ask_id` (Section 5.4 / 6.3) is recorded as a
`duplicate_reply` audit event and dropped; differing-content duplicates are
rejected with `E_REPLY_EXISTS`.

## 9. Ordering guarantees

- There is **no global order** across the bus.
- `seq` is a per-pair monotonic counter: it reflects the order in which the
  daemon enqueued messages for the ordered pair `(from_cell, to_cell)`.
- **Per-recipient FIFO is opt-in.** A recipient MAY declare
  `order_preserving = true` in the registry. When set, the daemon delivers to
  that recipient strictly in ascending `seq` order and holds back a later
  message until every earlier one reaches a terminal state (`acked`, `failed`,
  `expired`). When unset (default), delivery is best-effort with no head-of-line
  blocking.
- For `broadcast`, each recipient gets its own `seq` in the
  `(sender, recipient)` space; cross-recipient ordering is not guaranteed.
- Ordering is **best-effort under redelivery**: because delivery is
  at-least-once, an earlier `message_id` may legitimately re-arrive after a
  later one. Consumers MUST dedupe and SHOULD be order-tolerant; strict
  ordering is only meaningful together with `order_preserving` and dedupe.

## 10. Delivery adapters contract

Adapters translate a daemon delivery request into an actual handoff to a cell
engine. Per audit finding F4, adapters are **implementations** of a common
contract; the bus data model and all delivery guarantees live in `crewd`, never
in an adapter. ACP is one implementation, not the protocol's basis.

### 10.1 Adapter trait (interface contract)

The daemon invokes an adapter through the following trait-level contract
(language-neutral; the Rust trait in code mirrors it exactly):

```
trait DeliveryAdapter {
    /// Attempt to hand `envelope` to the recipient cell engine.
    /// Must be non-blocking beyond a bounded, short duration; long waits
    /// are expressed by returning NotReady and being re-invoked.
    fn deliver(&self, envelope: &Envelope) -> DeliveryOutcome;
}

enum DeliveryOutcome {
    Delivered,          // handoff succeeded; record -> delivered
    Acked,              // handoff + consumption ack; record -> acked
    TransientFailure,   // retryable; record -> queued (backoff)
    PermanentFailure,   // non-retryable; record -> failed
    NotReady,           // cell not currently receivable; retry later (backoff)
}
```

`DeliveryOutcome` drives the Section 7 state transitions normatively:
`Delivered`→`delivered`, `Acked`→`acked`, `TransientFailure`/`NotReady`→
`queued` (retry), `PermanentFailure`→`failed`.

**Delivery timing (normative):** `deliver` MUST return within **250 ms** (wall
clock). An adapter that cannot complete a handoff within that bound MUST return
`NotReady` (the daemon re-queues the record with backoff, Section 7.2) rather
than holding the call. This keeps the daemon's delivery loop responsive under a
slow or stuck recipient engine and prevents one cell from stalling delivery to
others.

### 10.2 Fake adapter (testability, REQUIRED)

A **fake adapter** MUST exist as a first-class implementation, used by contract
tests so the bus can be tested without Claude Code, Codex, tmux, or any
external runtime. Contract tests inject envelopes into the fake adapter and
assert daemon state, audit events, retry behavior, lease expiry, and dedupe.
The fake adapter is the reference for the trait contract.

### 10.3 Implementations in scope for version `0.1`

| Adapter | Mechanism | Status in `0.1` |
|---|---|---|
| `fake` | In-process, test only. | REQUIRED for contract tests. |
| `tmux` | `send-keys` (best-effort, last-resort fallback). | Impl; best-effort only. |
| `hook` | `crew hook stop \| posttooluse`; Stop long-poll used only as an idle wake signal. | **Experimental**, gated behind the `experimental_hook_wake` feature flag. NOT required for correctness. |
| `appserver` | Codex JSON-RPC `turn/start`, `turn/steer`. | Impl; lower priority in `0.1`. |
| `acp` | `session/prompt` via the `agent-client-protocol` crate. | Impl; **off the critical path** for `0.1` (see 10.4). |

### 10.4 ACP is off the critical path (F4)

- ACP is one `DeliveryAdapter` implementation. The bus envelope, state machine,
  ACL, audit, and delivery guarantees are defined entirely by `crewd` and this
  SPEC; none of them is inherited from or dependent on ACP semantics.
- No single-delivery guarantee may be derived from ACP. Any delivery semantics
  the bus offers are provided by `crewd` (Section 7).
- The code MUST pin an exact `agent-client-protocol` crate version and declare a
  Minimum Supported Rust Version (MSRV); contract tests MUST cover delivery via
  a fake ACP agent and trace replay. Until those pass, the `acp` adapter MUST
  NOT be enabled for production cells.

### 10.5 Mid-turn delivery rule (F3)

Adapters MUST NOT deliver a message body into a cell mid-turn. The only
mid-turn surface permitted is the `notice` kind (Section 4): a short, fixed
signal with no task body, no instruction, and no approval prompt. Full message
bodies are retrieved explicitly by the recipient via `cell_inbox`. Every
delivered body (including via the hook adapter) MUST be prefixed by the
normative delivery preamble defined in `THREAT_MODEL.md` (Injection policy).

## 11. ACL capability model

Authorization is **capability-based** (audit finding F9): a cell's permissions
are expressed as a set of granular capabilities, not as a coarse "open reads /
scoped sends" rule. The capability set is the normative authorization unit; the
TOML file is its serialization for version `0.1`.

### 11.1 Capabilities

| Capability | Grants |
|---|---|
| `send` | Call `cell_send`. |
| `ask` | Call `cell_ask` and `cell_await`. |
| `reply` | Call `cell_reply` (post the single reply to an ask addressed to this cell). |
| `broadcast` | Call `cell_broadcast` (fan-out). |
| `read_inbox` | Call `cell_inbox`. |
| `list_cells` | Call `cell_list`. |
| `attach_files` | Include non-empty `file_refs` on any outbound message. |
| `wake` | Trigger an idle-cell wake via the experimental hook adapter. |
| `admin_registry` | Read/write the cell registry and the ACL file through the CLI. |
| `read_audit` | Read the audit chain through the CLI. |

### 11.2 Default-deny set

The following capabilities are **default-deny**: a cell holds them only via an
explicit grant, never by omission of a deny rule:

- `broadcast`
- `wake`
- `admin_registry`
- `attach_files`

All other capabilities are granted to a cell by being listed in its grant set;
absence of a capability means the tool call is rejected with `E_ACL_DENIED`.

### 11.3 TOML serialization (version `0.1`)

```toml
# acl.toml — version 0.1 example

[cell.dev-senior]
capabilities = [
  "send", "ask", "reply", "read_inbox", "list_cells", "read_audit",
]
# Per-target grants to reach protected cells (Section 11.5); default-deny.
send_to_protected = ["coordinator"]
ask_protected      = ["coordinator"]

[cell.codex-audit]
capabilities = [
  "send", "ask", "reply", "read_inbox", "list_cells", "attach_files",
]

[cell.coordinator]
protected = true                    # high-privilege cell (Section 11.5)
capabilities = [
  "send", "ask", "reply", "broadcast", "read_inbox", "list_cells",
  "attach_files", "admin_registry", "read_audit",
]
# 'wake' is intentionally absent everywhere by default (experimental).
```

Normative parsing rules:

- Each `[cell.<name>]` table lists exactly the capabilities granted to that
  cell. Unknown capability strings MUST fail validation with `E_INTERNAL` and
  abort the reload.
- A capability appears at most once per cell; duplicates fail validation.
- `send_to_protected` and `ask_protected` are OPTIONAL arrays of registry names
  (protected or not); names not present in the registry fail validation. They
  grant only the per-target reach described in Section 11.5, never a capability.
- The file is read atomically: a reload either applies in full or does not
  apply at all. A partially valid file MUST be rejected in full, leaving the
  previously loaded ACL active.
- The loaded ACL is keyed by the authenticated `from_cell`; the daemon MUST NOT
  accept a capability assertion from a cell at request time.

### 11.4 Reload semantics

- Reload is **atomic and validated**: parse → validate (known capabilities, no
  duplicates, default-deny set respected) → swap the in-memory ACL in a single
  step. On validation failure the previous ACL remains authoritative.
- A successful reload MUST append an `acl_changed` audit event whose payload
  records the new grant sets (per cell) and the actor that triggered the reload
  (CLI session or coordinator cell).
- Reloads are triggered via the operational CLI by a principal holding
  `admin_registry` (Section 15). There is no in-band reload through a bus
  message (a bus message can never change the ACL — Section 1 non-goals).

### 11.5 Protected cells (minimal per-target gating)

A cell may be marked `protected = true` in the registry when it is high-privilege
(coordinator, auditor, bus-admin, or any cell that holds dangerous capabilities
or reaches sensitive systems). This is the minimal resolution of cross-review
CR-A-04 / CR-B-01: **no full per-recipient matrix**, only a default-deny reach
rule toward a small set of protected targets.

- `send` to a protected `to_cell` additionally requires the target to be listed
  in the caller's `send_to_protected` array (Section 11.3). Absent →
  `E_ACL_DENIED` and a `protected_access_denied` audit event (Section 12.2).
- `ask` to a protected `to_cell` additionally requires the target to be listed
  in the caller's `ask_protected` array. Absent → `E_ACL_DENIED` and a
  `protected_access_denied` audit event.
- `cell_broadcast` is subject to the **same per-recipient gate**: a protected
  recipient is included in the fan-out only if listed in the sender's
  `send_to_protected`; otherwise it is omitted and a `protected_access_denied`
  event is appended (Section 5.7). The `broadcast` capability never authorizes
  delivery to a protected cell on its own.
- The `protected` flag and the per-target arrays are **registry/ACL data**, not
  capabilities: holding `send` or `ask` is still necessary, and the per-target
  grant never implies any capability. `reply` to an ask that the caller
  legitimately received from a protected cell is governed by the normal
  ownership check (Section 5.4), not by `ask_protected`.
- This is L0-honest: it raises friction and makes targeted access to
  high-privilege cells auditable, it does not prevent a same-UID attacker that
  can rewrite the ACL file (Section 17 / THREAT_MODEL §4).

### 11.6 Registry invariants

- Cell names are **unique** in the registry. A registration/rename that would
  duplicate an existing name MUST be rejected.
- A cell's token is **bound at registration**: the registry maps
  `cell_name → token_id` (and the token material lives in a `0600` file per
  Section 17.3). A name without its bound token is not an identity.
- **Rename is a new identity.** Renaming a cell, or re-registering a name after
  deletion, MUST mint a **new token** and start a **new audit history**; a
  rename MUST NOT inherit another cell's token, capabilities, per-target
  grants, or audit chain (closes the registry-poisoning / identity-inheritance
  vector — THREAT_MODEL T-18).
- Every registration, rename, deletion, and `protected` flag change is an
  admin-gated (`admin_registry`) mutation that MUST append a `registry_changed`
  audit event recording the change and the operator principal.

## 12. Audit events

Every security-relevant action is recorded as an immutable audit event in an
append-only chain. The chain is the forensic record of the bus (who said what
to whom, when, and with what outcome) and the detection control that makes
spoofing observable (Section 1; see `THREAT_MODEL.md`).

### 12.1 Event schema

```json
{
  "event_id": "0196...-uuidv7",
  "ts": "2026-07-03T12:00:00.123Z",
  "kind": "delivered",
  "message_id": "0196...-uuidv7",
  "from": "dev-senior",
  "to": "codex-audit",
  "outcome": "ok",
  "detail": { /* kind-specific, optional */ },
  "prev_hash": "9f1a...hex",
  "hash": "c0b2...hex"
}
```

Normative fields:

| Field | Rule |
|---|---|
| `event_id` | UUIDv7, daemon-assigned, unique. |
| `ts` | RFC 3339 UTC with sub-second precision, daemon clock. |
| `kind` | One of the event kinds in 12.2. |
| `message_id` | The related envelope's `message_id`, when applicable; omitted (`null`) for daemon-level events (e.g. `acl_changed`). |
| `from` / `to` | The authenticated `from_cell` and the addressed `to_cell` (or `"*"` for broadcast fan-out entries). |
| `outcome` | Short stable string: `ok`, `denied`, `dropped`, `failed`, `expired`, `rejected`. |
| `detail` | OPTIONAL, kind-specific structured payload (e.g. recipients list, cycle cells, quota figures). MUST NOT contain message bodies or secrets. |
| `prev_hash` | SHA-256 `hash` of the immediately preceding event; `"0000..."` (all-zero) for the chain genesis. |
| `hash` | SHA-256 over the canonical-JSON serialization of the event with the `hash` field removed, concatenated with `prev_hash`. |

Canonical JSON: UTF-8, no insignificant whitespace, object keys sorted
lexicographically, no trailing newline. The exact canonicalization MUST be
specified identically in code and in the contract test.

### 12.2 Event kinds

`enqueued`, `delivered`, `acked`, `delivery_failed`, `expired`, `ask_opened`,
`ask_expired`, `duplicate_reply`, `acl_changed`, `quota_exceeded`,
`deadlock_prevented`, `broadcast_fanned_out`, `auth_rejected`,
`spec_version_rejected`, `protected_access_denied`, `registry_changed`,
`token_revoked`.

Each tool call and state transition in Sections 5–7 maps to at least one event
(e.g. `cell_send` → `enqueued`; delivery → `delivered` or `delivery_failed`;
expiry → `expired`).

### 12.3 Chain integrity and causal linkage

- The chain is **append-only**: events MUST NOT be modified or deleted in-band.
  Operational trimming, when introduced, MUST be hash-preserving (snapshot the
  head hash into a new genesis) and is out of scope for version `0.1`.
- The daemon MUST fsync each appended event before acknowledging the action it
  records, so that a crash leaves a chain that is consistent with delivered
  state.
- The hash-chain key material (any secret used to tag/verify chain ownership,
  beyond the pure `prev_hash` linkage) MUST be held by the daemon and stored
  with permissions separated from ordinary cell-readable state (e.g. owned by a
  dedicated `crew` user where deployed). This makes the chain tamper-evident
  against ordinary same-UID cell reads; it is not anti-tamper against a same-UID
  attacker with write access (see `THREAT_MODEL.md`).
- For tool requests triggered by a peer message, the audit MUST capture the
  causal chain `message_id → tool → outcome` (including `msg_type`) so that
  downstream effects of an untrusted peer message are traceable (see
  `THREAT_MODEL.md`, Injection policy). This causal linkage is normative and
  **mandatory** for any **high-risk** tool call that is **bus-induced**, where:
  - a tool call is **bus-induced** if it occurs in the same turn of processing a
    delivered bus message whose `msg_type ∈ {task, ask, evidence,
    admin_request}` (a `note` or `reply` carrying no work delegation does not,
    by itself, make a call bus-induced);
  - a tool is **high-risk** if it is named in the recipient's
    registry-configured high-risk set, or flagged high-risk by the recipient
    runtime at call time (e.g. destructive filesystem writes, network egress,
    publish/deploy, secret or credential access). The daemon does not judge
    tool semantics: the recipient reports the `(message_id, tool, high-risk)`
    correlation and the daemon records it in the chain.
- The chain MUST be machine-verifiable end-to-end: `crew audit verify`
  (Section 15) walks every `prev_hash`/`hash` link and the canonical-JSON
  recomputation, and reports the first broken event. Under same-UID this is
  tamper-**evident** (detection-grade), not tamper-proof (THREAT_MODEL §4).

## 13. Error codes

Errors returned to MCP tool callers use **stable string codes** (a contract test
asserts the exact set). Each error carries a stable `code` string and a
human-readable `message`; the `code` is the normative identifier.

| Code | Meaning | Returned by |
|---|---|---|
| `E_ACL_DENIED` | Caller lacks the required capability, or lacks the per-target grant for a protected recipient (Section 11.5). | any tool |
| `E_AUTH_REJECTED` | The connection proof does not match the claimed cell identity — unknown token, wrong token, or a token that has been revoked (Section 17.2). Maps to the `auth_rejected` audit event. | connection handshake; any tool on a revoked/invalid session |
| `E_UNKNOWN_CELL` | `to_cell` is not a registered cell. | `cell_send`, `cell_ask` |
| `E_TTL_EXPIRED` | The envelope (or ask ticket) has passed its `expires_at`. | `cell_await`, delivery |
| `E_WOULD_DEADLOCK` | Activating a wait-for edge (entering `cell_await`) would close a cycle (Section 6.4). | `cell_await` |
| `E_QUOTA` | Sender exceeded a rate/quota limit, or the queue is saturated; new message rejected. | `cell_send`, `cell_ask`, `cell_broadcast` |
| `E_BODY_TOO_LARGE` | `body` exceeds 64 KiB, or a `file_ref` exceeds its limit. | any outbound tool |
| `E_DUP` | A reused `idempotency_key` with different content (Section 8.1). | `cell_send`, `cell_ask`, `cell_broadcast` |
| `E_ASK_NOT_FOUND` | `ask_id` is unknown, expired, already answered, or not owned by the caller. | `cell_await`, `cell_reply` |
| `E_REPLY_EXISTS` | A differing reply is already recorded for this `ask_id`. | `cell_reply` |
| `E_UNSUPPORTED_SPEC_VERSION` | The `spec_version` offered by a shim is not accepted by the daemon. | connection handshake |
| `E_INTERNAL` | Unexpected daemon error; details logged, not leaked to the caller. | any |

Stability rule: codes are part of the protocol contract. Adding a code is a
minor version bump; removing, renaming, or changing the meaning of a code is a
major version bump (Section 16).

## 14. Quotas & backpressure

- **Per-sender rate limit:** default **60 messages / minute** (sliding window,
  per `from_cell`, counting `send` + `ask` + `broadcast` fan-out units). Daemon
  defaults are configurable.
- **Per-capability rate limit:** in addition to the per-sender ceiling, the
  daemon applies per-`(from_cell, capability)` limits, with explicit default
  ceilings for the dangerous capabilities (all daemon-configurable). They exist
  to contain a compromised or noisy cell's lateral movement on a single
  capability even when its aggregate sender rate is within budget. Defaults:

  | Capability | Default per-`(from_cell, capability)` ceiling |
  |---|---|
  | `ask` | 20 / minute |
  | `broadcast` | 5 / minute |
  | `attach_files` (non-empty `file_refs`) | 10 / minute |
  | `wake` | 10 / minute |
  | `admin_registry` | 10 / minute |
  | `send`, `reply` | governed by the per-sender 60 / minute ceiling |
- **Pending-ask cap:** default **10 open asks** per `from_cell` as the
  requester. Exceeding it rejects new `cell_ask` with `E_QUOTA`.
- **Queue depth:** each recipient has a bounded pending queue (default depth
  configurable). On overflow the daemon MUST **reject new** messages for that
  recipient with `E_QUOTA`. The daemon MUST NOT silently drop the oldest
  pending message (no oldest-drop policy): rejection-with-error is the only
  overflow behavior, so senders learn to retry.
- **Backpressure:** when the daemon is saturated (disk, CPU, or global queue
  thresholds), inbound tool calls are rejected with `E_QUOTA`; the error
  `message` MAY include a `retry_after_seconds` hint. Senders SHOULD back off.
- Quota events: every rejection appends a `quota_exceeded` audit event.

## 15. Operational CLI

The `crew` CLI is the operational surface for administrators. It speaks to the
daemon over the same authenticated socket; admin subcommands require the caller
to hold `admin_registry` and/or `read_audit`.

| Command | Capability | Semantics |
|---|---|---|
| `crew status` | `read_audit` | Print daemon health: uptime, queue depths per recipient, count of open asks, terminal-failure count, and the current audit chain head `hash`. Read-only. |
| `crew inspect <id>` | `read_audit` | Print the envelope and all delivery records plus the audit events for a `message_id` or `ask_id`. Read-only. |
| `crew retry <id>` | `admin_registry` | Re-queue a `failed` (or non-terminal) delivery for one more attempt, bypassing the exhausted retry budget. Appends an audit event (`kind = enqueued`, `detail.retry_of`). Does not bypass `expires_at`. |
| `crew drain <cell>` | `admin_registry` | Flush all pending, deliverable messages for a recipient through its adapter (used before maintenance or shutdown). Records per-message outcomes in the audit chain. |
| `crew audit verify` | `read_audit` | Walk the audit hash chain end-to-end (every `prev_hash`/`hash` link plus the canonical-JSON recomputation, Section 12.1). Prints `OK <head_hash>` on a clean chain, or `BROKEN at <event_id>` pinpointing the first event whose `hash` does not recompute or whose `prev_hash` does not link. Read-only. Normative: a tamper-evident chain MUST be machine-verifiable (Section 12.3). |

Normative CLI rules:

- The CLI MUST NOT provide any path to bypass ACL, approve tool permissions,
  inject messages as a cell, or read message bodies beyond what `inspect`
  requires for diagnosis.
- Every state-changing CLI command (`retry`, `drain`) MUST append audit events
  identifying the operator principal.

## 16. Versioning & compatibility

- The `spec_version` field (Section 3) carries the SPEC version an envelope was
  produced under. For this document it is the string `"0.1"`.
- `spec_version` is daemon-assigned: envelopes carry the daemon's active
  version, never a client-supplied value. A shim declares the version it speaks
  at connection handshake; a mismatch is rejected with `E_UNSUPPORTED_SPEC_VERSION`
  and a `spec_version_rejected` audit event.
- **Minor** bump (e.g. `0.1` → `0.2`): additive only — new optional fields, new
  event kinds, new error codes, new capabilities. A daemon accepting a higher
  minor version MUST tolerate shims and envelopes at lower minors within the
  same major.
- **Major** bump (e.g. `0.x` → `1.0`): removing or renaming a field, state,
  event kind, error code, capability, or tool, or changing the meaning of any
  of them. Major bumps require explicit migration and MAY break compatibility.
- Canonicalization (Section 12.1), the audit hash function (SHA-256), and the
  error-code set are themselves versioned aspects of the protocol; changing
  them is at minimum a minor bump and, if it breaks verification, a major bump.

---

## 17. Identity and delivery trust boundary

The bus protocol never trusts `from_cell` or any sender identity field supplied
by the MCP payload. `crewd` derives the sender from the configured identity
backend and stores the resulting `CellPrincipal` in the message envelope and
audit log.

The identity backend is replaceable. v0 may use per-cell token files plus Unix
peer credentials (`SO_PEERCRED`, and `SO_PEERPIDFD` when available). Future
backends may bind cells to dedicated Unix users, systemd units, or container
identities without changing the message envelope, ACL model, or `cell_*` tool
contract.

L0 identity is not a strong isolation boundary when multiple cells run under the
same Unix UID. Under L0, `crewd` provides daemon-side attribution, ACL checks,
rate limits, and auditability, but not cryptographic non-spoofability against a
cell that can read another cell's token or same-UID filesystem state.

### 17.1 Identity contract (normative)

Identity is a pluggable backend expressed through the following contract. The
code trait mirrors it exactly; envelope, ACL, and audit consume only the
principal abstraction, never raw tokens or UIDs:

```rust
struct CellPrincipal {
    cell_id: String,
    auth_level: AuthLevel,      // L0Token, UnixUid, IsolatedService, Container
    unix_uid: Option<u32>,
    unix_gid: Option<u32>,
    pid: Option<u32>,
    pidfd_supported: bool,
    token_id: Option<String>,
}

trait AuthBackend {
    fn authenticate(&self, peer: PeerCred, proof: ClientProof) -> Result<CellPrincipal>;
}

trait CredentialIssuer {
    fn issue(&self, cell_id: &str, scope: CredentialScope) -> Result<IssuedCredential>;
    fn rotate(&self, cell_id: &str) -> Result<IssuedCredential>;
    fn revoke(&self, cell_id: &str, token_id: &str) -> Result<()>;
}
```

Normative rules:

- ACL checks (Section 11) and audit events (Section 12) consume **only**
  `CellPrincipal`. The protocol speaks in terms of principals, never tokens or
  UIDs.
- The payload MUST NOT carry an authoritative `from_cell`; identity comes
  exclusively from `AuthBackend::authenticate` over the peer credential and
  client proof.
- `CredentialIssuer` (issuance, rotation, revocation) is part of the identity
  contract **even in v0**: tokens are not permanent, not hand-waved into
  existence. v0 supplies a file/`fd`-based issuer; stronger issuers plug in
  without touching the `cell_*` protocol.
- Upgrading the deployment from L0 to L1/L2/L3 changes the `AuthBackend` and
  `CredentialIssuer` implementations (and the resulting `AuthLevel`), never the
  envelope, ACL model, or tool contract.

### 17.2 L0 invariants (mandatory in v0)

Even at L0, the following are normative for v0:

- The per-cell credential MUST NOT be passed via an environment variable when a
  restricted file path or an inherited file descriptor is available; env-based
  transport is a fallback only, with the reason recorded.
- Tokens MUST have a TTL and MUST support rotation (`CredentialIssuer::rotate`)
  and revocation (`CredentialIssuer::revoke`) in v0. **Revocation**
  (`revoke(cell_id, token_id)`) MUST immediately invalidate every live session
  bound to that `token_id` (forcible close of the connection(s)); any subsequent
  call on such a session MUST be rejected with `E_AUTH_REJECTED` (Section 13)
  and append an `auth_rejected` audit event. The daemon MUST append a
  `token_revoked` audit event at revocation time, naming the revoked
  `token_id` and the revoking principal.
- `from_cell` is always daemon-derived, never payload-supplied (restates I3.2).
- Rate limits apply per sender **and** per capability (Section 14).
- The audit causal chain is mandatory for high-risk tool calls induced by a peer
  message (Section 12.3).
- The hash-chain key is held by the daemon with permissions separated from
  cell-readable state (Section 12.3).

### 17.3 Filesystem & socket hardening (MUST)

The following are normative v0 invariants — the SPEC-side mirror of
`THREAT_MODEL.md` §7 hardening items H-01/H-02/H-03/H-04/H-09/H-10. They apply
to `crewd`'s own runtime artifacts (not to cell repositories). Each is testable
as stated in `THREAT_MODEL.md` §7:

- **Runtime directory `0700`** (socket, DB, tokens, audit), owned by the daemon
  user, created before any bind (H-01).
- **Listening socket `0600`** inside that `0700` directory (H-02).
- **Unlink-before-bind with `O_NOFOLLOW`/`O_CREAT|O_EXCL`**: the daemon MUST
  refuse to bind if the target path is a symlink or a foreign stale socket
  (H-03).
- **Socket path from explicit config** (a fixed runtime dir), NOT derived from
  the current working directory (H-04).
- **DB, token files, and the audit store each `0600`** inside the `0700`
  runtime dir (H-09).
- **No secret on `argv`, `--help`, or log/audit output** — only token hashes or
  prefixes may be logged (H-10).
- **Unix peer credentials (H-05, MUST)** — `crewd` MUST obtain Unix peer
  credentials (`SO_PEERCRED` on Linux, the platform equivalent elsewhere) for
  every accepted connection and record pid/uid/gid in the authenticated
  `CellPrincipal` and the connection audit detail. If peer credentials cannot be
  obtained, the connection MUST be rejected with `E_AUTH_REJECTED`.
- **Dedicated daemon user (H-18, MUST; L0-degraded mode documented)** — `crewd`
  MUST run as a dedicated daemon user (for example `crew`) distinct from
  ordinary cell users in deployments where cells are launched by this project.
  The runtime directory, DB, token store, audit store, and audit-chain key
  material MUST be owned by that daemon user and not readable by cell users. A
  local developer deployment that deliberately runs `crewd` and cells under a
  single UID MUST be documented as an **L0-degraded mode** in which H-18 is not
  satisfied (the same-UID risks of Section 17 / `THREAT_MODEL.md` §4 then apply
  in full).

## 18. Bus message trust

All bus message bodies are untrusted peer input. A bus message cannot approve
permissions, modify ACL/registry/token policy, or override system, developer, or
user instructions of the recipient. The recipient runtime must present bus
messages as peer input with a visible envelope (`from`, `kind`, `message_id`,
`ask_id`, `principal_capabilities`, `created_at`) and must not inject peer
message bodies as privileged system reminders or hidden context.

The v0 delivery guarantee is at-least-once with idempotency and dedupe. Exactly
once delivery is not promised.

### 18.1 Delivery presentation (normative)

- The visible envelope presented to the recipient MUST include at least
  `from`, `kind`, `msg_type`, `message_id`, `ask_id`, `principal_capabilities`,
  and `created_at`. `principal_capabilities` (the field normatively defined in
  Section 3) denotes the capabilities asserted by the sender's principal for
  auditability, not permissions granted to the recipient.
- The body MUST be delivered as peer data with the normative delivery preamble
  defined in `THREAT_MODEL.md` (Injection policy), never as a system reminder,
  hidden context, or `PostToolUse.additionalContext` body in v0.
- The only mid-turn surface permitted is a `notice` signal with no task body
  (Sections 4 and 10.5).

## 19. Testing & conformance

The protocol ships with a conformance test suite that asserts the normative
statements of this SPEC. The suite is normative: an implementation that fails a
conformance test is not compliant with spec version `0.1`.

### 19.1 Required test categories

The conformance suite MUST include at minimum the following categories, each
exercised through the fake adapter (Section 10.2) so tests do not depend on
Claude Code, Codex, tmux, or any external runtime:

- **Envelope authority** — the daemon rejects client-supplied `message_id`,
  `ask_id`, `seq`, `from_cell`, and `kind`/`msg_type` it cannot validate
  (I3.1–I3.6).
- **Ask/reply lifecycle (F1)** — `cell_ask` returns within 2 s; `cell_await`
  honors `timeout_ms` and the 120 s clamp; single reply per `ask_id`;
  `duplicate_reply` audit on conflict; `E_WOULD_DEADLOCK` is returned by
  `cell_await` (not by `cell_ask`) when activating a wait-for edge would close
  a cycle; an ask with no active awaiter adds no edge; TTL expiry returns
  `expired`; `cell_await` on a non-owned `ask_id` is rejected immediately with
  `E_ASK_NOT_FOUND`.
- **Delivery state machine (F5)** — every transition in Section 7.1 is
  reproduced, including lease lapse → redelivery, retry backoff exhaustion →
  `failed`, and TTL → `expired`; at-least-once redelivery is injected and
  consumer-side dedupe is asserted.
- **ACL capability (F9)** — default-deny capabilities are rejected without an
  explicit grant; `admin_request` requires `admin_registry`; atomic reload
  rejects partial/invalid files and leaves the prior ACL active; `acl_changed`
  is appended.
- **Quotas & backpressure** — overflow rejects new with `E_QUOTA` (no
  oldest-drop); per-sender and per-capability limits enforced independently.
- **Audit chain** — SHA-256 `prev_hash`/`hash` linkage verifies; an event
  appended out-of-order or mutated fails verification; causal chain captures
  `message_id → tool → outcome`.
- **Audit verify CLI (CR-A-01)** — `crew audit verify` prints `OK <head_hash>`
  on a clean chain and `BROKEN at <event_id>` after a one-byte flip in the
  audit store; it is read-only and requires `read_audit`.
- **Protected cells (D2)** — `cell_send`/`cell_ask` to a `protected = true`
  recipient without the matching `send_to_protected`/`ask_protected` grant are
  rejected with `E_ACL_DENIED` and a `protected_access_denied` audit event; with
  the grant they succeed; `cell_reply` by the legitimate addressee still works
  regardless of the protected flag (ownership check, not `ask_protected`).
- **Broadcast to protected cells (G1-01)** — a sender holding `broadcast` but
  without `send_to_protected = ["<protected-cell>"]` does not deliver to that
  protected recipient, appends a `protected_access_denied` event naming the
  broadcast `message_id`, sender, and denied recipient, and the broadcast still
  succeeds for the other permitted recipients; adding the per-target grant
  includes the protected recipient in the fan-out. A `broadcast`-only sender
  (no `send_to_protected`) never reaches a protected cell.
- **Deadlock graph poisoning (CR-B-02)** — opening asks without awaiting adds no
  wait-for edge and never yields `E_WOULD_DEADLOCK`; `E_WOULD_DEADLOCK` fires
  only at `cell_await` on a real cycle; a probing burst of cycle-check
  rejections is bounded by the pending-ask cap and the per-`ask` rate ceiling,
  and every rejection appends `deadlock_prevented`.
- **Bus-induced / high-risk causal chain (CR-B-04)** — a tool call made in the
  same turn as a delivered `msg_type ∈ {task, ask, evidence, admin_request}`
  message, where the recipient flags the tool high-risk, produces an audited
  causal link `message_id → tool → outcome`; a tool call in a turn with only a
  `note`/`reply` is not recorded as bus-induced.
- **Identity & revocation (CR-B-07, CR-A-12)** — a revoked `token_id`'s live
  session is closed; any subsequent call on it returns `E_AUTH_REJECTED` and
  appends `auth_rejected`; revocation appends `token_revoked`; a connection with
  a wrong/unknown token is rejected at handshake with `E_AUTH_REJECTED`.
- **Hardening (CR-B-05)** — the §17.3 invariants hold: runtime dir `0700`,
  socket `0600`, bind refuses a symlinked/stale path, socket path is
  cwd-independent, DB/token/audit are `0600`, and no raw secret appears in
  `argv`/logs.
- **Prompt injection (mandatory category)** — the suite MUST include a
  prompt-injection test category that asserts: (a) a peer message attempting to
  approve a permission, change ACL/registry/tokens, or override system/developer
  instructions has no privileged effect and is delivered only as tainted peer
  data with the visible envelope; (b) a `task`/`admin_request` body that
  instructs a destructive tool call is still subject to the recipient's own
  permission model and is recorded in the causal chain; (c) no body is injected
  as a privileged system reminder or via `PostToolUse.additionalContext`.

---

## Deviations

Conflicts between this plan and the audit report are resolved in favour of the
audit. Deviations from the plan literal, and from earlier instructions, are
recorded here with rationale.

- **D1 — Tool surface extended beyond the plan's literal list (§5).** The plan's
  Section 5 list named `cell_send, cell_ask, cell_await, cell_inbox, cell_list`.
  This SPEC adds `cell_reply` (to cover the `reply` kind of §4 with a testable,
  single-reply tool) and `cell_broadcast` (admin-only; required by README's tool
  list and by the plan's own self-review note). Rationale: every message kind in
  §4 must have a deterministic producing tool; this is additive, not a conflict
  with any audit finding.
- **D2 — Error-code table extended (§13).** The plan named eight codes; this
  SPEC adds `E_ASK_NOT_FOUND`, `E_REPLY_EXISTS`, and `E_UNSUPPORTED_SPEC_VERSION`
  to make the ask/reply and handshake behaviors in §5/§6/§16 testable. Additive;
  consistent with F1.
- **D3 — `msg_type` semantic field added (§3, §4.1).** Per the security-posture
  addendum (point 4), the transport `kind` is unchanged
  (`send|ask|reply|broadcast|notice`); a new independent `msg_type`
  (`note|task|ask|reply|evidence|admin_request`) classifies the body's role.
  Required by the security posture review §3 (distinct message types with
  distinct affordances).
- **D4 — Rate limit is per-sender **and** per-capability (§14).** The plan
  specified only per-sender; the security-posture addendum (point 3, L0
  invariants) requires per-capability limits too.
- **D5 — Hash-chain key held by the daemon with separated permissions (§12.3).**
  Security-posture addendum L0 invariant; not in the original plan.
- **D6 — Identity contract §17 / Bus message trust §18 / Testing §19 added.**
  Imposed verbatim (§17 lead, §18 lead) plus the trait contract (§17.1) and L0
  invariants (§17.2) by the security-posture addendum. These sections prevail
  over any earlier wording in §1–§2 on the same subjects.
- **D7 — "Exactly once delivery is not promised" wording (§18).** The verbatim
  §18 block imposed by the addendum contains the phrase *"Exactly once delivery
  is not promised"* (with a space). This is retained verbatim as mandated. It is
  a negation, fully consistent with the standing rule that single-delivery is
  never promised. The forbidden-claims check remains keyed on the hyphenated
  positive-promise form (the token that asserts single-delivery as a guarantee),
  which does not occur in this document; the verbatim audit text is the
  authoritative exception.
- **D8 — Default-deny semantics clarified (§11).** The security posture review
  §3 suggested adding `ask` to an emphasized default-deny set. In this SPEC's
  model **every** capability is opt-in (a cell holds it only by being listed),
  so `ask` is already denied in the absence of an explicit grant. The emphasized
  default-deny set (§11.2) therefore stays as the four capabilities from finding
  F9, with no loss of coverage; this clarification is recorded here for the
  reviewer.
- **D9 — G1 gate fixes applied.** Following the G1 verdict (Fase 0 approved
  with modifications, same-UID L0 posture ratified D1), this SPEC integrates:
  `crew audit verify` (§15, CR-A-01/CR-B-05); new §17.3 hardening (CR-B-05);
  protected-cells minimal model `protected` + `send_to_protected`/`ask_protected`
  (§11.5, D2/CR-A-04/CR-B-01); `principal_capabilities` envelope field (§3,
  CR-A-08/CR-B-03); deadlock restricted to active `cell_await` + graph-poisoning
  mitigation (§6.4, CR-B-02); `expires_at` MUST clamp (§3.1, CR-B-06); revoke
  invalidates live sessions → `E_AUTH_REJECTED` (§17.2, CR-B-07); new
  `E_AUTH_REJECTED` (§13, CR-A-12); bus-induced/high-risk causal-chain
  definition (§12.3, CR-B-04); per-capability numeric defaults (§14, CR-B-08);
  `deliver` ≤ 250 ms (§10.1, CR-B-09); `idempotency_key` ≤ 128 bytes (§3.1,
  CR-B-10); `cell_await` bounds + immediate non-owned rejection (§5.3,
  CR-B-11); read tools cheap/bounded/unaudited-on-success (§5.8, CR-B-12);
  `file_refs` resolved with `O_NOFOLLOW` on the final component at read (§3.1,
  CR-B-13); registry invariants — rename = new identity, `registry_changed`
  (§11.6, CR-B-14); `cell_broadcast` wording "explicit `broadcast` grant
  (default-deny)", not "admin-only" (§1/§4/§5.7, CR-B-15); `cell_reply`
  capability-necessary-not-sufficient vs ownership (§5.4, CR-B-16). The verbatim
  §17 and §18 blocks from the security-posture review are imposed (D1); the sole
  permitted normalization (`capabilities` → `principal_capabilities`, task 10)
  is applied consistently in the §18 lead and §18.1 (no `capabilities` alias
  remains).
- **D10 — D2 protected cells replace a full per-recipient matrix.** CR-A-04
  (allowlist for `ask` toward high-privilege cells) is resolved with the minimal
  "protected cells" model (§11.5: flag + per-target default-deny grants) rather
  than a complete per-recipient ACL matrix — a DAG/coordinator decision (D2).
- **D11 — `E_AUTH_REJECTED` used for revoked sessions (CR-B-07 mapping).** Task
  CR-B-07 originally named `E_ACL_DENIED` for calls on a revoked session; this
  is mapped to `E_AUTH_REJECTED` (introduced by CR-A-12) for categorical
  consistency: a revoked token is an authentication failure (the proof no longer
  matches a valid identity), not an authorization failure. Both map to the
  existing `auth_rejected` audit event.
- **D12 — AUDIT2 round-2 gate fixes.** Following the final GPT-5.5 audit
  (external audit round 2, NEEDS_CHANGES, 2 MAJOR): (1) `cell_broadcast` now evaluates
  fan-out permission per recipient — a protected recipient is included only with
  a matching `send_to_protected` grant, otherwise omitted with a
  `protected_access_denied` event, and `broadcast` alone never delivers to a
  protected cell (§5.7, §11.5, §19.1 — G1-01); (2) §17.3 now norms `SO_PEERCRED`
  (H-05) and the dedicated daemon user (H-18) as MUST, with a single-UID
  developer deployment explicitly documented as an **L0-degraded mode** where
  H-18 is not satisfied (G1-02, preferred option ratified by the coordinator);
  (3) the stale `capabilities`-alias note was removed from §18.1 (editorial
  NIT — the §18 lead already says `principal_capabilities`).

## 20. Cell Fabric (Fase 2)

Normative extension: crewd as cross-CLI cell fabric. A primary CLI's AI
launches and follows workers on any backend via the MCP shim tools.

### 20.1 Cell model

- Cell = `{ name, engine, model?, profile?, cwd, worktree_default, memory_device?, created_at }`.
- **Named** cells live in the registry, are created/updated only via
  `admin_registry` capability (audited `cell_registered`/`cell_updated`), and
  are **immutable at launch**: any engine/model/profile/cwd override in
  `cell_spawn` targeting a named cell fails `E_POLICY_DENIED`.
- **Ephemeral** targets pass `engine` (+ optional model/profile/cwd) inline;
  cell_name is generated `~ephemeral-<uuid8>`.
- `memory_device` is informative in v0 (exposed in `cell_list`); the worker
  mounts its own mcp-memory-rs namespace. No active health check in v0.
- v0 invariants: single-host; **one active thread per named cell** (active =
  `spawning|running`); nested delegation OFF (§20.7).

### 20.2 Tables and identity domains

Five tables: `cells`, `cell_threads`, `cell_jobs`, `spawn_requests`,
`worktrees` (+ `thread_journal`, `cell_locks`). Identity domains are separate
and never interchangeable: `crewd_thread_id` (UUIDv7, ours) vs
`engine_process_id` vs `engine_thread_id` vs `engine_turn_id` vs
`engine_session_id`. `cell_result` exposes them as distinct fields.

### 20.3 State machines (three, independent)

- **ThreadState (7)**: `spawning, running, idle, interrupted, timeout,
  failed_unknown, done`. Allowed transitions: `spawning→running|failed_unknown`;
  `running→idle|interrupted|timeout|failed_unknown|done`; any of
  `idle|interrupted|timeout|failed_unknown|done→running` only via explicit
  follow-up (`cell_send_task`). No others.
- **JobState (5)**: `queued, leased, started, finished, cancelled`.
- **EngineProcState (2)**: `up, down`.

### 20.4 Fabric tools

`cell_spawn{cell?|engine?,model?,profile?,cwd?,worktree?,task,idempotency_key,mode}`
→ `{crewd_thread_id, replayed}` · `cell_send_task{crewd_thread_id,message,
idempotency_key}` → `{job_id}` · `cell_status{crewd_thread_id?}` ·
`cell_result{crewd_thread_id}` → §20.10 result · `cell_cancel{crewd_thread_id}`
· `cell_list` extended with `fabric` section. `mode ∈ {background,wait}`
default `background`; `wait` is an await on a persisted thread, it owns no
state.

### 20.5 Queue and redelivery boundary

Per-cell persistent FIFO with leasing (survives daemon restart). Queue depth
cap default 8 → `E_QUEUE_FULL`. Head-of-line blocking is accepted v0.
`accepted_by_engine_at` is the redelivery boundary: a leased job whose lease
expires **before** engine acceptance is re-queued; once accepted, **never**
auto-retried (agentic double-turn = damage) — only explicit re-submission.

### 20.6 Resume contract (honest)

Live reattach only while engine process and thread are alive. After engine
death, resume (`thread/resume` for codex, SDK `resume` for claude) is a
**follow-up on materialized history**, never completion of the lost turn. A
turn in flight at death becomes `interrupted` or `failed_unknown`. Engines
without session resume (pi v0) fail `E_THREAD_NOT_RESUMABLE`.

### 20.7 Permissions

- New capability **`spawn`** (default-deny) gates `cell_spawn`,
  `cell_send_task`, `cell_cancel`. `cell_status`/`cell_result` allowed to the
  thread's `created_by_principal` or holders of `admin_registry`.
- YOLO is explicit engine configuration, verified: codex adapter sends
  `approvalPolicy:"never"` + full-access sandbox at thread/start, thread/resume
  and turn/start and verifies the response; mismatch → `E_POLICY_DENIED`
  (fail-clear, never degrade).
- Nesting OFF: the MCP shim in `--worker-mode` does not expose
  `cell_spawn`/`cell_send_task`/`cell_cancel`.
- engine-claude env allowlist (exact): `ANTHROPIC_AUTH_TOKEN,
  ANTHROPIC_BASE_URL, ANTHROPIC_MODEL, ANTHROPIC_SMALL_FAST_MODEL,
  CLAUDE_CODE_AUTO_COMPACT_WINDOW, HOME, PATH, NODE_OPTIONS, TMPDIR, LANG,
  TERM`. Secrets never in argv/logs (prefix ≤8 chars in errors).

### 20.8 Worktrees

Ownership record in `worktrees` (state `created|active|cleanup_requested|
removed`) written **before** filesystem creation. Deletion only via explicit
audited command (`worktree_cleanup` event). Never automatic.

### 20.9 Timeouts

Per-turn timeout default 1800s, clamp max 7200s. On timeout: persist
`engine_turn_id` and outcome **before** best-effort interrupt/kill; thread →
`timeout`; audit `cell_timeout`.

### 20.10 Result, errors, audit

- Result: `{final_answer?, event_tail (≤50, seq-prefixed), artifact_refs[],
  exit_status ∈ {done,interrupted,timeout,failed_unknown,cancelled},
  crewd_thread_id, engine_thread_id?, engine_session_id?}`. The tail is not
  the result.
- New error codes (7): `E_CELL_BUSY, E_QUEUE_FULL, E_ENGINE_DOWN,
  E_THREAD_NOT_RESUMABLE, E_POLICY_DENIED, E_TIMEOUT, E_CANCELLED`.
- New audit event kinds (12): `cell_registered, cell_updated,
  cell_spawn_requested, cell_turn_started, cell_turn_completed,
  cell_turn_failed, cell_cancelled, cell_timeout, engine_started,
  engine_stopped, worktree_created, worktree_cleanup`. Boot recovery of
  orphaned `running` threads (daemon restart) is audited as `cell_turn_failed`
  with `detail.reason="boot_recovery"` — no dedicated kind. Audit-before-mutation:
  `cell_spawn_requested` is written+fsynced before any consumable record,
  worktree or process is created.
