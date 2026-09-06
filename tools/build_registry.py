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



ACCESSOR_RE = re.compile(
    r"pub fn (\w+)\(\s*&self,?\s*\)\s*->\s*Result<(.+?), crate::error::Error>\s*\{\s*self\.0\.read(_opt)?\((\d+)\)",
    re.S,
)


def classify_type(rust_type: str) -> str:
    """Map a generated accessor return type onto a JSON-encodable kind.

    Only writes need this: TLV is self-describing on read, but turning a JSON
    payload back into TLV has to know whether a string is text or base64 bytes.
    """
    t = rust_type.replace(" ", "").replace("\n", "")
    t = t.removeprefix("Option<").removesuffix(">") if t.startswith("Option<") else t
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
    if "ArrayIter" in t or "TLVArray" in t:
        return "list"
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


def parse_request_types(source: str, command_name: str) -> dict[str, str]:
    """Field TLV tag -> kind, read off the generated request struct accessors."""
    marker = f"impl<'a> {command_name}Request<'a> {{"
    start = source.find(marker)
    if start < 0:
        return {}
    end = source.find("\n}", start)
    block = source[start:end if end > 0 else len(source)]
    return {tag: classify_type(rust_type) for _, rust_type, _, tag in ACCESSOR_RE.findall(block)}


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

        command_responses = parse_command_responses(source)
        commands = {}
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
            types = parse_request_types(source, command_name)
            if types:
                entry["types"] = types
            commands[command_name] = entry

        registry[str(cluster_id)] = {
            "name": pascal(path.stem),
            "revision": revision,
            "commands": commands,
            "attributes": enums.get("AttributeId", {}),
            "events": enums.get("EventId", {}),
        }

    Path(out_path).write_text(json.dumps(registry, separators=(",", ":"), sort_keys=True))
    clusters = len(registry)
    commands = sum(len(c["commands"]) for c in registry.values())
    attributes = sum(len(c["attributes"]) for c in registry.values())
    print(f"{clusters} clusters, {commands} commands, {attributes} attributes")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
