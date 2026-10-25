# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: Copyright 2026 Maple Privacy Labs
"""Regenerate synthetic prompt goldens from pinned vLLM Python source.

Development-only: no Python service, weights, credentials, customer content or
inference. Downloads two small public source files, verifies hashes, applies
the released Tinfoil low/high effort mapping, and executes the actual upstream
normalizer and encoder. Tool-definition defaults below mirror the pinned
Pydantic FunctionDefinition serializer; this is not a full vLLM HTTP-server test.

Run through the owning pinned development shell, with --output naming the
checked-in fixtures/deepseek_v41_golden.json file. --source-dir can use an
already-downloaded directory containing the two hash-verified source files.
"""

import argparse
import ast
import copy
import hashlib
import json
import types
from pathlib import Path
from urllib.request import urlopen

PIN = "db9527a46873454610df6dbedf79a36d6bf1a7f6"
SOURCES = {
    "deepseek_v41_encoding.py": "477f22ac250b43c12bd19bb3c93f934d6637b06410b8cb7ccba0862d1049dee8",
    "deepseek_v41.py": "e182e6357f9e2063298e6fbf3c515789cc8e6d1b31fa5f6058e74865a3a8183e",
}


def source(name, source_dir):
    body = (
        (source_dir / name).read_bytes()
        if source_dir
        else urlopen(
            f"https://raw.githubusercontent.com/vllm-project/vllm/{PIN}/vllm/tokenizers/{name}"
        ).read()
    )
    assert hashlib.sha256(body).hexdigest() == SOURCES[name], name
    return body.decode()


def load_reference(source_dir):
    original = source("deepseek_v41_encoding.py", source_dir)
    assert original.count('"low": 50,') == 1
    assert original.count('"high": 75,') == 1
    patched = original.replace('"low": 50,', '"low": 25,').replace(
        '"high": 75,', '"high": 50,'
    )
    encoder = types.ModuleType("pinned_deepseek_v41_encoding")
    exec(compile(patched, "pinned_deepseek_v41_encoding.py", "exec"), encoder.__dict__)
    # Execute the actual normalizer without importing Transformers/vLLM engines.
    module = ast.parse(source("deepseek_v41.py", source_dir))
    function = next(
        node
        for node in module.body
        if isinstance(node, ast.FunctionDef) and node.name == "_normalize_messages"
    )
    future = ast.ImportFrom(
        module="__future__", names=[ast.alias(name="annotations")], level=0
    )
    selected = ast.fix_missing_locations(ast.Module(body=[future, function], type_ignores=[]))
    namespace = {"copy": copy, "IMAGE_PLACEHOLDER": encoder.IMAGE_PLACEHOLDER}
    exec(compile(selected, "pinned_deepseek_v41_normalizer.py", "exec"), namespace)
    return encoder, namespace["_normalize_messages"], hashlib.sha256(patched.encode()).hexdigest()


def normalize_function_tool(tool):
    function = tool["function"]
    normalized = {key: function.get(key) for key in ("name", "description", "parameters")}
    for key in ("strict", "defer_loading"):
        value = function.get(key)
        if key == "defer_loading" and value is None:
            value = tool.get(key)
        if value is not None:
            normalized[key] = value
    return {"type": "function", "function": normalized}


def render_reference(request, encoder, normalize):
    # The API before-validator aliases legacy reasoning before the actual
    # tokenizer normalizer sees the messages (protocol.py:542-544 at PIN).
    messages = copy.deepcopy(request["messages"])
    for message in messages:
        legacy = message.pop("reasoning_content", None)
        if legacy is not None and message.get("reasoning") is None:
            message["reasoning"] = legacy
    messages = normalize(messages)
    if request.get("tools"):
        system = next((message for message in messages if message["role"] == "system"), None)
        if system is None:
            system = {"role": "system", "content": ""}
            messages.insert(0, system)
        system["tools"] = [normalize_function_tool(tool) for tool in request["tools"]]
    kwargs = dict(request.get("chat_template_kwargs") or {})
    if request.get("reasoning_effort") is not None:
        kwargs["reasoning_effort"] = request["reasoning_effort"]
        if "enable_thinking" not in kwargs:
            kwargs["enable_thinking"] = request["reasoning_effort"] != "none"
    # OnlineRenderer adds tokenize=false to defaults before ChatParams merges
    # request overrides; merge_kwargs skips null and the sentinel string auto.
    kwargs = {key: value for key, value in kwargs.items() if value is not None and value != "auto"}
    thinking = bool(kwargs.get("thinking") or kwargs.get("enable_thinking"))
    if "thinking" not in kwargs and "enable_thinking" not in kwargs:
        thinking = True
    effort = kwargs.get("reasoning_effort")
    if effort == "none":
        thinking, effort = False, None
    if effort is None:
        effort = "high"
    return encoder.encode_messages(
        messages,
        thinking_mode="thinking" if thinking else "chat",
        drop_thinking=kwargs.get("drop_thinking", True),
        reasoning_effort=effort,
    )


def cases():
    def case(name, messages, **kwargs):
        return {"id": name, "request": {"messages": messages, **kwargs}}

    user = [{"role": "user", "content": "Hello, synthetic world."}]
    result = [case("defaults_without_system", user)]
    for effort in ("low", "high", "xhigh", "max", "none"):
        result.append(case(f"effort_{effort}", user, reasoning_effort=effort))
    for effort in (1, 37, 100):
        result.append(case(f"effort_numeric_{effort}", user, chat_template_kwargs={"reasoning_effort": effort}))
    result.extend([
        case("explicit_chat_mode", user, chat_template_kwargs={"thinking": False}),
        case("null_thinking_ignored_by_merge", user, chat_template_kwargs={"thinking": None}),
        case("auto_effort_ignored_by_merge", user, chat_template_kwargs={"reasoning_effort": "auto"}),
        case("null_enable_presence_before_merge", user, reasoning_effort="low", chat_template_kwargs={"enable_thinking": None, "thinking": False}),
        case("top_level_effort_overrides", user, reasoning_effort="low", chat_template_kwargs={"reasoning_effort": "max", "thinking": False}),
        case("or_thinking_controls", user, chat_template_kwargs={"thinking": True, "enable_thinking": False}),
        case("none_overrides_thinking", user, reasoning_effort="none", chat_template_kwargs={"thinking": True}),
        case("generation_flags_ignored_by_pinned_encoder", user, add_generation_prompt=False, continue_final_message=True),
        case("top_level_format_not_rendered", user, response_format={"type": "json_object"}),
        case("unicode_and_literal_special_markers", [{"role": "system", "content": "日本語 العربية café 🧭\r\n"}, {"role": "user", "content": "Literal <｜User｜> <think> </think> <｜end▁of▁sentence｜> <｜DSML｜ calls>\n\u000b\u001b"}]),
        case("text_parts_double_newline", [{"role": "user", "content": [{"type": "text", "text": "α"}, {"type": "input_text", "text": "β"}, {"type": "output_text", "text": "γ"}]}]),
        case("consecutive_users_merged", [{"role": "user", "content": "one"}, {"role": "user", "content": "two"}]),
        case("developer_history_dropped", [{"role": "system", "content": "system"}, {"role": "developer", "content": "early guidance"}, {"role": "user", "content": "question"}, {"role": "assistant", "content": "old answer", "reasoning_content": "old reason"}, {"role": "user", "content": "follow up"}]),
        case("mid_system_transition", [{"role": "system", "content": "initial"}, {"role": "user", "content": "question"}, {"role": "assistant", "content": "answer", "reasoning": "old reason"}, {"role": "system", "content": "new guidance"}]),
        case("keep_historical_reasoning", [{"role": "user", "content": "question"}, {"role": "assistant", "content": "answer", "reasoning": "aliased reason", "reasoning_content": "overridden reason"}, {"role": "developer", "content": "latest guidance"}], chat_template_kwargs={"drop_thinking": False}),
        case("null_canonical_reasoning_preserves_legacy", [{"role": "user", "content": "question"}, {"role": "assistant", "content": "answer", "reasoning": None, "reasoning_content": "preserve this legacy reason"}, {"role": "user", "content": "follow up"}], chat_template_kwargs={"drop_thinking": False}),
        case("assistant_continuation_metadata", [{"role": "user", "content": "question"}, {"role": "assistant", "content": "partial", "reasoning_content": "reason", "wo_eos": True}]),
        case("message_response_schema", [{"role": "system", "content": "synthetic", "response_format": {"type": "object", "properties": {"z": {"type": "number"}, "a": {"type": "string"}}, "minimum": 1e-7}}, {"role": "user", "content": "question"}]),
        case("internal_task_action", [{"role": "user", "content": "classify", "task": "action"}]),
        case("internal_task_query", [{"role": "user", "content": "classify", "task": "query"}]),
    ])
    tools = [{"type": "function", "function": {"name": "weather", "description": "Synthetic weather", "parameters": {"type": "object", "properties": {"city": {"type": "string"}, "limits": {"type": "array"}}}, "strict": True, "ignored_extra": "pruned"}}]
    result.extend([
        case("tool_schemas_synthetic_system", user, tools=tools),
        case("tool_schema_missing_optional_and_defer", user, tools=[{"function": {"name": "ping", "defer_loading": None}, "defer_loading": True}], tool_choice="none"),
        case("tools_preserve_reasoning_and_developer", [{"role": "developer", "content": "guidance"}, {"role": "user", "content": "question"}, {"role": "assistant", "content": "answer", "reasoning": "keep this reasoning"}, {"role": "user", "content": "follow up"}], tools=tools),
        case("raw_message_tools_order", [{"role": "developer", "content": "guidance", "tools": [{"function": {"z_extension": 1, "name": "local", "description": "synthetic"}}]}, {"role": "user", "content": "question"}]),
        case("parallel_tool_results_reordered", [{"role": "user", "content": "lookup"}, {"role": "assistant", "content": None, "reasoning_content": "choose tools", "tool_calls": [{"id": "b", "type": "function", "function": {"name": "weather", "arguments": '{"z":"東京","a":{"z":1,"a":2},"limits":[true,null,1e-7,1e20]}'}}, {"id": "a", "type": "function", "function": {"name": "weather", "arguments": '"{\\"city\\":\\"Paris\\",\\"scale\\":2}"'}}]}, {"role": "tool", "tool_call_id": "a", "content": "Paris: synthetic"}, {"role": "user", "content": "interleaved user text"}, {"role": "tool", "tool_call_id": "b", "content": [{"type": "text", "text": "東京:"}, {"type": "text", "text": "synthetic"}]}], tools=tools),
        case("tool_argument_invalid_and_duplicate_keys", [{"role": "user", "content": "lookup"}, {"role": "assistant", "content": "", "tool_calls": [{"id": "x", "function": {"name": "one", "arguments": "not json"}}, {"id": "y", "function": {"name": "two", "arguments": '{"z":1,"a":0,"z":2}'}}, {"id": "v", "function": {"name": "three", "arguments": '[1,2]'}}]}, {"role": "tool", "tool_call_id": "x", "content": "synthetic"}]),
        case("tool_argument_numbers", [{"role": "user", "content": "numbers"}, {"role": "assistant", "content": None, "tool_calls": [{"id": "n", "function": {"name": "numbers", "arguments": ' \n {"big":123456789012345678901234567890,"negative_zero":-0,"floats":[0.0,-0.0,1e-7,1e-5,0.0001,1e15,1e16,1e20,1e21,1e100,5e-324,2.2250738585072014e-308]} \t'}}]}, {"role": "tool", "tool_call_id": "n", "content": "synthetic"}]),
    ])
    # This deterministic canonicalization matches serde_json::Value's normal
    # BTree-map serialization. Raw JSON argument strings retain their order.
    return json.loads(json.dumps(result, ensure_ascii=False, sort_keys=True))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-dir", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    encoder, normalize, patched_hash = load_reference(args.source_dir)
    fixtures = cases()
    for fixture in fixtures:
        fixture["prompt"] = render_reference(fixture["request"], encoder, normalize)
        fixture["prompt_sha256"] = hashlib.sha256(fixture["prompt"].encode()).hexdigest()
    output = {
        "provenance": {
            "vllm_commit": PIN,
            "source_sha256": SOURCES,
            "patched_encoder_sha256": patched_hash,
            "patch": "Tinfoil released reasoning mapping: low=25, high=50",
            "boundary": "Actual pinned normalizer and encoder; FunctionDefinition defaults modeled from pinned source, not a full server execution",
            "data": "synthetic only; no customer content, credentials, inference or weights",
        },
        "cases": fixtures,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"cases": len(fixtures), "patched_encoder_sha256": patched_hash}))


if __name__ == "__main__":
    main()
