# KVP Diagnostics Proposal

## What changes

Hyper-V KVP stores physical key/value records. It does not define chunking, diagnostics, or payload formats. This proposal does not change KVP.

Today, the diagnostic view reconstructs complete events and may omit incomplete groups. The proposal changes the default diagnostic view to return every recognized physical record with parsed metadata and its original value. Reconstruction and content decoding become optional.

The reader supports existing Azure-init and cloud-init records. The writer produces Azure-init records only.

## Components and composition

| Component | Composed from | Responsibility |
|---|---|---|
| `KvpPoolStore` | One Hyper-V KVP pool | Read and write physical key/value records without diagnostic meaning |
| Indexed framing | A base key and optional numeric `|index` suffixes | Split values for writing; group and order records only when reconstruction is requested |
| `DiagnosticsPool` | One `KvpPoolStore` plus Azure-init writer identity | Read both source formats, write Azure-init diagnostics, and expose diagnostic entries |
| `DiagnosticKey` | Metadata parsed from one Azure-init or cloud-init key | Provide one source-independent key model |
| `DiagnosticEntry` | Raw key, `DiagnosticKey`, and unchanged value | Represent one physical diagnostic record |
| Message and Content views | One or more `DiagnosticEntry` values | Optionally reconstruct a message and decode its declared content encoding |

`DiagnosticsPool` is a diagnostic view over an existing `KvpPoolStore`; it does not replace the store or create another persisted format. It recognizes the Azure-init and cloud-init key layouts and converts either one into `DiagnosticKey`.

Indexed framing is an internal rule, not diagnostic metadata. It understands only a base key, an index, and value fragments. It does not understand `kind`, `name`, JSON, or compression.

```text
Read:
KvpPoolStore
  -> physical records
  -> DiagnosticsPool parses each source key
  -> DiagnosticEntry values                         [default]
  -> indexed framing groups and orders entries      [Message requested]
  -> DiagnosticsPool reconstructs the source value
  -> encoding envelope is decoded                   [Content requested]

Write:
caller
  -> DiagnosticsPool creates an Azure-init base key
  -> indexed framing splits the value and adds indexes
  -> KvpPoolStore writes the physical records
```

The default `DiagnosticsPool` read returns one `DiagnosticEntry` for each recognized physical record; it does not group records. Its point-diagnostic write generates the event ID and timestamp. A lifecycle-aware write accepts `Start`, `Finish`, or `Diagnostic` and the caller's event metadata.

Raw KVP commands use `KvpPoolStore` directly. Parsed diagnostic commands use `DiagnosticsPool`. Message reconstruction and content decoding are requested views, not additional storage layers or stored data structures.

## One normalized `DiagnosticKey`

New Azure-init records use:

```text
<agent>|<boot_epoch>|<vm_id>|<kind>|<name>|<event_id>|<timestamp>[|<index>]
```

Existing cloud-init records use:

```text
CLOUD_INIT|<boot_epoch>|<type>|<name>|<vm_id>|<event_id>[|<index>]
CLOUD_INIT|<boot_epoch>|<type>|<name>|<event_id>[|<index>]
```

Both formats produce the same key model:

```text
DiagnosticKey {
  agent, boot_epoch, vm_id?, kind, name,
  event_id, timestamp?, chunk_index?
}

DiagnosticEntry { raw_key, key: DiagnosticKey, value }
```

| `DiagnosticKey` field | Azure-init source | Cloud-init source |
|---|---|---|
| `agent` | First key field | `CLOUD_INIT` |
| `boot_epoch` | Key | Key |
| `vm_id` | Key | Key, or absent in the older layout |
| `kind` | Derived from key `kind` | Derived from key `type` |
| `name` | Key | Key |
| `event_id` | Key | Key |
| `timestamp` | Key | Best-effort `ts` from the value |
| `chunk_index` | Trailing numeric suffix | Trailing numeric suffix |

The value is preserved unchanged. In the default view, cloud-init value parsing is used only to obtain its timestamp. Message and Content views interpret more of the value only when requested.

| Field | Meaning and use | Example |
|---|---|---|
| `raw_key` | Preserve the exact source key for compatibility and inspection | Original `CLOUD_INIT|...` key |
| `agent` | Identify the producer and its key namespace | `CLOUD_INIT`, `azure-init` |
| `boot_epoch` | Identify when the VM boot began so records from different boots can be separated | `1788371515` |
| `vm_id` | Identify the VM when records are exported or aggregated; absent from older cloud-init keys | `vm-123` |
| `kind` | State the record's lifecycle role | `Start`, `Finish`, `Diagnostic` |
| `name` | Identify the producer-defined operation or subject for filtering | `provision:run`, `dmesg` |
| `event_id` | Identify one logical emission and tie all of its physical parts together | `event-1` |
| `timestamp` | State when the occurrence was reported; optional when unavailable | `2026-09-02T17:52:25Z` |
| `chunk_index` | Order the physical parts of one value | `0`, `1`, `2` |
| `value` | Preserve the producer-owned message or structured payload | `Retrieved 1 key from IMDS` |

An `event_id` always groups parts of one emission. It correlates separate `Start` and `Finish` records only when the producer explicitly guarantees that convention.

## `kind` compared with `name`

`kind` answers “what role does this record have?” `name` answers “what is this record about?”

| `kind` | `name` | Meaning |
|---|---|---|
| `Start` | `provision:run` | The named operation began |
| `Diagnostic` | `provision:run` | A point-in-time observation about the operation |
| `Finish` | `provision:run` | The named operation finished |
| `Diagnostic` | `dmesg` | Point-in-time diagnostic data named `dmesg` |

`kind` does not rename `name` and does not select a value parser. Results, durations, and other details remain in the producer-owned value.

Azure-init calls its classification `kind`; cloud-init calls it `type`. Each known token means:

| Source token | Exact source meaning | Common `kind` |
|---|---|---|
| `start` | The operation identified by `name` began | `Start` |
| `finish` | The operation identified by `name` ended | `Finish` |
| `event` | Legacy point-in-time event | `Diagnostic` |
| `diagnostic` | Point-in-time diagnostic observation | `Diagnostic` |
| `system-info` | System, OS, or agent information | `Diagnostic` |
| `boot-telemetry` | Boot timing or boot-related information | `Diagnostic` |
| `compressed` | Legacy cloud-init label for an encoded diagnostic payload | `Diagnostic` |
| Any other value | Producer-defined or unknown classification | `Diagnostic` |

The token changes the record's lifecycle meaning only for `start` and `finish`. It never changes how `name` is interpreted. The original source token remains available in `raw_key`; it does not select a value parser in the common model. In particular, compression is a payload encoding, not a lifecycle kind.

## Proposed read behavior

| Command | Result |
|---|---|
| `read <key>` | The raw value for that exact KVP key |
| `dump` | Every physical key/value record in pool order |
| `dump --parse-diagnostics` | One parsed record per recognized physical record, including its chunk index and unchanged value |
| `dump --parse-diagnostics --view message` | Reconstruct the source message when its indexed records are available |
| `dump --parse-diagnostics --view content` | Decode a validated encoding envelope such as `{"encoding":"gz+b64","data":"..."}` |

Raw parsed diagnostics are the default. `--name` filters by diagnostic name, and `--tail` counts physical records in this view. JSON output keeps the original value as a string.

If Message or Content processing fails, that request falls back to the available Raw records rather than hiding them.

The existing suffix format has no total-part count. Gaps and duplicate indexes can be detected, but a missing final part cannot always be detected; reconstruction of existing records is therefore best effort.

## End-to-end example

Write one Azure-init point diagnostic:

```text
libazureinit-kvp emit --prefix azure-init --vm-id vm-123 --name imds --message "Retrieved 1 key from IMDS"
```

The command uses the supplied agent and VM ID, generates the boot epoch, event ID, and timestamp, then writes:

```text
azure-init|1788371515|vm-123|diagnostic|imds|event-1|2026-09-02T17:52:25Z|0
value: Retrieved 1 key from IMDS
```

An exact-key read returns:

```text
Retrieved 1 key from IMDS
```

A parsed diagnostic read returns:

```text
kind=Diagnostic agent=azure-init boot_epoch=1788371515 vm_id=vm-123 name=imds event_id=event-1 timestamp=2026-09-02T17:52:25Z chunk_index=0 value="Retrieved 1 key from IMDS"
```

A long value may produce two physical records:

```text
<same-base-key>|0 = "Retrieved "
<same-base-key>|1 = "1 key from IMDS"
```

The default diagnostic view returns both records. Message view returns one value:

```text
Retrieved 1 key from IMDS
```

If part 0 is missing, part 1 remains visible in the default view and no reconstructed message is claimed.

Existing cloud-init records follow the same read contract. Their key metadata is normalized, while their original key and JSON value remain unchanged.

## Feedback requested

Please confirm these four decisions:

1. Keep physical KVP, indexed framing, and diagnostics as separate concepts, while leaving indexed framing internal for now.
2. Use only `Start`, `Finish`, and `Diagnostic` in the common model. Preserve cloud-init's exact `type` in the raw key rather than assigning it cross-source semantics.
3. Make one physical diagnostic record the default result and request higher-level processing with `--view message` or `--view content`.
4. Treat encoding as a validated value envelope rather than `type=compressed`; if a higher-level view fails, preserve the available physical records.
