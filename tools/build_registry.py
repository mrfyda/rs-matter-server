"""Generate server/src/matter/clusters.json from rs-matter's cluster code.

`device_command` addresses commands and their payload fields by name, so the
server needs the same cluster metadata the reference server gets from matter.js.
rs-matter generates exactly that — cluster ids, command ids, attribute ids, the
TLV context tags of every request and response struct, and each payload field's
type — as Rust source, so it is lifted from there rather than transcribed from
the specification. Taking it from the same stack that puts the bytes on the wire
means the two cannot drift apart.

Usage:

    # rs-matter writes its generated clusters into the build directory, so the
    # crate has to have been compiled at least once.
    cargo build --manifest-path server/Cargo.toml --release
    python3 tools/build_registry.py \
        "$(ls -d server/target/release/build/rs-matter-*/out/clusters_generated | head -1)" \
        server/src/matter/clusters.json

Re-run this after upgrading rs-matter. The output is checked in so that
building this project does not depend on locating a build artifact.

The names it emits are rs-matter's PascalCase spellings; converting them to the
wire form clients use is done at load time by server/src/matter/wire_naming.rs.

A payload field's kind is one of the scalar names below, `list:<kind>` for an
array, or `struct:<Name>` naming an entry in the same cluster's `structs`
table. Structs are emitted by reference rather than inlined: the same struct is
often reachable from several commands, and a reference cannot recurse forever
if a definition ever becomes cyclic.
"""
import json
import re
import sys
from pathlib import Path

ENUM_RE = re.compile(r"pub enum (\w+) \{(.*?)\n\}", re.S)
VARIANT_RE = re.compile(r"^\s*(\w+) = (\d+),", re.M)
CLUSTER_ID_RE = re.compile(
    r"pub const FULL_CLUSTER: crate::dm::Cluster<'static> = crate::dm::Cluster::new\(\s*(\d+),\s*(\d+),",
    re.S,
)

# The return type is captured lazily, so the tail has to match every spelling
# rustfmt produces: a one-line `Result<T, Error>` and the wrapped form, which
# puts each parameter on its own line and leaves a trailing comma. Without the
# optional comma the lazy group runs past the end of the accessor and pairs a
# later field's type with this field's tag.
ACCESSOR_RE = re.compile(
    r"pub fn (\w+)\(\s*&self,?\s*\)\s*->\s*Result<\s*(.+?),\s*crate::error::Error,?\s*>"
    r"\s*\{\s*self\.0\.read(_opt)?\((\d+)\)",
    re.S,
)

ARRAY_RE = re.compile(r"(?:TLVArray|ArrayIter)<\s*'\w+\s*,\s*(.+)>$")


def unwrap(rust_type: str) -> str:
    """Strip the wrappers that do not change how a value is encoded."""
    for wrapper in ("Option<", "crate::tlv::Nullable<", "Nullable<"):
        while rust_type.startswith(wrapper):
            rust_type = rust_type[len(wrapper) : -1]
    return rust_type


def classify_type(rust_type: str, structs: set[str]) -> str:
    """Map a generated accessor return type onto a JSON-encodable kind.

    Encoding is what needs this: TLV is self-describing on read, but turning a
    JSON payload back into TLV has to know whether a string is text or base64
    bytes, and whether an object's keys are field names or numeric tags.
    """
    t = unwrap("".join(rust_type.split()))
    array = ARRAY_RE.search(t)
    if array:
        return "list:" + classify_type(array.group(1), structs)
    if "OctetStr" in t:
        return "bytes"
    if "Utf8Str" in t:
        return "string"
    if t.startswith("bool"):
        return "bool"
    if re.match(r"^[ui](8|16|32|64)$", t):
        return "int"
    if re.match(r"^f(32|64)$", t):
        return "float"
    # A struct is named by its own type, and the generated code always gives it
    # a tag enum. Anything else — an enum, a bitmap, a newtype like
    # `AmperageMilliA` — encodes from the JSON shape alone.
    bare = re.sub(r"<.*", "", t.split("::")[-1])
    if bare in structs:
        return "struct:" + bare
    return "other"


COMMAND_RESPONSE_RE = re.compile(
    r"Command::new\(\s*CommandId::(\w+) as _,\s*Some\(CommandResponseId::(\w+) as _\),",
    re.S,
)


def parse_command_responses(source: str) -> dict[str, str]:
    """Command name -> response struct name.

    The generated cluster metadata states this explicitly, which matters
    because a response struct is often shared and named for neither command
    that produces it (AddNOC, UpdateNOC, UpdateFabricLabel and RemoveFabric all
    answer with NOCResponse).
    """
    return dict(COMMAND_RESPONSE_RE.findall(source))


def parse_field_types(source: str, type_name: str, structs: set[str]) -> dict[str, str]:
    """Field TLV tag -> kind, read off a generated struct's own accessors.

    The same shape serves a command's request struct and any struct nested
    inside it: both are a `TLVElement` newtype with one accessor per field.
    """
    marker = f"impl<'a> {type_name}<'a> {{"
    start = source.find(marker)
    if start < 0:
        return {}
    end = source.find("\n}", start)
    block = source[start : end if end > 0 else len(source)]
    return {
        tag: classify_type(rust_type, structs)
        for _, rust_type, _, tag in ACCESSOR_RE.findall(block)
    }


def struct_reference(kind: str) -> str | None:
    """The struct a kind names, through any number of list wrappers."""
    while kind.startswith("list:"):
        kind = kind[len("list:") :]
    return kind[len("struct:") :] if kind.startswith("struct:") else None


def collect_structs(
    source: str, kinds: dict[str, str], enums: dict[str, dict[str, int]], structs: set[str]
) -> dict[str, dict]:
    """Every struct definition reachable from the given field kinds.

    Walks references breadth-first so a struct nested inside a struct is
    emitted too, and keeps a seen set so a cyclic definition would terminate.
    """
    out: dict[str, dict] = {}
    pending = [name for name in map(struct_reference, kinds.values()) if name]
    while pending:
        name = pending.pop()
        if name in out:
            continue
        fields = enums.get(f"{name}Tag")
        if not fields:
            continue
        types = parse_field_types(source, name, structs)
        out[name] = {"fields": fields, "types": types}
        pending.extend(ref for ref in map(struct_reference, types.values()) if ref)
    return out


def pascal(snake: str) -> str:
    return "".join(part[:1].upper() + part[1:] for part in snake.split("_"))


def parse_enums(source: str) -> dict[str, dict[str, int]]:
    out = {}
    for name, body in ENUM_RE.findall(source):
        variants = {v: int(n) for v, n in VARIANT_RE.findall(body)}
        if variants:
            out[name] = variants
    return out


def main(generated_dir: str, out_path: str) -> None:
    directory = Path(generated_dir)
    registry = {}

    for path in sorted(directory.glob("*.rs")):
        source = path.read_text()
        cluster_match = CLUSTER_ID_RE.search(source)
        if cluster_match is None:
            continue
        cluster_id = int(cluster_match.group(1))
        revision = int(cluster_match.group(2))
        enums = parse_enums(source)
        # A struct is anything with a tag enum; the suffix alone is not a
        # reliable test, since request and response structs carry one too.
        structs = {name[: -len("Tag")] for name in enums if name.endswith("Tag")}

        command_responses = parse_command_responses(source)
        commands = {}
        definitions: dict[str, dict] = {}
        for command_name, command_id in enums.get("CommandId", {}).items():
            entry = {"id": command_id}
            request = enums.get(f"{command_name}RequestTag")
            if request:
                entry["req"] = request
            # Prefer the declared response struct; fall back to the
            # same-named one for clusters that do not declare a mapping.
            response_struct = command_responses.get(command_name, command_name)
            response = enums.get(f"{response_struct}Tag") or enums.get(
                f"{command_name}ResponseTag"
            )
            if response:
                entry["resp"] = response
            types = parse_field_types(source, f"{command_name}Request", structs)
            if types:
                entry["types"] = types
                definitions.update(collect_structs(source, types, enums, structs))
            commands[command_name] = entry

        cluster = {
            "name": pascal(path.stem),
            "revision": revision,
            "commands": commands,
            "attributes": enums.get("AttributeId", {}),
            "events": enums.get("EventId", {}),
        }
        if definitions:
            cluster["structs"] = definitions
        registry[str(cluster_id)] = cluster

    Path(out_path).write_text(json.dumps(registry, separators=(",", ":"), sort_keys=True))
    clusters = len(registry)
    commands = sum(len(c["commands"]) for c in registry.values())
    attributes = sum(len(c["attributes"]) for c in registry.values())
    structs = sum(len(c.get("structs", {})) for c in registry.values())
    print(f"{clusters} clusters, {commands} commands, {attributes} attributes, {structs} structs")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
