"""Convert Hermes's OpenAI-style tool schemas to Gemini function declarations.

Gemini's `parameters` takes an OpenAPI subset: no $ref, oneOf, const, type lists or
additionalProperties, and enum values must be strings. Anything it can't express is dropped
rather than rejected, so one odd tool never breaks a call's setup.
"""

from __future__ import annotations

KEEP = {"type", "format", "description", "nullable", "enum", "properties", "required", "items",
        "anyOf", "minItems", "maxItems", "minimum", "maximum", "minLength", "maxLength", "pattern",
        "title"}  # fmt: skip
TYPES = {"string", "number", "integer", "boolean", "array", "object"}


def _resolve(node, defs: dict, seen: frozenset):
    ref = node.get("$ref", "") if isinstance(node, dict) else ""
    if not ref.startswith("#/"):
        return node, seen
    name = ref.rsplit("/", 1)[-1]
    if name in seen or name not in defs:
        return {"type": "object"}, seen  # recursive or dangling reference
    return _resolve(defs[name], defs, seen | {name})


def _schema(node, defs: dict, seen: frozenset = frozenset()) -> dict:
    node, seen = _resolve(node, defs, seen)
    if not isinstance(node, dict):
        return {"type": "string"}
    out: dict = {}
    if "const" in node:
        out["enum"] = [str(node["const"])]
    variants = node.get("anyOf") or node.get("oneOf")
    kind = node.get("type")
    if isinstance(kind, list):
        if "null" in kind:
            out["nullable"] = True
        kind = next((t for t in kind if t != "null"), "string")
    if variants:
        real = [v for v in variants if not (isinstance(v, dict) and v.get("type") == "null")]
        if len(real) < len(variants):
            out["nullable"] = True
        if len(real) == 1:
            out.update({k: v for k, v in _schema(real[0], defs, seen).items() if k not in out})
        elif real:
            out["anyOf"] = [_schema(v, defs, seen) for v in real]
    if kind in TYPES:
        out["type"] = kind
    elif "properties" in node:
        out["type"] = "object"
    elif "items" in node:
        out["type"] = "array"
    elif "anyOf" not in out and "type" not in out:
        out["type"] = "string"
    for key, value in node.items():
        if key not in KEEP or key in ("type", "anyOf") or key in out:
            continue
        if key == "properties":
            out[key] = {name: _schema(sub, defs, seen) for name, sub in value.items()}
        elif key == "items":
            out[key] = _schema(value, defs, seen)
        elif key == "enum":
            out[key] = [str(v) for v in value if v is not None]
            out["type"] = "string"  # Gemini only takes string enums
        elif key == "required":
            out[key] = [r for r in value if r in node.get("properties", {})]
        else:
            out[key] = value
    if out.get("type") == "object" and not out.get("properties"):
        out.pop("required", None)
    return out


def function_declaration(tool: dict, behavior: str | None = "NON_BLOCKING") -> dict:
    """One OpenAI tool ({"type": "function", "function": {...}} or the bare function) to Gemini."""
    fn = tool.get("function", tool)
    params = fn.get("parameters") or {}
    defs = {**params.get("definitions", {}), **params.get("$defs", {})}
    decl = {"name": fn["name"], "description": (fn.get("description") or "")[:1024]}
    if params.get("properties"):
        decl["parameters"] = _schema(params, defs)
    if behavior:
        decl["behavior"] = behavior
    return decl
