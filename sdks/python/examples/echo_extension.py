#!/usr/bin/env python3
"""Minimal S5R echo extension.

Run from a host via extension.json:

    {
      "extension_id": "s5r-echo-example",
      "protocol": { "s5r": "3.0" },
      "command": ["python3", "/path/to/sdks/python/examples/echo_extension.py"],
      "env": { "PYTHONPATH": "/path/to/sdks/python/src" }
    }

Or run the conformance suite against it (see README.md).
"""

from s5r import ToolDefinition, Worker, tool_text

worker = Worker("s5r-echo-example", "0.1.0")


@worker.tool(
    ToolDefinition(
        name="echo",
        description="Echo the input text back to the caller",
        parameters={
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    )
)
async def echo(arguments, ctx):
    return tool_text(str(arguments["text"]))


if __name__ == "__main__":
    worker.run_stdio()
