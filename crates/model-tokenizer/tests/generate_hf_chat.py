#!/usr/bin/env python3
"""Generate synthetic HF rendered-string goldens; never a runtime dependency.

Run with Jinja2 3.1.6 and Transformers 4.57.6. Pinned templates are local and
hash checked. The text normalization follows vLLM db9527a (Llama: 0b3ba88) chat_utils.py
and ChatCompletionRequest.build_chat_params; no model weights/network/credentials
are used. These are source-profile tests, not active-provider certification.
"""
import copy
import datetime
import hashlib
import json
from pathlib import Path

import jinja2
import transformers
from transformers.utils.chat_template_utils import _compile_jinja_template

ROOT = Path(__file__).resolve().parents[1]
PROFILES = {
    "Glm53Official": "glm53-official.jinja",
    "Glm53FlashOfficial": "glm53-flash-official.jinja",
    "Glm53TinfoilNvfp4": "glm53-tinfoil-nvfp4.jinja",
    "Glm53FlashTinfoilNvfp4": "glm53-flash-continuum.jinja",
    "Glm53Continuum": "glm53-continuum.jinja",
    "Glm53FlashContinuum": "glm53-flash-continuum.jinja",
    "GptOss120bJinjaReference": "gpt-oss-120b.jinja",
    "GptOssSafeguard120bJinjaReference": "gpt-oss-safeguard-120b.jinja",
    "Gemma4Tinfoil": "gemma4-tinfoil.jinja",
    "Llama33Tinfoil": "llama33-tinfoil.jinja",
}


def requests():
    user = {"role": "user", "content": "Hello, 世界 👋 café e\u0301 <tag>& \u2028 <|assistant|>"}
    yield "unicode-and-special-literals", {"messages": [user]}
    yield "simple-ascii", {"messages": [{"role": "user", "content": "Reply briefly."}]}
    yield "python-unicode-whitespace", {"messages": [{"role": "user", "content": "\u001c\u001d\u001e\u001f\u0085\u00a0\u2003Text\u2003\u00a0\u0085\u001f\u001e\u001d\u001c"}]}
    yield "system", {"messages": [{"role": "system", "content": " Be precise.\n"}, user]}
    yield "developer-consolidation", {"messages": [{"role": "system", "content": "System"}, user, {"role": "developer", "content": "Developer"}, user]}
    yield "multipart-text", {"messages": [{"role": "user", "content": [{"type": "text", "text": " First "}, {"type": "text", "text": "Second\n"}]}]}
    yield "multipart-empty-text", {"messages": [{"role": "user", "content": [{"type": "text", "text": "A"}, {"type": "text", "text": ""}, {"type": "text", "text": "B"}]}]}
    yield "history-reasoning-alias", {"messages": [user, {"role": "assistant", "content": "Answer", "reasoning_content": "previous reasoning"}, user]}
    yield "empty-tool-array", {"messages": [user, {"role": "assistant", "content": "Answer", "tool_calls": []}, user]}
    for effort in ["none", "low", "high", "max"]:
        yield f"effort-{effort}", {"messages": [user], "reasoning_effort": effort}
    yield "effort-kwargs-precedence", {"messages": [user], "reasoning_effort": "low", "chat_template_kwargs": {"reasoning_effort": "max", "enable_thinking": False}}
    for sentinel in [None, "auto"]:
        yield f"kwargs-default-filter-{sentinel}", {"messages": [user], "reasoning_effort": "high", "chat_template_kwargs": {"enable_thinking": sentinel, "clear_thinking": sentinel}}
    yield "no-generation-prefix", {"messages": [user], "add_generation_prompt": False}
    tools = [{"type": "function", "function": {"name": "lookup", "description": "Find café <&>\u2028", "parameters": {"type": "object", "properties": {"z": {"type": "number", "default": 1e-5}, "a": {"type": "string", "enum": ["é", "世界"]}}, "required": ["a"]}, "strict": True}}]
    yield "tools-schema", {"messages": [user], "tools": tools}
    yield "tools-default-fields", {"messages": [user], "tools": [{"type": "function", "function": {"name": "empty", "ignored": "pruned"}}]}
    yield "tools-extra-null-strict", {"messages": [user], "tools": [{"type": "function", "extra": "outer-extra", "function": {"name": "empty", "ignored": "extra", "strict": None, "defer_loading": False}}]}
    yield "tools-deferred", {"messages": [user], "tools": [{"type": "function", "defer_loading": True, "function": {"name": "later", "parameters": {"type": "object", "properties": {}}}}]}
    yield "tools-in-user-message", {"messages": [user], "tools": tools, "chat_template_kwargs": {"tools_in_user_message": True}}
    for arguments in ["{\"z\":1e-5,\"a\":\"café\"}", {"a": [1, True, None, {"nested": "<&>"}], "z": -0.0}, "{bad", "[]"]:
        yield f"tool-roundtrip-{len(json.dumps(arguments))}", {"messages": [user, {"role": "assistant", "content": None, "reasoning": "look it up", "tool_calls": [{"id": "call-1", "type": "function", "function": {"name": "lookup", "arguments": arguments}}]}, {"role": "tool", "tool_call_id": "call-1", "content": [{"type": "text", "text": "Result"}, {"type": "text", "text": "世界"}]}, user], "tools": tools}
    yield "two-tools-reordered-results", {"messages": [user, {"role": "assistant", "tool_calls": [{"id": "one", "function": {"name": "lookup", "arguments": "{}"}}, {"id": "two", "function": {"name": "lookup", "arguments": "{}"}}]}, {"role": "tool", "tool_call_id": "two", "content": "Second"}, {"role": "tool", "tool_call_id": "one", "content": "First"}, user], "tools": tools}
    yield "clear-thinking", {"messages": [user, {"role": "assistant", "content": "<think>hidden</think>answer", "reasoning": "reasoning"}, user], "chat_template_kwargs": {"clear_thinking": True, "preserve_thinking": True}}
    for clear in [False,True]:
        yield f"thinking-history-clear-{clear}", {"messages": [user, {"role":"assistant","content":"Past answer","reasoning_content":"Past thought"}, user, {"role":"assistant","content":None,"reasoning":"Current tool thought","tool_calls":[{"id":"current","function":{"name":"lookup","arguments":"{}"}}]}, {"role":"tool","tool_call_id":"current","content":"Current result"}], "tools":tools,"chat_template_kwargs":{"clear_thinking":clear,"enable_thinking":True}}
    yield "disabled-thinking-kwargs", {"messages":[user],"tools":tools,"chat_template_kwargs":{"enable_thinking":False,"clear_thinking":False}}
    yield "duplicate-tool-result-identities", {"messages":[user,{"role":"assistant","content":"","tool_calls":[{"id":"dup","function":{"name":"lookup","arguments":"{}"}},{"id":"other","function":{"name":"lookup","arguments":"{}"}}]},{"role":"tool","tool_call_id":"dup","content":"First"},{"role":"tool","tool_call_id":"dup","content":"Duplicate"},{"role":"tool","tool_call_id":"other","content":"Other"},user],"tools":tools}
    yield "assistant-no-generation-prefix", {"messages": [user, {"role": "assistant", "content": "Answer", "reasoning": "Reason"}], "add_generation_prompt": False}
    yield "raw-json-key-order-duplicates", {"messages": [user, {"role": "assistant", "content": "", "tool_calls": [{"id": "ordered", "function": {"name": "lookup", "arguments": '{"z":{"last":1e-5,"first":2},"a":1,"a":3}'}}]}, {"role": "tool", "tool_call_id": "ordered", "content": "done"}, user], "tools": tools}


def normalize_messages(profile, request, template):
    # vLLM's pinned content-format detector resolves these five templates to
    # openai, and GPT-OSS templates to string. Tool results are text-joined.
    openai = profile not in ["GptOss120bJinjaReference", "GptOssSafeguard120bJinjaReference", "Glm53Continuum", "Glm53FlashTinfoilNvfp4"]
    result = []
    for message in request["messages"]:
        role = message["role"]
        content = message.get("content")
        parts = [] if content is None else [{"type": "text", "text": content}] if isinstance(content, str) else content
        texts = [part if isinstance(part, str) else part.get("text", part.get("refusal")) for part in parts]
        if not openai:
            texts = [text for text in texts if text]
        target = {"role": role, "content": [{"type": "text", "text": text} for text in texts] if openai and role != "tool" else "\n".join(texts)}
        for key in ["name", "task"]:
            if isinstance(message.get(key), str):
                target[key] = message[key]
        if role == "tool" and "tool_call_id" in message:
            target["tool_call_id"] = message["tool_call_id"]
        if role == "developer":
            target["tools"] = message.get("tools")
        if role == "assistant":
            reasoning = message.get("reasoning")
            if reasoning is None:
                reasoning = message.get("reasoning_content")
            if reasoning is not None:
                target.update(reasoning=reasoning, reasoning_content=reasoning)
            calls = copy.deepcopy(message.get("tool_calls"))
            if calls:
                for call in calls:
                    arguments = call["function"].get("arguments")
                    if profile == "Llama33Tinfoil":
                        if not arguments:
                            arguments = {}
                        elif not isinstance(arguments, (dict, list)):
                            arguments = json.loads(arguments)
                        call["function"]["arguments"] = arguments
                        continue
                    if isinstance(arguments, str):
                        try:
                            arguments = json.loads(arguments)
                        except json.JSONDecodeError:
                            arguments = {}
                    call["function"]["arguments"] = arguments if isinstance(arguments, dict) else {}
                target["tool_calls"] = calls
        result.append(target)
    if profile != "Llama33Tinfoil" and any(message["role"] == "developer" for message in result) and '"developer"' not in template and "'developer'" not in template:
        for message in result:
            if message["role"] == "developer":
                message["role"] = "system"
                message.pop("tools", None)
        if any(index > 0 and message["role"] == "system" for index, message in enumerate(result)):
            systems = []
            for message in result:
                if message["role"] == "system":
                    content = message["content"]
                    text = content if isinstance(content, str) else "\n".join(part["text"] for part in content)
                    if text:
                        systems.append(text)
            result = [{"role": "system", "content": "\n\n".join(systems)}, *[m for m in result if m["role"] != "system"]]
    return result


def normalize_tools(profile, request):
    result = []
    for tool in request.get("tools", []):
        original = tool["function"]
        function = {key: original.get(key) for key in ["name", "description", "parameters"]}
        for key in (["defer_loading"] if profile == "Llama33Tinfoil" else ["strict", "defer_loading"]):
            value = original.get(key)
            if value is None and key == "defer_loading":
                value = tool.get(key)
            if value is not None:
                function[key] = value
        if profile == "Llama33Tinfoil":
            function.update({key: value for key,value in original.items() if key not in ["name", "description", "parameters", "defer_loading"]})
        normalized = {"type": "function", "function": function}
        if tool.get("defer_loading") is not None:
            normalized["defer_loading"] = tool["defer_loading"]
        if profile == "Llama33Tinfoil":
            normalized.update({key:value for key,value in tool.items() if key not in ["type", "function", "defer_loading"]})
        result.append(normalized)
    return result


def main():
    manifest = json.loads((ROOT / "profiles/manifest.json").read_text())
    hashes = {entry["file"]: entry["sha256"] for entry in manifest}
    cases = []
    for profile, file in PROFILES.items():
        template = (ROOT / "profiles" / file).read_text()
        assert hashlib.sha256(template.encode()).hexdigest() == hashes[file]
        compiled = _compile_jinja_template(template)
        compiled.environment.globals["strftime_now"] = lambda fmt: datetime.datetime(2026, 10, 20).strftime(fmt)
        for name, original in requests():
            # Match the existing application's serde_json::Value wire order.
            request = json.loads(json.dumps(original, sort_keys=True, ensure_ascii=False))
            variables = copy.deepcopy(request.get("chat_template_kwargs", {}))
            try:
                variables["messages"] = normalize_messages(profile, request, template)
            except Exception:
                cases.append({"name": name, "profile": profile, "date": "2026-10-20", "request": request, "error": True})
                continue
            effort = request.get("reasoning_effort")
            if effort is not None:
                variables["reasoning_effort"] = effort
                variables.setdefault("enable_thinking", effort != "none")
            for key in ["response_format", "documents"]:
                if request.get(key) is not None:
                    variables[key] = request[key]
            if "tools" in request:
                variables["tools"] = normalize_tools(profile, request)
                if request.get("tool_choice") is not None:
                    variables["tool_choice"] = request["tool_choice"]
            variables["add_generation_prompt"] = request.get("add_generation_prompt", True)
            # OnlineRenderer always passes nonempty defaults (tokenize=False),
            # so ChatParams.with_defaults removes null / "auto" override values.
            variables = {key: value for key, value in variables.items() if value is not None and value != "auto"}
            variables["bos_token"] = "<|begin_of_text|>" if profile == "Llama33Tinfoil" else "<bos>" if profile == "Gemma4Tinfoil" else "<|startoftext|>" if profile.startswith("GptOss") else ""
            variables["eos_token"] = "<|eot_id|>" if profile == "Llama33Tinfoil" else "<eos>" if profile == "Gemma4Tinfoil" else "<|return|>" if profile.startswith("GptOss") else "<|endoftext|>"
            case = {"name": name, "profile": profile, "date": "2026-10-20", "request": request}
            try:
                case["rendered"] = compiled.render(**variables)
            except Exception:
                case["error"] = True
            cases.append(case)
    output = {"reference": {"jinja2": jinja2.__version__, "transformers": transformers.__version__, "normalizer_sources": {"default": "https://github.com/vllm-project/vllm/blob/db9527a46873454610df6dbedf79a36d6bf1a7f6/vllm/entrypoints/chat_utils.py", "Llama33Tinfoil": "https://github.com/vllm-project/vllm/blob/0b3ba88f165976e77ca5e6a7a3f5bba4562b80af/vllm/entrypoints/chat_utils.py"}, "scope": "synthetic pinned source-profile rendering, not active-provider certification"}, "cases": cases}
    path = ROOT / "tests/fixtures/hf_chat.json"
    path.parent.mkdir(exist_ok=True)
    path.write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")
    print(f"wrote {len(cases)} synthetic renders ({sum('error' in c for c in cases)} expected errors)")


if __name__ == "__main__":
    main()
