"""Emit the SWE fields consumed by SweBenchRecord as UTF-8 JSON Lines."""
import json
import sys

try:
    import pyarrow.parquet as pq
except ModuleNotFoundError as exc:
    sys.exit(f"{exc}. Install PyArrow in the active environment with python -m pip install pyarrow")

FIELDS = {
    "id", "instance_id", "repo", "base_commit", "commit", "problem_statement",
    "question", "hints", "hints_text", "test_command", "command", "test",
    "FAIL_TO_PASS", "PASS_TO_PASS", "test_patch", "timeout_secs", "tags",
}


def decode_binary(value):
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    raise TypeError(f"unexpected SWE field type: {type(value).__name__}")


with pq.ParquetFile(sys.argv[1]) as source:
    columns = [name for name in source.schema_arrow.names if name in FIELDS]
    for batch in source.iter_batches(batch_size=1024, columns=columns):
        for record in batch.to_pylist():
            sys.stdout.buffer.write(json.dumps(record, default=decode_binary).encode("ascii") + b"\n")
