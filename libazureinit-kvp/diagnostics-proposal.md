# KVP Diagnostics Proposal

## Purpose

Hyper-V KVP is a guest-to-host exchange of physical key/value records. Each record has a size limit. KVP preserves records but does not define diagnostics, splitting, reconstruction, or payload formats.

Azure-init and cloud-init are guest provisioning agents that write diagnostic records to KVP. This proposal defines one way to read both formats and one Azure-init format for new writes.

This proposal leaves KVP unchanged and changes the diagnostic view:

- The default result is one entry per recognized physical diagnostic record, with its raw key and value unchanged.
- Splitting and grouping records is separate from diagnostic meaning.
- Message reconstruction and content decoding happen only when requested.

## Architecture

### Existing foundation: `KvpPoolStore`

`KvpPoolStore` reads and writes one Hyper-V KVP pool. It returns physical key/value records exactly as stored and has no diagnostic meaning.

### Layer 1: indexed values

A value that exceeds one KVP record is split across records. Each part uses the same base key plus a numeric suffix:

```text
<base-key>|0 = first value fragment
<base-key>|1 = second value fragment
```

Indexed framing splits values for writes and identifies, groups, and orders parts for reconstruction. It understands only a caller-defined base key, index, and value fragments. It does not understand diagnostic fields, JSON, or compression.

Indexed framing is decoupled from diagnostics in the code, but it is not a separate public API. Callers use raw KVP or diagnostics; nothing else needs to span values today.

### Layer 2: diagnostics

Diagnostics defines the base-key formats and how ordered source values become a logical message. It uses three components:

#### `DiagnosticKey`: normalized metadata

`DiagnosticKey` contains diagnostic metadata parsed from one Azure-init or cloud-init key:

```text
DiagnosticKey {
  agent, boot_epoch, vm_id?, kind, name,
  event_id, timestamp?, chunk_index?
}
```

Its purpose is to give both source formats one common key model.

#### `DiagnosticEntry`: one physical diagnostic record

`DiagnosticEntry` is the default unit returned by a diagnostic read:

```text
DiagnosticEntry {
  raw_key,
  key: DiagnosticKey,
  value
}
```

The raw key and value remain unchanged. `DiagnosticKey` provides the normalized metadata. A value split into four KVP records produces four `DiagnosticEntry` values.

#### `DiagnosticsPool`: diagnostic access

`DiagnosticsPool` is the diagnostic interface composed over one `KvpPoolStore` and the indexed framing rules.

On read, it returns one `DiagnosticEntry` per recognized physical record, in pool order. A record is recognized when its key matches the Azure-init or cloud-init layout; other keys, such as `PROVISIONING_REPORT`, are skipped in the diagnostic view but remain visible through `dump`. Optional fields (`vm_id`, `timestamp`, `chunk_index`) are omitted from output when absent rather than shown as empty. On write, it creates Azure-init keys, uses indexed framing when a value must be split, and writes the resulting records through `KvpPoolStore`.

```text
Read:
KvpPoolStore -> physical records -> DiagnosticsPool -> DiagnosticEntry values

Write:
caller -> DiagnosticsPool -> indexed framing -> KvpPoolStore
```

## Reading a value back: Raw, Message, Content

A diagnostic value can be wrapped up to three times before it lands in KVP:

1. Split — a value larger than one record is spread across several physical records (indexed values).
2. Wrapped — the producer stores the message inside its own structure, such as cloud-init's JSON `{"...","msg":"..."}`.
3. Encoded — the message itself may be compressed and base64-encoded, such as a captured `dmesg` or `cloud-init.log`.

Reading a value back means choosing how far to unwrap it. The three views are read-time transformations, not stored data:

| View | Unwraps | Result |
|---|---|---|
| Raw | Nothing | Each physical record exactly as stored |
| Message | Split and producer wrapper | The producer's logical message |
| Content | Split, wrapper, and encoding | The decoded, decompressed payload |

### Example: a compressed `dmesg`

Stored as two physical records. Each is a cloud-init JSON wrapper carrying one fragment of the message:

```text
Raw:
  CLOUD_INIT|...|compressed|dmesg|...|0 = {"...","msg_i":0,"msg":"{\"encoding\":\"gz+b64\",\"da"}
  CLOUD_INIT|...|compressed|dmesg|...|1 = {"...","msg_i":1,"msg":"ta\":\"H4sIA...\"}"}
```

Reassemble the fragments into the producer's message. For a compressed event that message is an encoding envelope, not yet readable text:

```text
Message:
  {"encoding":"gz+b64","data":"H4sIA..."}
```

Decode and decompress the envelope to get the actual artifact:

```text
Content:
  [    0.000000] Linux version 6.8.0-azure ...
```

### First-class encoding mechanism

Undoing the split and the producer wrapper is mechanical. Decoding is not: it needs a defined envelope, a codec, and a decompression step. Today no supported operation does this, so a compressed diagnostic can only be read back as an opaque `{encoding,data}` blob. The proposal is to make decoding a first-class, explicitly requested step — the Content view — driven by a validated `{encoding,data}` envelope in the value rather than guessed from the key.

For any value that is not encoded there is nothing to decode, so Message and Content are identical:

```text
Raw:     <base-key>|0 = "Retrieved ", <base-key>|1 = "1 key from IMDS"
Message: "Retrieved 1 key from IMDS"
Content: "Retrieved 1 key from IMDS"   (no envelope, nothing to decode)
```

Raw returns one result per physical record. Message and Content return one result per event, that is, per set of records sharing a base key. Both require a contiguous run of parts beginning at `chunk_index` 0; if the first or a middle part is missing, no message is produced and the available parts remain as Raw entries. The suffix format has no total-part count, so a missing final part cannot always be detected; reconstruction of existing records is therefore best effort.

## Normalizing Azure-init and cloud-init

Azure-init keys:

```text
<agent>|<boot_epoch>|<vm_id>|<kind>|<name>|<event_id>|<timestamp>[|<chunk_index>]
```

Current cloud-init keys:

```text
CLOUD_INIT|<boot_epoch>|<type>|<name>|<vm_id>|<event_id>[|<chunk_index>]
```

Older cloud-init keys omit `vm_id`:

```text
CLOUD_INIT|<boot_epoch>|<type>|<name>|<event_id>[|<chunk_index>]
```

Both formats map to `DiagnosticKey`:

| Field | Meaning and use | Azure-init source | Cloud-init source |
|---|---|---|---|
| `agent` | Producer and key namespace | First key field | `CLOUD_INIT` |
| `boot_epoch` | Separates records from different VM boots | Key | Key |
| `vm_id` | Identifies the VM when records are exported or aggregated | Key | Key or absent |
| `kind` | Lifecycle role of the record | Key `kind` | Derived from key `type` |
| `name` | Producer-defined operation or subject used for filtering | Key | Key |
| `event_id` | Correlates records chosen by the producer; all parts of one value share it | Key | Key |
| `timestamp` | Time the occurrence was reported | Key (always present) | Best-effort `ts` from value; omitted if missing or unparsable |
| `chunk_index` | Orders physical parts of one value | Numeric suffix (always present) | Numeric suffix, present only when split |

`DiagnosticEntry.raw_key` preserves source details that are not in the common model. `DiagnosticEntry.value` preserves the producer-owned message or structured payload.

## What `kind`, cloud-init `type`, and `name` mean

`kind` answers “what role does this record have?” `name` answers “what is this record about?”

| Common `kind` | Meaning |
|---|---|
| `Start` | The operation identified by `name` began |
| `Finish` | The operation identified by `name` ended |
| `Diagnostic` | A point-in-time observation about `name` |

The same name can appear with different lifecycle roles:

| `kind` | `name` | What the record answers |
|---|---|---|
| `Start` | `provision:run` | When did provisioning begin? |
| `Diagnostic` | `provision:run` | What was observed during provisioning? |
| `Finish` | `provision:run` | When did provisioning end? |
| `Diagnostic` | `dmesg` | What `dmesg` data was reported? |

The source prefix selects the Azure-init or cloud-init key layout. Common `kind` does not select a key layout, rename or parse `name`, or define the value format. Results, durations, messages, and other producer-defined details remain in the value.

Azure-init writes `start`, `finish`, and `diagnostic` directly and maps them to `Start`, `Finish`, and `Diagnostic`.

Cloud-init calls its broader source classification `type`. Some values describe lifecycle, some describe a data category, and `compressed` describes representation. They normalize as follows:

| Cloud-init `type` | Source meaning | Common `kind` |
|---|---|---|
| `start` | Named operation began | `Start` |
| `finish` | Named operation ended | `Finish` |
| `event` | Legacy point event | `Diagnostic` |
| `diagnostic` | Diagnostic point event | `Diagnostic` |
| `system-info` | System, OS, or agent information | `Diagnostic` |
| `boot-telemetry` | Boot timing or boot information | `Diagnostic` |
| `compressed` | Legacy label for encoded diagnostic data | `Diagnostic` |
| Any other value | Producer-defined or unknown classification | `Diagnostic` |

Only `start` and `finish` have common lifecycle meaning. Every other source type is a point-in-time `Diagnostic`. The original cloud-init `type` remains in `raw_key`, but it does not become another common field under this proposal.

## Read and write example

Write one Azure-init point diagnostic:

```text
libazureinit-kvp emit --prefix azure-init --vm-id vm-123 --name imds --message "Retrieved 1 key from IMDS"
```

The command generates the boot epoch, event ID, and timestamp and writes:

```text
azure-init|1788371515|vm-123|diagnostic|imds|event-1|2026-09-02T17:52:25Z|0
value: Retrieved 1 key from IMDS
```

An exact KVP read returns only the raw value:

```text
Retrieved 1 key from IMDS
```

The default diagnostic view returns one entry:

```text
raw_key="azure-init|1788371515|vm-123|diagnostic|imds|event-1|2026-09-02T17:52:25Z|0"
kind=Diagnostic agent=azure-init boot_epoch=1788371515 vm_id=vm-123 name=imds event_id=event-1 timestamp=2026-09-02T17:52:25Z chunk_index=0
value="Retrieved 1 key from IMDS"
```

A longer value may produce two entries:

```text
<same-base-key>|0 = "Retrieved "
<same-base-key>|1 = "1 key from IMDS"
```

Raw returns both entries. Message returns `Retrieved 1 key from IMDS`. If part 0 is missing, part 1 remains visible and no reconstructed message is claimed.

## Proposed commands

| Command | Result |
|---|---|
| `read <key>` | Raw value for one exact KVP key |
| `dump` | Every physical KVP record in pool order |
| `dump --parse-diagnostics` | Raw diagnostic view: one entry per recognized physical record |
| `dump --parse-diagnostics --view message` | Reconstructed Message view |
| `dump --parse-diagnostics --view content` | Decoded Content view |

`--name` filters by diagnostic name and `--tail` counts physical records. Both select records before any `--view` transformation, so Message and Content are built from the selected records. JSON output keeps the original value as a string.

## Feedback requested

Please confirm these four design choices.

### 1. Common `kind` versus source `type`

Proposed: `DiagnosticKey` exposes only `Start`, `Finish`, or `Diagnostic`. The exact cloud-init `type` remains available in `raw_key` but does not control `name`, key parsing, or value parsing.

Effect: A consumer that wants to distinguish `system-info`, `boot-telemetry`, or another cloud-init classification must inspect the source key.

Alternative: add `source_type` to `DiagnosticKey`. Is that distinction important enough to expose directly, or is the normalized lifecycle view sufficient?

### 2. Default read unit

Proposed: `dump --parse-diagnostics` returns one `DiagnosticEntry` for every recognized physical record. Message and Content are explicit views.

Effect: partial payloads remain inspectable. Callers that want one reconstructed event must request it. Filters and tail counts apply to physical records in the default view.

Alternative: continue returning reconstructed events by default, which is simpler for complete data but can hide incomplete records. Which result should be the primary contract?

### 3. Transformation failure

Proposed: if Message reconstruction or Content decoding fails, return the available Raw entries.

Effect: no physical record disappears because a higher-level view could not be produced, and callers can still inspect the source data.

Alternative: return an error for the requested view. Should failure fall back to Raw entries or fail the request?

### 4. Encoding as a first-class step

Proposed: decoding is an explicit operation (the Content view), directed by a validated `{encoding,data}` envelope in the reconstructed value rather than guessed from the key.

Effect: encoding remains independent of lifecycle `kind`. Existing cloud-init `type=compressed` remains readable, but new Azure-init data does not need a `compressed` kind merely to carry encoded content.

Alternative: inspect the envelope only when a source token such as `type=compressed` indicates encoded content. That would require exposing or deriving that hint and deciding whether Azure-init also writes `compressed`. Should encoding be payload-directed or source-token-gated? A related question is whether to keep Raw, Message, and Content as three views or collapse Message and Content into a single Decoded view, since they differ only for encoded payloads.
