# KVP Diagnostics Proposal

## Summary

The base proposal is a typed view of physical KVP records.

Every recognized Azure-init or cloud-init record produces one `DiagnosticEntry`. Its key is normalized into `DiagnosticKey`, and its physical value is returned unchanged. Reading diagnostics does not automatically group chunks, extract messages, or decompress content.

## Architecture

`KvpPoolStore` is the existing foundation. Above it, the proposal separates the two concerns that are currently combined:

1. Spanned values handle `base-key|chunk-index`, splitting, grouping, and ordering. They do not know what the key or value means.
2. Diagnostics defines the base key, normalizes its fields, and knows how source values would be combined when payload decoding is requested.

These are separate responsibilities, but spanned values are not a new public API. The public API remains `KvpPoolStore` for raw KVP and `DiagnosticsPool` for diagnostics. `DiagnosticKey` and `DiagnosticEntry` are the public data model returned by the diagnostics layer.

### API layers

| Component | Input and output | Responsibility | Why it exists |
|---|---|---|---|
| `KvpPoolStore` | Reads and writes physical key/value records | Preserve the KVP pool exactly, in pool order | Existing general-purpose KVP access with no diagnostic meaning |
| `DiagnosticsPool` | Uses one `KvpPoolStore`; returns `DiagnosticEntry` values and writes Azure-init diagnostics | Recognize diagnostic keys, normalize their metadata, and preserve each physical value | Give callers one diagnostic view across Azure-init and cloud-init |

`KvpPoolStore` owns storage; `DiagnosticsPool` owns diagnostic meaning and delegates all physical reads and writes to the store. Raw KVP callers use the store directly. Diagnostic callers use the pool layered over it.

### Internal operations

| Operation | Used when | Responsibility | Not responsible for |
|---|---|---|---|
| Key normalization | Every diagnostic read | Convert either source key into `DiagnosticKey` and best-effort obtain cloud-init's timestamp | Grouping chunks or interpreting payload content |
| Chunk framing | Writing a long value | Split the value and add numeric key suffixes | Diagnostic meaning |
| Chunk grouping | Payload decoding is explicitly requested | Group, validate, and order entries from one event | The default physical-entry read |
| Payload transformation | Message or Content is explicitly requested | Extract a logical message and optionally decode compressed content | Key parsing or timestamp extraction |

Chunk framing and grouping remain internal. They are separate from diagnostic meaning, but there is no separate public indexed-value API in this proposal.

`DiagnosticKey` and `DiagnosticEntry` are data models within `DiagnosticsPool`, not additional architecture layers.

### How the pieces interact

Default read:

```text
KvpPoolStore.dump()
  -> one physical key/value record
  -> DiagnosticsPool normalizes its key
  -> one DiagnosticEntry with the unchanged physical value
```

This repeats independently for every record. No grouping occurs.

Write:

```text
caller
  -> DiagnosticsPool creates an Azure-init key and value
  -> internal chunk framing splits the value when necessary
  -> KvpPoolStore appends the physical record or records
```

Optional payload decoding:

```text
DiagnosticEntry values
  -> internal chunk grouping
  -> source-specific message reconstruction
  -> optional decoding and decompression
```

This optional path transforms payloads only. It does not change the default entries or the metadata already extracted from their keys.

## Base public model

```text
DiagnosticKind = Start | Finish | Diagnostic

DiagnosticKey {
  agent,
  boot_epoch,
  vm_id?,
  kind,
  name,
  event_id,
  timestamp?,
  chunk_index?
}

DiagnosticEntry {
  key: DiagnosticKey,
  value
}
```

`DiagnosticKey` describes the record. `DiagnosticEntry` combines that description with the unchanged physical value.

| Field | Required | Source | Meaning and concrete use |
|---|---|---|---|
| `agent` | Yes | Key | Producer of the record; distinguishes `CLOUD_INIT` from Azure-init |
| `boot_epoch` | Yes | Key | Boot that produced the record; separates current-boot from previous-boot data |
| `vm_id` | No | Key | VM identity; supports host-side collection while remaining compatible with older cloud-init keys that omit it |
| `kind` | Yes | Key `type` token | Common lifecycle meaning; tells a reader whether `provision:run` began, ended, or emitted a point observation |
| `name` | Yes | Key | Actual producer-defined subject; lets a caller filter for `dmesg` or `provision:run` without parsing the value |
| `event_id` | Yes | Key | Identity shared by physical chunks from one emission; associates those chunks without inspecting their values |
| `timestamp` | No | Azure-init key or cloud-init `ts` | Time the occurrence was reported; remains absent when cloud-init metadata is unreadable |
| `chunk_index` | No | Numeric key suffix | Position of this physical fragment; makes chunks `16` through `19` useful even when earlier chunks are gone |
| `value` | Yes | Physical KVP value | Original fragment exactly as stored; preserves incomplete or invalid JSON for inspection |

The optional fields are optional in the common model because the supported source formats do not always provide them. Their absence never causes an otherwise valid physical entry to be discarded.

## Key normalization

Azure-init keys:

```text
<agent>|<boot_epoch>|<vm_id>|<type>|<name>|<event_id>|<timestamp>[|<chunk_index>]
```

Current cloud-init keys:

```text
CLOUD_INIT|<boot_epoch>|<type>|<name>|<vm_id>|<event_id>[|<chunk_index>]
```

Older cloud-init keys omit `vm_id`:

```text
CLOUD_INIT|<boot_epoch>|<type>|<name>|<event_id>[|<chunk_index>]
```

All three layouts map into the same `DiagnosticKey`.

Azure-init provides its timestamp in the key. Cloud-init provides `ts` in the value, so `DiagnosticsPool` reads only enough metadata to find it. If `ts` is missing or unreadable, `timestamp` is absent and the entry is still returned.

## What `type` and `name` mean

The source key contains a `type` token and a `name`.

- `name` answers: what is this record about?
- `type` answers: what did the source report about that name?

The base proposal retains only the common meaning needed by readers: `kind`. It does not retain the exact source type as another public field.

Under this proposal, `type` has a narrow key-level role:

- It does not select the Azure-init or cloud-init key layout; the key shape and source prefix do that.
- It does not define, parse, or validate the client-owned value during the default read.
- It derives `kind`, so lifecycle can be understood without parsing the value.


| Source `type` | Common `kind` | Meaning |
|---|---|---|
| `start` | `Start` | The operation identified by `name` began |
| `finish` | `Finish` | The operation identified by `name` ended |
| Any other type, including `compressed` | `Diagnostic` | A point-in-time observation about `name` |

`type` does not rename or parse `name`. For example:

```text
type=start       name=provision:run     -> provisioning began
type=finish      name=provision:run     -> provisioning ended
type=system-info name=system information -> system information was reported
type=compressed  name=dmesg             -> dmesg data was reported
```

The common results are:

```text
Start       name=provision:run
Finish      name=provision:run
Diagnostic name=system information
Diagnostic name=dmesg
```

Existing `event`, `diagnostic`, `system-info`, `boot-telemetry`, and unknown types all become `Diagnostic`. Their actual `name` remains unchanged. Existing Azure-init `event` records remain readable; new Azure-init point records use `diagnostic`.

## Default read contract

`DiagnosticsPool::entries()` returns one `DiagnosticEntry` for each recognized physical record, in pool order.

It does:

- Parse key metadata into `DiagnosticKey`.
- Best-effort extract cloud-init's `ts`.
- Preserve the physical value unchanged.
- Return duplicate, partial, and out-of-sequence chunks independently.

It does not:

- Group or order chunks.
- Require a complete chunk sequence.
- Validate cloud-init `msg_i`.
- Extract or unescape `msg`.
- Extract `result` or `duration`.
- Decode or decompress content.

Example: if only `dmesg` chunks 16 through 19 remain, the read returns four entries:

```text
Diagnostic name=dmesg chunk_index=16 value=<raw chunk 16>
Diagnostic name=dmesg chunk_index=17 value=<raw chunk 17>
Diagnostic name=dmesg chunk_index=18 value=<raw chunk 18>
Diagnostic name=dmesg chunk_index=19 value=<raw chunk 19>
```

Missing chunks 0 through 15 do not hide the records that are present. A cloud-init chunk also remains visible when a split inside `msg` makes that individual value invalid JSON.

## Write contract

`DiagnosticsPool` writes Azure-init records only.

- A point diagnostic uses the `diagnostic` token and generates its event ID and timestamp.
- Explicit writes may use `Start`, `Finish`, or `Diagnostic` with caller-supplied event metadata.
- Values larger than one record are split at safe text boundaries and written with numeric chunk suffixes.
- All chunks from one write are appended together.

Example:

```text
emit diagnostic: name=imds, value="Retrieved 1 key from IMDS"

azure-init|1788371515|vm-123|diagnostic|imds|event-1|2026-09-02T17:52:25Z|0
= Retrieved 1 key from IMDS
```

A long value produces multiple physical records. The default read returns each one as a separate `DiagnosticEntry`.

## CLI contract

`dump --parse-diagnostics` means classify physical diagnostic records.

Each output item contains the fields from `DiagnosticKey` plus the unchanged physical value.

- `--name` filters by the actual `name`.
- `--tail` counts physical records.
- JSON output represents the KVP value as a string, even when that string contains JSON.
- Decoded `message`, `result`, and `duration` are not part of this default output.

## Separate proposal: first-class payload decoding

The base design above always returns physical entries. Some callers may also need one logical message or the decompressed artifact contained by that message. That transformation should be explicit and must not change key parsing or timestamp extraction.

There are three possible levels:

| Level | Result |
|---|---|
| Raw | Every physical `DiagnosticEntry`, unchanged |
| Message | Group a complete chunk sequence and reconstruct the logical message |
| Content | Perform Message processing, then decode compressed content |

For Azure-init, Message concatenates value chunks. For cloud-init, Message joins the `msg` fragments and removes the outer telemetry JSON.

For an ordinary diagnostic:

```text
Raw:     {"name":"imds","msg":"Retrieved 1 key from IMDS",...}
Message: Retrieved 1 key from IMDS
Content: Retrieved 1 key from IMDS
```

For a complete compressed `dmesg` event:

```text
Raw:     separate physical cloud-init chunks
Message: {"encoding":"gz+b64","data":"..."}
Content: decompressed dmesg bytes or text
```

The base `DiagnosticKey` does not identify encoded content. If Content decoding is approved, one of these signals must also be selected:

| Option | Design | Effect |
|---|---|---|
| Key-derived flag | Add `compressed` to `DiagnosticKey`, derived from `type=compressed` | Identifies partial chunks before reconstruction, but gives one source token special public meaning |
| Complete source type | Add `source_type` to `DiagnosticKey` | Preserves `compressed` and every other producer classification, but expands the common model |
| Reconstructed envelope | Keep the base model unchanged and inspect the completed `{encoding,data}` message | Keeps encoding in the payload, but cannot identify incomplete chunks as encoded |

In all three options, a valid envelope determines the codec. The signal only determines whether Content decoding should be attempted. Content then base64-decodes the data and applies the declared decompression.

If a requested transformation cannot be completed, the proposed behavior is to return the original Raw entries rather than discard them.

## Feedback requested

Sign-off is needed on both the base physical-entry design and the optional payload-decoding mechanism.

“First-class” means callers select one named payload level. They do not manually combine separate grouping, message-extraction, base64, and decompression flags. The proposed public shape is:

```text
Default API read:  entries() -> Raw DiagnosticEntry records
Optional API read: payload view = Message | Content

CLI:
  dump --parse-diagnostics                 -> Raw
  dump --parse-diagnostics --view message  -> Message
  dump --parse-diagnostics --view content  -> Content
```

Message and Content automatically perform their required earlier stages. This public control is part of the proposal and also requires approval.

1. Base design: Does one physical `DiagnosticEntry` per record, with the model and normalization above, match the intended diagnostics contract? Does keeping spanned-value handling separate but internal, with diagnostics layered on top of it, match the intended architecture?

2. Purpose of `type`: Should `type` remain a key field whose base public effect is deriving `kind`, while the value schema remains client-owned? Should lifecycle continue to use `type=start` and `type=finish` in the key, or should Start and Finish use a different key structure?

3. Decoding levels: Should the first-class mechanism expose Raw, Message, and Content, or only Raw and fully decoded Content?

4. Public control: Should `entries()` remain the unchanged Raw read while one separate payload-view operation and matching `--view` option select Message or Content? If only Raw and Content are needed, should the option be named `--decode` instead of `--view content`?

5. Decoded content type: Should Content return bytes, or require UTF-8 text?

6. Failure behavior: When chunks are incomplete or decoding fails, should the operation return the original Raw entries or an error?

7. Encoded-content signal: If Content decoding is approved, should it use a key-derived `compressed` flag, retain the complete `source_type`, or inspect only the reconstructed `{encoding,data}` envelope? Is this proposal limited to decoding existing encoded content, or should it also define how Azure-init compresses and writes new encoded content?