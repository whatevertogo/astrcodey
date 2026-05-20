"""Tests that expose the bug."""
import tempfile
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from src.processor import process_contacts


def test_plus_addresses_are_valid():
    """Gmail-style +tag addresses should be accepted."""
    input_csv = "name,email,phone\nAlice,alice+work@gmail.com,555-1234\nBob,bob@example.com,555-5678\n"

    with tempfile.NamedTemporaryFile(mode='w', suffix='.csv', delete=False) as fin:
        fin.write(input_csv)
        input_path = fin.name

    output_path = input_path + ".out"

    try:
        valid, invalid = process_contacts(input_path, output_path)
        # Both should be valid — alice+work@gmail.com is a valid email
        assert valid == 2, f"Expected 2 valid, got {valid} (invalid={invalid})"
        assert invalid == 0, f"Expected 0 invalid, got {invalid}"

        with open(output_path) as f:
            lines = f.readlines()
        assert len(lines) == 3  # header + 2 data lines
        assert "alice+work@gmail.com" in lines[1]
    finally:
        os.unlink(input_path)
        if os.path.exists(output_path):
            os.unlink(output_path)


if __name__ == "__main__":
    test_plus_addresses_are_valid()
    print("All tests passed!")
