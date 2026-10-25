#!/usr/bin/env python3
"""Generate independent Kimi serving-profile segment goldens, offline only.

Requires Pydantic 2.13.5. Supply captured, pinned vLLM source files and the
official encoder explicitly. Actual source AST functions/classes execute with
text-only multimodal trackers; no vLLM/model initialization, network, credentials,
or inference is used. This script is never a runtime dependency.
"""

import argparse
import ast
import copy
from dataclasses import dataclass, field
import hashlib
import importlib.util
import json
import math
from pathlib import Path
from types import SimpleNamespace
from typing import Any, ClassVar, Literal, cast
import sys

import pydantic
from pydantic import BaseModel, ConfigDict, Field, model_serializer, model_validator

VLLM_PIN = "db9527a46873454610df6dbedf79a36d6bf1a7f6"
KIMI_PIN = "a590ce090cb049c93a33dfe8c208ec652aa20503"
ENCODER_SHA256 = "49ff03305fdc4be26867972788d36150b67f8a9e852e62bb7959d87482223676"
SOURCE_HASHES = {
    "vllm__renderers__kimi_k3.py": "1b51e2bc6af29466d1f15c2f8211990903a947da5995e95e9bc68ed1104ecdb4",
    "vllm__renderers__params.py": "ad38852674ebc5bbe69aa9df91602847d518382cc2ceb0a25aac1d07be66b504",
    "vllm__entrypoints__chat_utils.py": "4d442ec6bc7783f2ed37e7400390631a5f39c27e3a48add89991d1669fef13bd",
    "vllm__entrypoints__openai__chat_completion__protocol.py": "4bdc71f668e4820d8ae5bae2a153fed0296b430cd6353ace8c0c27cfb982ebc1",
    "vllm__entrypoints__generate__base__protocol.py": "67ede411b35a8344ddb87bfaaf1a1abea8acd39a4d23655e34032015505167c0",
    "vllm__entrypoints__serve__engine__protocol.py": "9d865d7006ed6ede3417f75444f8b5dcd82769165e703b140b45ed639d0c16a4",
}


class ValidationError(ValueError):
    def __init__(self, message, **kwargs):
        super().__init__(message)


class QuietLogger:
    def __getattr__(self, name):
        return lambda *args, **kwargs: None


class TextParser:
    def __init__(self, model_config):
        self.model_config = model_config

    def mm_placeholder_storage(self):
        return {}


class TextTracker:
    def __init__(self, model_config, **kwargs):
        self.model_config = model_config

    def create_parser(self, **kwargs):
        return TextParser(self.model_config)

    def resolve_items(self):
        return None, None


def load_nodes(namespace, path, selectors):
    """Execute reviewed AST nodes, unchanged except method decorator removal."""
    tree = ast.parse(path.read_text())
    nodes = []
    for name, methods in selectors.items():
        original = next(node for node in tree.body if getattr(node, "name", None) == name or isinstance(node, ast.AnnAssign) and getattr(node.target, "id", None) == name or isinstance(node, ast.Assign) and any(getattr(target, "id", None) == name for target in node.targets))
        if methods is None:
            nodes.append(copy.deepcopy(original))
        else:
            for method in methods:
                selected = copy.deepcopy(next(node for node in original.body if getattr(node, "name", None) == method))
                selected.decorator_list = []
                selected.name = f"{name}_{method}"
                nodes.append(selected)
    module = ast.Module(body=[ast.ImportFrom(module="__future__", names=[ast.alias(name="annotations")], level=0), *nodes], type_ignores=[])
    exec(compile(ast.fix_missing_locations(module), str(path), "exec"), namespace)


def reference_namespace(root):
    def no_media_merge(defaults, overrides):
        assert defaults is None and overrides is None
        return None

    namespace = {
        "__name__": "kimi_serving_reference",
        "Any": Any, "ClassVar": ClassVar, "Literal": Literal, "cast": cast,
        "BaseModel": BaseModel, "ConfigDict": ConfigDict, "Field": Field,
        "model_serializer": model_serializer, "model_validator": model_validator,
        "dataclass": dataclass, "field": field, "json": json,
        "VLLMValidationError": ValidationError, "logger": QuietLogger(),
        "_REMOVED_GUIDED_FIELDS": set(), "_K3_THINKING_EFFORTS": ("low", "high", "max"),
        "MultiModalItemTracker": TextTracker, "ConversationMessage": dict,
        "_ContentPart": Any,
        "ChatCompletionContentPartTextParam": dict,
        "_AssistantParser": lambda value: value, "_ToolParser": lambda value: value,
        "_TextParser": lambda value: value, "_ThinkParser": lambda value: value,
        "_RefusalParser": lambda value: value,
        "merge_media_io_kwargs": no_media_merge,
    }
    # Dataclasses and Pydantic resolve postponed types through the module table.
    module = type(sys)(namespace["__name__"])
    module.__dict__.update(namespace)
    sys.modules[module.__name__] = module
    namespace = module.__dict__
    for filename, expected in SOURCE_HASHES.items():
        if hashlib.sha256((root / filename).read_bytes()).hexdigest() != expected:
            raise ValueError("source differs from pinned vLLM reference")
    load_nodes(namespace, root / "vllm__entrypoints__serve__engine__protocol.py", {"OpenAIBaseModel": None})
    load_nodes(namespace, root / "vllm__entrypoints__generate__base__protocol.py", {"FunctionDefinition": None, "JsonSchemaResponseFormat": None, "ResponseFormat": None})
    load_nodes(namespace, root / "vllm__entrypoints__openai__chat_completion__protocol.py", {"ChatCompletionToolsParam": None, "ChatCompletionNamedFunction": None, "ChatCompletionNamedToolChoiceParam": None, "ChatCompletionRequest": ["_normalize_messages_before", "check_tool_usage", "check_generation_prompt", "build_chat_params"]})
    load_nodes(namespace, root / "vllm__renderers__params.py", {"merge_kwargs": None, "recursively_merge_kwargs": None, "ChatParams": None})
    load_nodes(namespace, root / "vllm__renderers__kimi_k3.py", {"_dump_k3_template_value": None, "_apply_k3_thinking_kwargs": None, "_normalize_k3_tool_messages": None, "KimiK3Renderer": ["_apply_chat_template"]})
    load_nodes(namespace, root / "vllm__entrypoints__chat_utils.py", {
        "MM_PARSER_MAP": None, "PART_TYPES_TO_SKIP_NONE_CONTENT": None,
        "_parse_chat_message_content_mm_part": None, "_reject_reserved_placeholder_in_text": None,
        "_parse_chat_message_content_part": None, "_parse_chat_message_content_parts": None,
        "_parse_chat_message_content": None, "_postprocess_messages": None, "parse_chat_messages": None,
    })
    return namespace


def cases():
    user = {"role": "user", "content": "Hello, 世界 👋 <|open|>"}
    yield "basic-defaults", {"messages": [user]}
    yield "multipart-newline-empty-parts", {"messages": [{"role": "user", "content": [{"type": "text", "text": "First"}, {"type": "text", "text": ""}, {"type": "text", "text": "Second"}]}]}
    yield "multipart-alias-types", {"messages": [{"role": "user", "content": ["First", {"type": "input_text", "text": "Input"}, {"type": "output_text", "text": "Output"}, {"type": "refusal", "refusal": "Refused"}, {"type": "thinking", "thinking": "Thought"}]}]}
    for reasoning in [
        {"reasoning_content": "deprecated"},
        {"reasoning": "current"},
        {"reasoning": "current", "reasoning_content": "deprecated"},
        {"reasoning": "", "reasoning_content": "deprecated"},
        {"reasoning": None, "reasoning_content": "deprecated"},
    ]:
        yield f"reasoning-alias-{len(reasoning)}-{reasoning.get('reasoning')}", {"messages": [user, {"role": "assistant", "content": "Answer", **reasoning}, user]}
    for effort in [None, "none", "low", "high", "max", "medium"]:
        yield f"top-effort-{effort}", {"messages": [user], "reasoning_effort": effort}
    for kwargs in [
        {"enable_thinking": False}, {"thinking": False}, {"thinking_effort": "low"},
        {"enable_thinking": False, "thinking": True},
        {"enable_thinking": True, "thinking": False},
        {"reasoning_effort": "none"}, {"reasoning_effort": "low"},
        {"thinking_effort": None}, {"thinking": False, "thinking_effort": "medium"},
        {"thinking": None}, {"thinking": "auto"},
        {"thinking_effort": "auto"},
        {"enable_thinking": None}, {"enable_thinking": "auto"},
        {"reasoning_effort": None}, {"reasoning_effort": "auto"},
    ]:
        yield f"kwargs-{kwargs}", {"messages": [user], "chat_template_kwargs": kwargs}
    for effort, kwargs in [
        ("low", {"reasoning_effort": "max"}),
        ("none", {"thinking": True}),
        ("none", {"enable_thinking": True}),
        ("high", {"enable_thinking": False}),
        ("high", {"thinking_effort": "low"}),
    ]:
        yield f"effort-precedence-{effort}-{kwargs}", {"messages": [user], "reasoning_effort": effort, "chat_template_kwargs": kwargs}
    yield "prefix-off", {"messages": [user], "add_generation_prompt": False}
    yield "prefix-top-default-overrides-kwargs", {"messages": [user], "chat_template_kwargs": {"add_generation_prompt": False}}
    yield "prefix-top-off-overrides-kwargs", {"messages": [user], "add_generation_prompt": False, "chat_template_kwargs": {"add_generation_prompt": True}}
    tools = [{"type": "function", "function": {"name": "lookup", "description": "Lookup", "parameters": {"type": "object", "properties": {"z": {"type": "number"}, "a": {"type": "string"}}}}}]
    yield "minimal-typed-tool-defaults", {"messages": [user], "tools": [{"function": {"name": "empty", "ignored": "removed"}, "ignored": "removed"}]}
    yield "typed-tool-null-and-deferred", {"messages": [user], "tools": [{"type": "function", "defer_loading": True, "function": {"name": "later", "strict": None, "parameters": None}}]}
    for choice in [None, "auto", "none", "required", {"type": "function", "function": {"name": "lookup"}}]:
        request = {"messages": [user], "tools": tools}
        if choice is not None:
            request["tool_choice"] = choice
        yield f"tool-choice-{choice}", request
    yield "no-tools-default-choice-collapsed", {"messages": [user], "tool_choice": "none"}
    # The HTTP request validator rejects empty tools. These two deliberate
    # renderer-stage cases test defensive handling of an already materialized
    # request, using actual build_chat_params after that validation boundary.
    yield "renderer-stage-empty-tools-none", {"messages": [user], "tools": [], "tool_choice": "none"}
    yield "renderer-stage-empty-tools-kwargs-required", {"messages": [user], "tools": [], "tool_choice": "none", "chat_template_kwargs": {"tool_choice": "required"}}
    yield "tool-choice-kwargs-retained-on-auto", {"messages": [user], "tools": tools, "chat_template_kwargs": {"tool_choice": "required"}}
    yield "tool-choice-top-none-overrides-kwargs", {"messages": [user], "tools": tools, "tool_choice": "none", "chat_template_kwargs": {"tool_choice": "required"}}
    for arguments in [None, "", '{"z":1e2,"a":[1,2],"a":"last"}', '{"b":{"z":1e-5,"a":[1.0,2e3]},"a":1,"a":3}', '{"b":"NaN","a":"Infinity","nested":{"NaN":"Infinity","z":1e-5}}', '{"z":1e2} trailing', "{bad", "[]", "true", {"z": [1, 2], "a": "<|sep|>"}]:
        yield f"history-arguments-{arguments}", {"messages": [user, {"role": "assistant", "content": None, "tool_calls": [{"id": "opaque", "type": "function", "function": {"name": "lookup", "arguments": arguments}}]}, {"role": "tool", "tool_call_id": "opaque", "content": [{"type": "text", "text": "First"}, {"type": "text", "text": ""}, {"type": "text", "text": "Second"}]}], "tools": tools}
    for name, arguments in [
        ("constants", '{"n":NaN,"positive":Infinity,"negative":-Infinity}'),
        ("overflow", '{"nested":{"n":1e999}}'),
    ]:
        yield f"profile-unsupported-nonfinite-{name}", {"messages": [user, {"role": "assistant", "content": "", "tool_calls": [{"id": "n", "function": {"name": "lookup", "arguments": arguments}}]}, {"role": "tool", "tool_call_id": "n", "content": "result"}], "tools": tools}
    calls = [{"id": "opaque-a", "function": {"name": "first", "arguments": "{}"}}, {"id": "opaque-b", "function": {"name": "second", "arguments": "{}"}}]
    for ids in [("opaque-b", "opaque-a"), ("second:1", "first:0"), ("second:1", "unknown")]:
        yield f"tool-result-aliases-{ids}", {"messages": [{"role": "assistant", "content": "", "tool_calls": calls}, {"role": "tool", "tool_call_id": ids[0], "name": "stale", "tool": "unforwarded", "content": "B"}, {"role": "tool", "tool_call_id": ids[1], "name": "stale", "content": "A"}], "tools": tools}
    for response_format in [{"type": "text"}, {"type": "json_object"}, {"type": "json_schema", "json_schema": {"name": "answer", "schema": {"type": "object", "properties": {"z": {"type": "number"}, "a": {"type": "string"}}}}}, {"type": "json_schema", "json_schema": {"name": "answer", "schema": None}}]:
        yield f"response-format-{response_format['type']}-{len(json.dumps(response_format))}", {"messages": [user], "response_format": response_format}
    yield "response-format-null-schema-preserves-extras", {"messages": [user], "response_format": {"type": "json_schema", "json_schema": {"name": "answer", "schema": None, "extra": "kept", "emptyextra": None}}}
    yield "response-schema-explicit-kwargs", {"messages": [user], "response_format": {"type": "json_schema", "json_schema": {"name": "answer", "schema": {"type": "object"}}}, "chat_template_kwargs": {"response_schema": {"type": "array"}}}
    yield "system-tools-dropped", {"messages": [{"role": "system", "content": "System", "tools": tools}, user]}
    yield "developer-is-rejected", {"messages": [{"role": "developer", "content": "Developer"}, user]}
    yield "developer-tools-is-rejected", {"messages": [{"role": "developer", "content": "", "tools": tools}, user]}


def render(namespace, encoder, original, renderer_stage=False):
    request = copy.deepcopy(original)
    if not renderer_stage:
        namespace["ChatCompletionRequest_check_tool_usage"](None, request)
    namespace["ChatCompletionRequest_check_generation_prompt"](None, request)
    namespace["ChatCompletionRequest__normalize_messages_before"](None, request)
    tools = None if request.get("tools") is None else [namespace["ChatCompletionToolsParam"].model_validate(value) for value in request["tools"]]
    choice = request.get("tool_choice", "none")
    if isinstance(choice, dict):
        choice = namespace["ChatCompletionNamedToolChoiceParam"].model_validate(choice)
    response_format = request.get("response_format")
    if response_format is not None:
        response_format = namespace["ResponseFormat"].model_validate(response_format)
    model = SimpleNamespace(
        chat_template=request.get("chat_template"),
        chat_template_kwargs=request.get("chat_template_kwargs"),
        add_generation_prompt=request.get("add_generation_prompt", True),
        continue_final_message=request.get("continue_final_message", False),
        documents=request.get("documents"), reasoning_effort=request.get("reasoning_effort"),
        media_io_kwargs=None, tools=tools, tool_choice=choice, response_format=response_format,
    )
    params = namespace["ChatCompletionRequest_build_chat_params"](model, None, "auto")
    tool_dicts = None if tools is None else [tool.model_dump() for tool in tools]
    defaults = namespace["merge_kwargs"](None, {"tools": tool_dicts, "tokenize": False})
    params = params.with_defaults(defaults)
    conversation, _, _ = namespace["parse_chat_messages"](request["messages"], SimpleNamespace(enable_prompt_embeds=False, multimodal_config=None), content_format="string")
    conversation = namespace["_normalize_k3_tool_messages"](conversation)
    captured = {}

    class Tokenizer:
        def apply_chat_template(self, messages, **kwargs):
            captured["kwargs"] = copy.deepcopy(kwargs)
            tools = kwargs.pop("tools", None)
            thinking = kwargs.pop("thinking", True)
            add_generation_prompt = kwargs.pop("add_generation_prompt", True)
            kwargs.setdefault("thinking_effort", "max")
            return encoder.build_chat_segments(messages, tools=tools, thinking=thinking, add_generation_prompt=add_generation_prompt, **kwargs)

    renderer = SimpleNamespace(get_tokenizer=lambda: Tokenizer())
    segments = namespace["KimiK3Renderer__apply_chat_template"](renderer, conversation, params)
    return {
        "normalized_messages": conversation,
        "effective_kwargs": captured["kwargs"],
        "segments": [{"text": value.text, "allow_special": value.allow_special} for value in segments],
    }


def json_safe_metadata(value):
    if isinstance(value, float) and not math.isfinite(value):
        return {"reference_nonfinite": "NaN" if math.isnan(value) else "-Infinity" if value < 0 else "Infinity"}
    if isinstance(value, dict):
        return {key: json_safe_metadata(item) for key, item in value.items()}
    if isinstance(value, list):
        return [json_safe_metadata(item) for item in value]
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference-root", type=Path, required=True)
    parser.add_argument("--encoder", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path(__file__).parent / "fixtures/kimi_serving.json")
    args = parser.parse_args()
    if pydantic.__version__ != "2.13.5":
        parser.error("fixture reference requires Pydantic 2.13.5")
    if hashlib.sha256(args.encoder.read_bytes()).hexdigest() != ENCODER_SHA256:
        parser.error("official encoder differs from the reviewed pinned source")
    spec = importlib.util.spec_from_file_location("kimi_official_encoder", args.encoder)
    encoder = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = encoder
    spec.loader.exec_module(encoder)
    namespace = reference_namespace(args.reference_root)
    output = []
    for name, original in cases():
        # Actual application outbound serde maps are sorted; JSON arguments
        # remain opaque strings until actual Python postprocessing parses them.
        request = json.loads(json.dumps(original, ensure_ascii=False, sort_keys=True))
        case = {"name": name, "profile": "KimiK3", "request": request}
        renderer_stage = name.startswith("renderer-stage-")
        if renderer_stage:
            case.update(stage="renderer_after_api_validation", api_would_reject_empty_tools=True)
        if name.startswith("profile-unsupported-"):
            case["profile_unsupported"] = True
        try:
            case.update(json_safe_metadata(render(namespace, encoder, request, renderer_stage)))
        except (ValueError, AssertionError) as error:
            case.update(error=True, reference_error_type=type(error).__name__)
        output.append(case)
    fixture = {
        "vllm_revision": VLLM_PIN, "model_revision": KIMI_PIN,
        "pydantic_version": pydantic.__version__, "source_sha256": SOURCE_HASHES,
        "encoder_sha256": ENCODER_SHA256,
        "boundary": "Actual pinned vLLM AST request/renderer functions plus Pydantic model serializers and official XTML encoder. Text-only tracker stubs; no inference, model initialization, live endpoint or provider identity claim.",
        "cases": output,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(fixture, ensure_ascii=False, indent=2, allow_nan=False) + "\n")
    errors = sum(bool(case.get("error")) for case in output)
    print(f"Generated {len(output)} cases ({errors} expected reference errors)")


if __name__ == "__main__":
    main()
