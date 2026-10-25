#!/usr/bin/env python3
"""Regenerate synthetic Kimi segment goldens using the exact upstream encoder.

This is an offline test-fixture tool, never a library/runtime dependency. Pass
the pinned encoding_k3.py explicitly; no credentials or network access occur.
"""

import argparse
import hashlib
import importlib.util
import json
import math
from pathlib import Path
import random
import struct
import sys

PIN = "a590ce090cb049c93a33dfe8c208ec652aa20503"
REFERENCE_SHA256 = "49ff03305fdc4be26867972788d36150b67f8a9e852e62bb7959d87482223676"


def requests():
    base = [{"role": "user", "content": "Hello, 世界 👋 café e\u0301 <|open|>"}]
    yield "unicode-and-literal-control-text", {"messages": base}
    yield "system-and-names", {"messages": [
        {"role": "system", "name": 'a&"b', "content": "Be precise."},
        {"role": "user", "name": "user", "content": [{"type": "text", "text": "One"}, {"type": "text", "text": "Two"}]},
    ]}
    history = [
        {"role": "user", "content": "Question"},
        {"role": "assistant", "content": "Answer", "reasoning_content": "  "},
        {"role": "assistant", "content": None, "reasoning": "fallback reasoning"},
        {"role": "user", "content": "Again"},
    ]
    yield "assistant-reasoning", {"messages": history}
    yield "nonthinking-drops-reasoning", {"messages": history, "chat_template_kwargs": {"thinking": False}}
    for effort in [None, "low", "high", "max"]:
        yield f"effort-{effort}", {"messages": base, "chat_template_kwargs": {"thinking_effort": effort}}
    yield "no-generation-prefix", {"messages": base, "add_generation_prompt": False}
    yield "top-reasoning-effort-is-not-custom-thinking", {"messages": base, "reasoning_effort": "low"}
    tools = [{"type": "function", "function": {"name": "lookup", "description": "Lookup café", "parameters": {"type": "object", "properties": {"z": {"type": "number"}, "a": {"type": "string"}}}}}]
    for choice in [None, "auto", "none", "required", {"type": "function", "function": {"name": "lookup"}}]:
        request = {"messages": base, "tools": tools}
        if choice is not None:
            request["tool_choice"] = choice
        yield f"tools-choice-{choice}", request
    yield "no-tools-default-none-collapsed", {"messages": base, "tool_choice": "none"}
    yield "dynamic-tools", {"messages": [{"role": "system", "tools": tools}, *base]}
    arguments = [
        None,
        "",
        {"z": [1, True, None, {"b": "é", "a": False}], "a": "<|sep|>"},
        {"f1": 1e-5, "f2": 1e-4, "f3": 1e15, "f4": 1e16, "f5": -0.0, "f6": 100.0, "f7": 1.23456789012345e-4},
        '{"z":1e2, "a":[1, 2],"nested":{" x ": true},"a":"decoded\\ntext"} ignored suffix',
        '{"n": NaN,"inf": Infinity,"negative": -Infinity,"s":"raw\ncontrol","object":{"s":"nested\tcontrol"}}',
        '  {} trailing',
        '[]',
        '{malformed',
        '   ',
        '{"not_constant":NaN1}',
        '{"not_constant":Infinity123}',
        '{"bad_escape":"backslash\\\nnewline"}',
        '{"overflow":1e999,"nested":{"underflow":-1e999},"integer":99999999999999999999999999999999999999999999999}',
    ]
    for index, args in enumerate(arguments):
        yield f"arguments-{index}", {"messages": [
            {"role": "assistant", "content": "", "tool_calls": [{"id": "opaque", "type": "function", "function": {"name": 'lookup&"', "arguments": args}}]},
            {"role": "tool", "tool_call_id": "opaque", "content": "result"},
        ], "tools": tools}
    calls = [
        {"id": "a", "function": {"name": "first", "arguments": {"value": 1}}},
        {"function": {"name": "idless", "arguments": "{}"}},
        {"id": "b", "function": {"name": "third", "arguments": "{}"}},
    ]
    yield "tool-results-reordered-authoritative-names", {"messages": [
        {"role": "assistant", "content": None, "tool_calls": calls},
        {"role": "tool", "tool_call_id": "b", "name": "stale", "content": "B"},
        {"role": "tool", "tool_call_id": "a", "tool": "stale", "content": "A"},
    ]}
    yield "partly-unmatched-results-retain-order", {"messages": [
        {"role": "assistant", "content": None, "tool_calls": calls},
        {"role": "tool", "tool_call_id": "b", "name": "explicit", "content": "B"},
        {"role": "tool", "tool_call_id": "unknown", "content": "Unknown"},
    ]}
    yield "duplicate-ids-first-match", {"messages": [
        {"role": "assistant", "content": None, "tool_calls": [calls[0], {"id": "a", "function": {"name": "second", "arguments": None}}]},
        {"role": "tool", "tool_call_id": "a", "content": "A"},
    ]}
    for response_format in [{"type": "json_object"}, {"type": "json_schema", "json_schema": {"name": "test", "schema": {"type": "object", "properties": {"z": {"type": "number"}, "a": {"type": "string"}}}}}, "json_schema"]:
        yield f"response-format-{response_format}", {"messages": base, "response_format": response_format}
    yield "explicit-response-schema", {"messages": base, "response_format": {"type": "json_schema"}, "chat_template_kwargs": {"response_schema": {"z": 1, "a": 2}}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path(__file__).parent / "fixtures/kimi_xtml.json")
    args = parser.parse_args()
    if hashlib.sha256(args.reference.read_bytes()).hexdigest() != REFERENCE_SHA256:
        parser.error("reference encoder differs from the reviewed pinned source")
    spec = importlib.util.spec_from_file_location("kimi_reference", args.reference)
    reference = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = reference
    spec.loader.exec_module(reference)
    cases = []
    for name, request in requests():
        # serde_json's default map order is sorted in the actual Rust request.
        request = json.loads(json.dumps(request, ensure_ascii=False, sort_keys=True))
        kwargs = dict(request.get("chat_template_kwargs") or {})
        kwargs.setdefault("thinking_effort", "max")
        kwargs.setdefault("add_generation_prompt", request.get("add_generation_prompt", True))
        if request.get("tools") and "tool_choice" in request:
            kwargs["tool_choice"] = request["tool_choice"]
        if "response_format" in request:
            kwargs["response_format"] = request["response_format"]
        segments = reference.build_chat_segments(request["messages"], tools=request.get("tools"), **kwargs)
        cases.append({"name": name, "request": request, "segments": [{"text": segment.text, "allow_special": segment.allow_special} for segment in segments]})
    fixture = {"reference": f"https://huggingface.co/moonshotai/Kimi-K3/blob/{PIN}/encoding_k3.py", "reference_sha256": hashlib.sha256(args.reference.read_bytes()).hexdigest(), "boundary": "Synthetic text-only segment goldens; no model inference or live-provider identity claim.", "cases": cases}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(fixture, ensure_ascii=False, indent=2) + "\n")
    print(f"Generated {len(cases)} synthetic cases in {args.output}")
    rng = random.Random(1017)
    numbers = [0.0, -0.0, 1e-5, 1e-4, 1e15, 1e16, 1e23, 100.0]
    for _ in range(2048):
        value = struct.unpack("!d", rng.getrandbits(64).to_bytes(8, "big"))[0]
        if math.isfinite(value):
            numbers.append(value)
    float_fixture = {
        "reference": "CPython json.dumps(ensure_ascii=False) finite f64 formatting",
        "seed": 1017,
        "cases": [{"json": json.dumps(value), "expected": json.dumps(value)} for value in numbers],
    }
    float_output = args.output.parent / "python_float.json"
    float_output.write_text(json.dumps(float_fixture, indent=2) + "\n")
    print(f"Generated {len(numbers)} finite float goldens in {float_output}")


if __name__ == "__main__":
    main()
