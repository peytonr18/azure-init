# KVP Diagnostics Proposal

## Goal

Hyper-V KVP stores physical key/value records that a guest exposes to its host. Azure-init and cloud-init use those records for diagnostics.

The proposal separates generic indexed values from diagnostic meaning, keeps each physical record available by default, and interprets payloads only when explicitly requested.

## Decisions requested

Please provide feedback on three decisions:

1. **Layer boundary:** Keep KVP storage and diagnostics as the public layers. Keep indexed-value framing as a separate internal component until another use case needs it directly. Is that the intended separation?
2. **Type and name:** The proposal gives `type` first-class key-level meaning: it describes what is reported about the subject identified by `name`, while a common `kind` is derived from it. Should `type` have this independent meaning, or should the common diagnostic view expose only `kind` and `name` and leave the producer's exact classification in the raw key or value?
3. **Encoded content:** When content decoding is explicitly requested, choose one approach:
   - **A — key-gated:** inspect an encoding envelope only when `type=compressed` indicates that encoded content is expected.
   - **B — payload-directed:** inspect every requested payload for a validated `{encoding,data}` envelope.

Under both approaches, the envelope determines the codec and ordinary reads do not decode content. If A is selected, should Azure-init also write `type=compressed`, and is the exact `type` sufficient or is a derived payload hint also useful?

## Composition on top of KVP

| Structure | Contains | Responsibility |
|---|---|---|
| Physical KVP record | Key and value | Preserve exactly what was written. |
| Indexed value | Base key plus physical parts numbered `0`, `1`, … | Split, identify, group, and order parts without interpreting their contents. |
| Diagnostic entry | Raw key, parsed metadata, and raw value | Explain the diagnostic meaning of one physical record. |
| Decoded diagnostic | Source entries plus decoded message or content | Optional source-specific combination and payload decoding. |

```text
Diagnostics
├── uses the existing KVP record layer
├── uses indexed-value framing for |index
└── optionally interprets grouped diagnostic values
```

Indexed framing understands only the numeric suffix. It does not understand `start`, `finish`, JSON, compression, or diagnostic names. Diagnostics defines the base key and knows how values from that source can be combined.

Use “indexed” or “chunked” for a value spread across KVP records. This is unrelated to an operation span represented by `start` and `finish`.

## Key layouts

- Azure-init: `<agent>|<boot_epoch>|<vm_id>|<type>|<name>|<event_id>|<timestamp>[|<index>]`
- Current cloud-init: `CLOUD_INIT|<boot_epoch>|<type>|<name>|<vm_id>|<event_id>[|<index>]`
- Older cloud-init: `CLOUD_INIT|<boot_epoch>|<type>|<name>|<event_id>[|<index>]`

The source prefix selects the key layout; `type` does not. A numeric suffix is an index only when the remaining base key is a valid diagnostic key.

## Why each field exists

| Field | Why it is meaningful | Example |
|---|---|---|
| `agent` | Identifies the writer and its key namespace. | `CLOUD_INIT`, `azure-init-0.1.1` |
| `boot_epoch` | Separates records produced by different boots. | `1788371515` |
| `vm_id` | Identifies the VM when records are collected outside the guest. | `e73baebd-...` |
| `type` | Preserves the producer's exact classification. It can describe lifecycle, data category, or representation. | `start`, `system-info`, `compressed` |
| `kind` | Gives consumers one common lifecycle view derived from `type`. It is not another encoded key field. | `Start`, `Finish`, `Diagnostic` |
| `name` | Identifies the operation or data the record concerns. | `provision:run`, `dmesg` |
| `event_id` | Identifies one emission and ties its physical parts together. Azure-init may reuse it to correlate a start and finish. | `operation-42` |
| `timestamp` | Identifies when the occurrence happened. Azure-init stores it in the key; cloud-init stores it in the value. | `2026-09-02T17:52:00Z` |
| `index` | Orders physical parts of one value. | `0`, `1`, `2` |
| `value` | Carries the producer-owned message or structured payload. | `starting provisioning` |

## What `type` means compared with `name`

`name` answers **“what is this record about?”** `type` adds context about **what is being reported about that subject**.

| `type` | `name` | Meaning | Derived `kind` |
|---|---|---|---|
| `start` | `provision:run` | The `provision:run` operation began. | `Start` |
| `finish` | `provision:run` | The same operation completed. | `Finish` |
| `diagnostic` | `user:create_user` | Point-in-time data about creating a user. | `Diagnostic` |
| `system-info` | `system information` | Point-in-time system information. | `Diagnostic` |
| `compressed` | `dmesg` | Diagnostic data named `dmesg` is represented as encoded content. | `Diagnostic` |

`type` changes the meaning of the **record**, but it does not rename, parse, or otherwise change `name`. The same name can have different lifecycle types, and the same type can be used with many names.

The exact `type` is retained so `compressed`, `system-info`, and future producer-defined values are not lost. A normalized `kind` is derived as follows:

- `start` → `Start`
- `finish` → `Finish`
- every other type → `Diagnostic`

## Expected cloud-init telemetry

The diagnostic view recognizes both current and older cloud-init key layouts and does not restrict `type` to a fixed allowlist. These are the known telemetry types supported explicitly:

The VM and event IDs below are shortened for readability; cloud-init normally writes UUIDs.

| Cloud-init `type` | Purpose | Expected value fields | Derived `kind` |
|---|---|---|---|
| `start` | An operation began. | `name`, `type`, `ts`, `msg` | `Start` |
| `finish` | An operation completed. | `name`, `type`, `ts`, `result`, `duration`, `msg` | `Finish` |
| `event` | Existing point-event format. | `name`, `type`, `ts`, `msg` | `Diagnostic` |
| `diagnostic` | Point-in-time diagnostic message. | `name`, `type`, `ts`, `msg` | `Diagnostic` |
| `system-info` | OS, kernel, distribution, and cloud-init information. | `name`, `type`, `ts`, `msg` | `Diagnostic` |
| `boot-telemetry` | Kernel, userspace, and cloud-init boot timing. | `name`, `type`, `ts`, `msg` | `Diagnostic` |
| `compressed` | Encoded diagnostic content such as `dmesg` or `cloud-init.log`. | `name`, `type`, `ts`, optional `msg_i`, `msg` | `Diagnostic` |
| Any other value | Future producer-defined telemetry. | Producer-defined | `Diagnostic` |

The value-field list documents known cloud-init output; ordinary reads retain the complete value even when a field is missing or malformed.

### Operation start

```text
CLOUD_INIT|1788371515|start|modules-final/config-install_hotplug|vm-123|event-1
```

```json
{"name":"modules-final/config-install_hotplug","type":"start","ts":"2026-09-02T17:52:24.256376Z","msg":"running config-install_hotplug"}
```

This means the named operation began. `type=start` supplies the lifecycle meaning; `name` identifies the operation.

### Operation finish

```text
CLOUD_INIT|1788371515|finish|modules-final/config-install_hotplug|vm-123|event-2
```

```json
{"name":"modules-final/config-install_hotplug","type":"finish","ts":"2026-09-02T17:52:24.257560Z","result":"SUCCESS","duration":0.0012,"msg":"config-install_hotplug ran successfully"}
```

This means the same named operation completed. Cloud-init may assign start and finish different event IDs; matching names describe the same operation but do not by themselves prove correlation.

### Diagnostic message

```text
CLOUD_INIT|1788371515|diagnostic|diagnostic message|vm-123|event-3
```

```json
{"name":"diagnostic message","type":"diagnostic","ts":"2026-09-02T17:52:25Z","msg":"Ephemeral resource disk exists."}
```

This is point-in-time data. Repeated diagnostic messages remain separate emissions because each has its own event ID.

### System information

```text
CLOUD_INIT|1788371515|system-info|system information|vm-123|event-4
```

```json
{"name":"system information","type":"system-info","ts":"2026-09-02T17:52:26Z","msg":"cloudinit_version=26.1, kernel_version=6.8.0-azure, distro_name=ubuntu"}
```

The exact `system-info` type is retained while its common kind is `Diagnostic`.

### Boot telemetry

```text
CLOUD_INIT|1788371515|boot-telemetry|boot-telemetry|vm-123|event-5
```

```json
{"name":"boot-telemetry","type":"boot-telemetry","ts":"2026-09-02T17:52:27Z","msg":"kernel_start=... user_start=... cloudinit_activation=..."}
```

This carries point-in-time boot timing data and also derives `kind=Diagnostic`.

### Existing `event` compatibility

```text
CLOUD_INIT|1788371515|event|user:create_user|vm-123|event-6
```

```json
{"name":"user:create_user","type":"event","ts":"2026-09-02T17:52:28Z","msg":"Created user azureuser"}
```

`event` remains supported and maps to `Diagnostic`; it is not rejected when `diagnostic` is also supported.

### Compressed, indexed content

```text
CLOUD_INIT|1788371515|compressed|dmesg|vm-123|event-7|0
CLOUD_INIT|1788371515|compressed|dmesg|vm-123|event-7|1
```

```json
{"name":"dmesg","type":"compressed","ts":"2026-09-02T17:52:29Z","msg_i":0,"msg":"{\"encoding\":\"gz+b64\",\"data\":\"first-fragment"}
{"name":"dmesg","type":"compressed","ts":"2026-09-02T17:52:29Z","msg_i":1,"msg":"second-fragment\"}"}
```

Ordinary reads return both physical records. A real split may occur inside an escape and make one value invalid JSON by itself. Optional decoding validates the key and value indices, joins the escaped `msg` fragments, and then interprets the completed envelope according to Decision 3.

### Older layout and unknown types

Older cloud-init records omit `vm_id`:

```text
CLOUD_INIT|1788371515|diagnostic|diagnostic message|event-8
```

The older layout is accepted for every telemetry type listed above.

Unknown types remain visible rather than being rejected:

```text
CLOUD_INIT|1788371515|future-type|future diagnostic|vm-123|event-9
```

The first record has no VM ID. The second retains `type=future-type` and derives `kind=Diagnostic`.

## Clear use cases

### 1. One operation starts and finishes

```text
azure-init|1788371515|vm-123|start|provision:run|operation-42|2026-09-02T17:52:00Z|0
value: starting provisioning

azure-init|1788371515|vm-123|finish|provision:run|operation-42|2026-09-02T17:52:24Z|0
value: provisioning completed
```

- `type` changes from `start` to `finish` because two different lifecycle occurrences are being reported.
- `name` stays `provision:run` because both records concern the same operation.
- `event_id` correlates the occurrences.
- `timestamp` says when each occurred.
- `index=0` says each value fits in one physical record.
- The values remain simple messages because identity and lifecycle metadata are already in the keys.

### 2. The same type describes different diagnostics

```text
azure-init|1788371515|vm-123|diagnostic|user:create_user|event-1|2026-09-02T17:52:25Z|0
value: created user azureuser

azure-init|1788371515|vm-123|diagnostic|network:configure|event-2|2026-09-02T17:52:26Z|0
value: configured eth0
```

Both records are point-in-time diagnostics, but their names identify different subjects. This demonstrates that `type` does not determine `name`.

### 3. One value spans physical records

```text
azure-init|1788371515|vm-123|diagnostic|config:dump|event-4|2026-09-02T17:52:28Z|0
value: first fragment

azure-init|1788371515|vm-123|diagnostic|config:dump|event-4|2026-09-02T17:52:28Z|1
value: second fragment
```

The base key is identical and only the index changes. Indexed framing can group and order the records without knowing they are diagnostics. The diagnostic layer knows that Azure-init values can be concatenated. Ordinary reads still return both physical records independently.

## Read and decode behavior

Ordinary diagnostic reads:

- Return one entry for each recognized physical diagnostic record, in pool order.
- Preserve the complete raw key and value.
- Parse available key metadata.
- Best-effort read cloud-init's timestamp without replacing or decoding its message.
- Retain duplicate, gapped, and incomplete indexed records.
- Omit unrelated records and records whose keys are malformed. The raw KVP view remains available when every pool record is needed.

Optional decoding is a separate request:

- Group only records with the same complete base key.
- Require unique, contiguous indices beginning at zero.
- Validate cloud-init's value index against the key index.
- Combine values according to their source: concatenate Azure-init fragments, or extract and combine cloud-init `msg` fragments.
- Return an explicit error while preserving the source records when combination or decoding fails.

The format has no total-part count, so a missing final fragment cannot always be detected. A physical cloud-init part may also be invalid JSON because a split can occur inside an escaped `msg`; that does not prevent the physical record from being returned.

Existing Azure-init `type=event` records remain readable and derive `kind=Diagnostic`; new point diagnostics use `type=diagnostic`. Unknown types are retained exactly and also derive `kind=Diagnostic`.