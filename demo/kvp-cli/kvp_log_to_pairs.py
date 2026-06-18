#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Convert a captured azure-init / cloud-init KVP telemetry log into
``KEY=VALUE`` lines suitable for ``libazureinit-kvp load``.

The logs are the null-stripped concatenation of pool records (each record is
``key`` immediately followed by ``value``, repeated). This splits them back
into individual key/value pairs so ``load`` can rebuild a real binary pool
file. Used by ``run-demo.sh``; not part of the shipped CLI.

Handles three record shapes:
  * azure-init:  ``azure-init-<ver>|<vmid>|<LEVEL>|<span>|<uuid>`` + value
  * cloud-init:  ``CLOUD_INIT|...|<uuid>`` + JSON value (starts at ``{``)
  * report:      ``PROVISIONING_REPORT`` + ``result=...`` value
"""

import re
import signal
import sys

# A new record begins immediately before one of these markers (zero-width
# lookahead so the marker stays attached to the record that follows).
BOUNDARY = re.compile(
    r"(?=azure-init-\d[\d.]*\|)"
    r"|(?=CLOUD_INIT\|)"
    r"|(?=PROVISIONING_REPORTresult=)"
)

UUID = re.compile(
    r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
    r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
)

# Wire-format value cap (matches the CLI's --unsafe limit).
MAX_VALUE_BYTES = 2048


def split_record(rec):
    """Return (key, value) for one record, or None if unrecognized."""
    if rec.startswith("PROVISIONING_REPORTresult="):
        return "PROVISIONING_REPORT", rec[len("PROVISIONING_REPORT"):]

    if rec.startswith("azure-init-"):
        # key = first five '|'-delimited fields; the 5th is a 36-char UUID
        # glued to the value, so only split off the four key separators.
        parts = rec.split("|", 4)
        if len(parts) < 5:
            return None
        *head, tail = parts
        m = UUID.match(tail)
        if not m:
            return None
        uuid = m.group(0)
        key = "|".join(head + [uuid])
        return key, tail[len(uuid):]

    if rec.startswith("CLOUD_INIT|"):
        # The value is a JSON object; the key never contains '{'.
        brace = rec.find("{")
        if brace == -1:
            return rec, ""
        return rec[:brace], rec[brace:]

    return None


def sanitize(value):
    """KVP values are single-line; flatten stray CR/LF from messy error
    payloads and cap at the wire-format maximum."""
    value = value.replace("\r", " ").replace("\n", " ")
    return value.encode("utf-8")[:MAX_VALUE_BYTES].decode("utf-8", "ignore")


def main():
    # Stay quiet when a downstream reader (e.g. `head`) closes the pipe early.
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)

    if len(sys.argv) > 1:
        with open(sys.argv[1], encoding="utf-8", errors="replace") as fh:
            data = fh.read()
    else:
        data = sys.stdin.read()

    data = data.strip("\n")
    for rec in BOUNDARY.split(data):
        if not rec:
            continue
        parsed = split_record(rec)
        if parsed is None:
            continue
        key, value = parsed
        key = key.strip()
        if key:
            print(f"{key}={sanitize(value)}")


if __name__ == "__main__":
    main()
