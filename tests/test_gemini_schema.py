import importlib.util
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "gemini_schema", Path(__file__).resolve().parent.parent / "gemini_schema.py"
)
gemini_schema = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gemini_schema)


def test_openai_tool_becomes_a_gemini_declaration():
    tool = {
        "type": "function",
        "function": {
            "name": "memory",
            "description": "Save or read memory.",
            "parameters": {
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "additionalProperties": False,
                "properties": {
                    "action": {"type": "string", "enum": ["add", "read"], "default": "read"},
                    "limit": {"type": ["integer", "null"], "minimum": 1},
                    "mode": {"const": "fast"},
                    "target": {"$ref": "#/$defs/Target"},
                    "tags": {"type": "array", "items": {"oneOf": [{"type": "string"}, {"type": "null"}]}},
                    "level": {"enum": [1, 2, None]},
                },
                "required": ["action", "missing"],
                "$defs": {"Target": {"type": "object", "properties": {"file": {"type": "string"}}}},
            },
        },
    }
    decl = gemini_schema.function_declaration(tool)
    assert decl["name"] == "memory" and decl["behavior"] == "NON_BLOCKING"
    p = decl["parameters"]
    assert "additionalProperties" not in p and "$schema" not in p and "$defs" not in p
    assert p["required"] == ["action"]
    props = p["properties"]
    assert props["action"] == {"type": "string", "enum": ["add", "read"]}
    assert props["limit"] == {"type": "integer", "nullable": True, "minimum": 1}
    assert props["mode"] == {"type": "string", "enum": ["fast"]}
    assert props["target"] == {"type": "object", "properties": {"file": {"type": "string"}}}
    assert props["tags"]["items"] == {"type": "string", "nullable": True}
    assert props["level"] == {"type": "string", "enum": ["1", "2"]}


def test_recursive_refs_and_empty_parameters_are_safe():
    recursive = {
        "name": "tree",
        "parameters": {
            "type": "object",
            "properties": {"node": {"$ref": "#/definitions/Node"}},
            "definitions": {"Node": {"type": "object", "properties": {"child": {"$ref": "#/definitions/Node"}}}},
        },
    }
    decl = gemini_schema.function_declaration(recursive, behavior=None)
    assert "behavior" not in decl
    assert decl["parameters"]["properties"]["node"]["properties"]["child"] == {"type": "object"}
    bare = gemini_schema.function_declaration({"name": "ping", "parameters": {"type": "object"}})
    assert "parameters" not in bare
