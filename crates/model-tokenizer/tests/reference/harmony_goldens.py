# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: Copyright 2026 Maple Privacy Labs
"""Regenerate synthetic native Harmony goldens using independent upstream code.

OFFLINE development tool. This executes hash-pinned vLLM 0.22.0 functions and
OpenAI's compiled openai-harmony 0.0.8 Python wheel, never the Maple Rust port.
Pass an already-extracted official wheel on PYTHONPATH plus existing pydantic;
set --source-dir to the hash-verified source bundle and --encodings-dir to a
directory holding canonical o200k_base.tiktoken. No imports of vLLM/torch, model
weights, provider calls, credential access, downloads, or package installation.

The source recipe pins the same vLLM/Harmony path for both GPT-OSS models. The
120B Tinfoil ignore_eos patch changes decoding termination, not this prompt.
These fixtures prove parity with that pinned source, not active fleet identity.
"""

import argparse
import ast
import copy
import hashlib
import importlib.metadata
import json
import logging
import os
from pathlib import Path
from types import SimpleNamespace
from typing import Any, ClassVar, Literal

import openai_harmony as harmony
from pydantic import BaseModel, ConfigDict, model_serializer, model_validator

VLLM_REVISION = "0b3ba88f165976e77ca5e6a7a3f5bba4562b80af"
HARMONY_REVISION = "abd677f7ac962629c808197caa1feb9e3e95d2b0"
ENCODING_SHA = "446a9538cb6c348e3516120d7c08b09f57c36495e2acfffe59a5bf8b0cfb1a2d"
SOURCES = {
    "vllm022_vllm_entrypoints_openai_parser_harmony_utils.py": (
        "vllm/entrypoints/openai/parser/harmony_utils.py",
        "8b92fe61b4a2db9c5796be46cb7ae00218759255e9d236ced3d2070ec00b27d5",
    ),
    "vllm022_vllm_entrypoints_serve_render_serving.py": (
        "vllm/entrypoints/serve/render/serving.py",
        "1dc7be234dad01e2013dd7ddcde6549cdf9b109d4ebca59c433fb364262f0066",
    ),
    "vllm022_protocol.py": (
        "vllm/entrypoints/openai/chat_completion/protocol.py",
        "8dca1877812be555fc12235a9edd4775870e122da8e3b8a4de556ad1b534e3ee",
    ),
    "vllm022_engine_protocol.py": (
        "vllm/entrypoints/openai/engine/protocol.py",
        "e65d22c0259b8bbc17b7812eae8e6f8e9465561a300ba3b4efcec908f4c7d8d3",
    ),
    "vllm022_mistral.py": (
        "vllm/tokenizers/mistral.py",
        "86d67cf9b68ec08924c1f6284a60685241f0b891b22ee844505aa4fd3f6bfc43",
    ),
}
MODELS = ["gpt-oss-120b", "gpt-oss-safeguard-120b"]
DATE = "2026-10-20"


def compile_nodes(nodes, filename, namespace):
    future = ast.ImportFrom(
        module="__future__", names=[ast.alias(name="annotations")], level=0
    )
    module = ast.fix_missing_locations(ast.Module(body=[future, *nodes], type_ignores=[]))
    exec(compile(module, filename, "exec"), namespace)


def source_nodes(source_dir, filename):
    raw = (source_dir / filename).read_bytes()
    assert hashlib.sha256(raw).hexdigest() == SOURCES[filename][1], filename
    return ast.parse(raw).body


def class_node(nodes, name):
    return next(n for n in nodes if isinstance(n, ast.ClassDef) and n.name == name)


def function_node(nodes, name):
    return next(n for n in nodes if isinstance(n, ast.FunctionDef) and n.name == name)


def load_reference(source_dir, encodings_dir):
    assert importlib.metadata.version("openai-harmony") == "0.0.8"
    encoding = encodings_dir / "o200k_base.tiktoken"
    assert hashlib.sha256(encoding.read_bytes()).hexdigest() == ENCODING_SHA
    # This explicit local source disables the upstream loader's network fallback.
    os.environ["TIKTOKEN_ENCODINGS_BASE"] = str(encodings_dir.resolve())
    enc = harmony.load_harmony_encoding(harmony.HarmonyEncodingName.HARMONY_GPT_OSS)
    namespace = {
        **vars(harmony), "Any": Any, "Literal": Literal, "ClassVar": ClassVar,
        "BaseModel": BaseModel, "ConfigDict": ConfigDict,
        "model_serializer": model_serializer, "model_validator": model_validator,
        "logger": logging.getLogger("offline_harmony_reference"),
        "envs": SimpleNamespace(
            VLLM_GPT_OSS_HARMONY_SYSTEM_INSTRUCTIONS=False, VLLM_SYSTEM_START_DATE=DATE
        ),
    }
    engine = source_nodes(source_dir, "vllm022_engine_protocol.py")
    compile_nodes(
        [class_node(engine, "OpenAIBaseModel"), class_node(engine, "FunctionDefinition")],
        "pinned_engine_protocol.py", namespace,
    )
    protocol = source_nodes(source_dir, "vllm022_protocol.py")
    compile_nodes([class_node(protocol, "ChatCompletionToolsParam")],
                  "pinned_chat_tools.py", namespace)
    for name in ("OpenAIBaseModel", "FunctionDefinition", "ChatCompletionToolsParam"):
        namespace[name].model_rebuild(_types_namespace=namespace)
    request_class = class_node(protocol, "ChatCompletionRequest")
    for method_name in (
        "_normalize_messages_before", "check_generation_prompt", "check_tool_usage"
    ):
        method = copy.deepcopy(function_node(request_class.body, method_name))
        method.decorator_list = []
        compile_nodes([method], f"pinned_{method_name}.py", namespace)
    functions = {
        "get_system_message", "create_tool_definition", "get_developer_message",
        "parse_chat_inputs_to_harmony_messages", "auto_drop_analysis_messages",
        "flatten_chat_text_content", "parse_chat_input_to_harmony_message",
        "render_for_completion",
    }
    helpers = source_nodes(
        source_dir, "vllm022_vllm_entrypoints_openai_parser_harmony_utils.py"
    )
    selected = [n for n in helpers if
                (isinstance(n, ast.FunctionDef) and n.name in functions)
                or (isinstance(n, ast.Assign)
                    and any(isinstance(t, ast.Name) and t.id == "REASONING_EFFORT"
                            for t in n.targets))]
    namespace["get_encoding"] = lambda: enc
    compile_nodes(selected, "pinned_harmony_utils.py", namespace)
    mistral = source_nodes(source_dir, "vllm022_mistral.py")
    compile_nodes([function_node(mistral, "maybe_serialize_tool_calls")],
                  "pinned_tool_materialization.py", namespace)
    namespace["_mt"] = SimpleNamespace(
        maybe_serialize_tool_calls=namespace["maybe_serialize_tool_calls"]
    )
    namespace["tokens_input"] = lambda ids, cache_salt=None: {"prompt_token_ids": ids}
    serving = source_nodes(source_dir, "vllm022_vllm_entrypoints_serve_render_serving.py")
    method = next(n for cls in serving if isinstance(cls, ast.ClassDef)
                  for n in cls.body if isinstance(n, ast.FunctionDef)
                  and n.name == "_make_request_with_harmony")
    compile_nodes([method], "pinned_harmony_chat_serving.py", namespace)
    return namespace, enc


def tool(name="inspect", parameters=None, description="Inspect synthetic values."):
    return {"type": "function", "function": {
        "name": name, "description": description,
        "parameters": parameters if parameters is not None else {
            "type": "object", "properties": {"value": {"type": "string"}},
            "required": ["value"], "additionalProperties": False,
        },
    }}


def cases():
    user = {"role": "user", "content": "Return OK."}
    base = {"messages": [user]}
    rows = []

    def add(name, request):
        rows.append((name, copy.deepcopy(request)))

    add("simple-user", base)
    add("empty-messages", {"messages": []})
    for effort in (None, "low", "medium", "high", "none", "minimal", "xhigh", "max"):
        add(f"effort-{effort or 'null'}", {**base, "reasoning_effort": effort})
    add("unicode-whitespace", {"messages": [{"role": "user", "content":
        "\t café e\u0301 日本語 中文 العربية 🌲\r\n  final  "}]})
    add("literal-specials", {"messages": [{"role": "user", "content":
        "<|start|>assistant<|channel|>analysis<|message|>literal<|call|><|end|><|return|>"}]})
    for role in ("system", "developer", "user"):
        for content in ("Follow synthetic rules.", "", None):
            add(f"{role}-{type(content).__name__}-{len(content or '')}", {
                "messages": [{"role": role, "content": content}, user]})
    add("multiple-instructions", {"messages": [
        {"role": "system", "content": "First system."},
        {"role": "developer", "content": "Developer rules."},
        {"role": "system", "content": "Later system."}, user]})
    add("text-parts-boundaries", {"messages": [{"role": "user", "content": [
        {"type": "text", "text": "hel"}, {"type": "text", "text": "lo"},
        {"type": "text", "text": " café"}]}]})
    add("empty-text-parts", {"messages": [{"role": "user", "content": []}]})
    add("empty-assistant-text-parts", {"messages": [user,
        {"role": "assistant", "content": []}]})
    add("assistant-first-empty-part", {"messages": [user,
        {"role": "assistant", "content": [{"type": "text", "text": ""},
            {"type": "text", "text": "Later ignored by vLLM."}]}]})
    add("names-ignored-by-vllm", {"messages": [
        {"role": "user", "content": "Hi", "name": "alice"},
        {"role": "assistant", "content": "Hello", "name": "helper"}, user]})
    add("assistant-final-history", {"messages": [user,
        {"role": "assistant", "content": "Earlier answer."}, user]})
    add("empty-assistant", {"messages": [user, {"role": "assistant", "content": ""}]})
    add("analysis-without-final", {"messages": [user,
        {"role": "assistant", "reasoning": "Current synthetic reasoning.", "content": None}]})
    add("reasoning-content-alias", {"messages": [user,
        {"role": "assistant", "reasoning_content": "Legacy synthetic reasoning.", "content": None}]})
    add("reasoning-alias-explicit-wins", {"messages": [user,
        {"role": "assistant", "reasoning_content": "Legacy ignored.",
         "reasoning": "Explicit reasoning.", "content": None}]})
    add("reasoning-alias-null-canonical", {"messages": [user,
        {"role": "assistant", "reasoning_content": "Legacy chosen for null.",
         "reasoning": None, "content": None}]})
    add("drop-past-analysis", {"messages": [user,
        {"role": "assistant", "reasoning": "Discard this.", "content": "Final one."},
        user, {"role": "assistant", "reasoning": "Discard too.", "content": "Final two."},
        user, {"role": "assistant", "reasoning": "Keep this.", "content": None}]})
    add("tool-basic", {**base, "tools": [tool()]})
    add("tool-default-type", {**base, "tools": [{"function": tool()["function"]}]})
    add("tools-empty", {**base, "tools": []})
    add("tools-null", {**base, "tools": None})
    for choice in ("auto", "none", "required", {"type": "function", "function": {"name": "inspect"}}):
        add(f"tool-choice-{choice if isinstance(choice, str) else 'named'}",
            {**base, "tools": [tool()], "tool_choice": choice})
    for name, choice in (
        ("unknown", {"type": "function", "function": {"name": "missing"}}),
        ("missing-function", {"type": "function"}),
        ("missing-name", {"type": "function", "function": {}}),
    ):
        add(f"tool-choice-{name}", {**base, "tools": [tool()], "tool_choice": choice})
    add("tool-choice-auto-without-tools", {**base, "tool_choice": "auto"})
    add("tool-choice-none-without-tools", {**base, "tool_choice": "none"})
    add("tool-no-description", {**base, "tools": [{"type": "function", "function": {
        "name": "inspect", "parameters": {"type": "object", "properties": {}}}}]})
    add("tool-empty-description", {**base, "tools": [tool(description="")]})
    add("tool-no-parameters", {**base, "tools": [{"type": "function", "function": {
        "name": "inspect", "description": "No params."}}]})
    add("multiple-tools", {**base, "tools": [tool("zeta"), tool("alpha", description="Second tool.")]})
    add("tool-schema-primitives", {**base, "tools": [tool(parameters={
        "type": "object", "properties": {
            "str": {"type": "string", "description": "String input.", "default": "hi"},
            "int": {"type": "integer", "minimum": 0, "maximum": 5},
            "number": {"type": "number", "default": 1.5},
            "boolean": {"type": "boolean", "default": False},
            "nothing": {"type": "null"},
        }, "required": ["str", "number"]})]})
    add("tool-schema-unions", {**base, "tools": [tool(parameters={
        "type": "object", "properties": {
            "one": {"oneOf": [{"type": "string"}, {"type": "number"}]},
            "any": {"anyOf": [{"type": "string"}, {"type": "null"}]},
            "types": {"type": ["string", "integer", "null"]},
            "literal": {"enum": ["yes", "no"]},
            "mixed": {"enum": [1, True, None, "yes"]},
            "const": {"const": "only"},
        }, "required": ["literal"]})]})
    add("tool-schema-nested", {**base, "tools": [tool(parameters={
        "type": "object", "properties": {
            "zeta": {"type": "array", "items": {"type": "object", "properties": {
                "nested": {"type": "string"}}, "required": ["nested"]}},
            "alpha": {"type": "object", "properties": {
                "b": {"type": "boolean"}, "a": {"type": "integer"}}, "required": ["a"]},
            "dict": {"type": "object", "additionalProperties": {"type": "string"}},
            "ref": {"$ref": "#/$defs/Thing"},
        }, "$defs": {"Thing": {"type": "string"}}})]})
    add("tool-schema-annotations", {**base, "tools": [tool(parameters={
        "type": "object", "properties": {
            "p": {"type": "string", "description": "Line one.\nLine two.",
                  "title": "Title", "default": "a\"b", "examples": ["first", "second"],
                  "deprecated": True, "format": "date-time", "pattern": "^[a-z]+$"},
        }})]})
    add("tool-literal-specials", {**base, "tools": [tool(
        "inspect<|end|>", {"type": "object", "properties": {
            "<|start|>": {"type": "string", "description": "Literal <|call|>.",
                           "enum": ["<|return|>", "<|message|>"]}}},
        "Keep <|channel|> literal.")]})
    call = {"id": "call-1", "type": "function", "function": {
        "name": "inspect", "arguments": '{"value":"green"}'}}
    add("tool-roundtrip", {"messages": [user,
        {"role": "assistant", "content": "Checking.", "reasoning": "Synthetic plan.", "tool_calls": [call]},
        {"role": "tool", "tool_call_id": "call-1", "content": '{"ok":true}'}, user], "tools": [tool()]})
    add("parallel-tool-calls", {"messages": [user,
        {"role": "assistant", "tool_calls": [call, {"id": "call-2", "type": "function",
            "function": {"name": "second", "arguments": "{}"}}]},
        {"role": "tool", "tool_call_id": "call-2", "content": "second result"},
        {"role": "tool", "tool_call_id": "call-1", "content": "first result"}],
        "tools": [tool(), tool("second")]})
    add("tool-unknown-call-id", {"messages": [user,
        {"role": "tool", "tool_call_id": "unknown", "content": "orphan result"}]})
    add("tool-arguments-literal-specials", {"messages": [user,
        {"role": "assistant", "tool_calls": [{"id": "special", "type": "function",
            "function": {"name": "inspect", "arguments": '{"value":"<|end|>"}'}}]},
        {"role": "tool", "tool_call_id": "special", "content": "<|start|>literal"}]})
    add("tool-text-parts", {"messages": [user,
        {"role": "assistant", "content": [{"type": "text", "text": "pre"},
            {"type": "text", "text": "amble"}], "tool_calls": [call]},
        {"role": "tool", "tool_call_id": "call-1", "content": [
            {"type": "text", "text": "left"}, {"type": "text", "text": "right"}]}]})
    add("sampling-options-no-prompt-effect", {**base, "temperature": 0.2,
        "max_tokens": 10, "response_format": {"type": "json_object"},
        "add_generation_prompt": False, "continue_final_message": True})
    add("generation-prompt-both-true", {**base,
        "add_generation_prompt": True, "continue_final_message": True})
    add("generation-prompt-both-false", {**base,
        "add_generation_prompt": False, "continue_final_message": False})
    add("generation-prompt-add-only", {**base, "add_generation_prompt": True})
    add("generation-prompt-continue-only", {**base, "continue_final_message": True})
    add("generation-prompt-truthy-numbers", {**base,
        "add_generation_prompt": 1, "continue_final_message": 1})
    add("generation-prompt-truthy-string-before-coercion", {**base,
        "add_generation_prompt": True, "continue_final_message": "false"})
    add("date-override", {**base, "_reference_date": "2030-01-02"})
    return rows


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-dir", type=Path, required=True)
    parser.add_argument("--encodings-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    namespace, enc = load_reference(args.source_dir, args.encodings_dir)
    native_path = Path(harmony.openai_harmony.__file__)
    result = {
        "schema": 1, "offline_only": True, "requests_are_synthetic": True,
        "request_object_order": "recursively_lexicographic",
        "models": MODELS, "date_context": DATE,
        "reference": {"vllm_revision": VLLM_REVISION,
            "harmony_revision": HARMONY_REVISION, "harmony_python_version": "0.0.8",
            "pydantic_version": importlib.metadata.version("pydantic"),
            "harmony_python_wrapper_sha256": hashlib.sha256(
                Path(harmony.__file__).read_bytes()).hexdigest(),
            "harmony_native_binary_sha256": hashlib.sha256(
                native_path.read_bytes()).hexdigest(),
            "harmony_native_platform": native_path.name,
            "encoding_sha256": ENCODING_SHA,
            "source_files": [{"path": path, "sha256": sha,
                "url": f"https://github.com/vllm-project/vllm/blob/{VLLM_REVISION}/{path}"}
                for path, sha in SOURCES.values()],
        },
        "qualification_notes": [
            "Native upstream Harmony wheel renders token IDs; no Maple renderer generates expected outputs.",
            "Actual pinned vLLM normalization/helpers/serving method are AST-extracted without model runtime imports.",
            "Request fields are synthetic text-only Chat Completions; this is not a full HTTP request-schema validation test.",
            "Both source recipes use native vLLM0.22 Harmony; no active shared-provider identity claim follows.",
            "Each native render_text_into call encodes ordinary text separately; concatenation alone loses token boundaries.",
        ], "cases": [],
    }
    for name, original in cases():
        # Fixtures must represent identical input under serde_json's default
        # BTreeMap mode and its consumer-enabled preserve_order feature. Sort
        # every object recursively BEFORE the independent upstream render, then
        # retain exactly that request ordering in the written reference fixture.
        # Array order and JSON string contents (including tool arguments) remain
        # untouched. This is fixture normalization, not a production policy.
        request = json.loads(json.dumps(original, ensure_ascii=False, sort_keys=True))
        date = request.pop("_reference_date", DATE)
        row = {"id": name, "models": MODELS, "date_context": date, "request": request}
        try:
            request = namespace["check_generation_prompt"](None, copy.deepcopy(request))
            request = namespace["check_tool_usage"](None, request)
            request = namespace["_normalize_messages_before"](None, request)
            tools = request.get("tools")
            if tools is not None:
                tools = [namespace["ChatCompletionToolsParam"](**item) for item in tools]
            req = SimpleNamespace(messages=request["messages"], tools=tools,
                reasoning_effort=request.get("reasoning_effort"), cache_salt=None)
            namespace["envs"].VLLM_SYSTEM_START_DATE = date
            messages, rendered = namespace["_make_request_with_harmony"](
                SimpleNamespace(supports_browsing=False, supports_code_interpreter=False),
                req, should_include_tools=tools is not None,
            )
            ids = rendered[0]["prompt_token_ids"]
            row["expected"] = {"rendered_text": enc.decode_utf8(ids),
                "token_ids": ids, "token_count": len(ids)}
        except (ValueError, TypeError, harmony.HarmonyError) as err:
            row["expected_error"] = {"kind": type(err).__name__}
        result["cases"].append(row)
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"cases": len(result["cases"]), "models": len(MODELS),
        "success": sum("expected" in x for x in result["cases"]),
        "errors": sum("expected_error" in x for x in result["cases"]),
        "sha256": hashlib.sha256(args.output.read_bytes()).hexdigest()}))


if __name__ == "__main__":
    main()
