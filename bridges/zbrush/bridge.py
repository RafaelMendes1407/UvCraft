"""ZBrush OBJ exchange with UvCraft, using ZBrush 2026's Python command bridge."""

from __future__ import annotations

import json
import math
from pathlib import Path
from tempfile import TemporaryDirectory


class BridgeError(ValueError):
    pass


def parse_commands(commands):
    if isinstance(commands, str):
        try:
            commands = json.loads(commands)
        except json.JSONDecodeError as exc:
            raise BridgeError(f"invalid command JSON: {exc}") from exc
    if isinstance(commands, dict):
        commands = [commands]
    if not isinstance(commands, (list, tuple)) or not commands or any(
        not isinstance(command, dict) or not isinstance(command.get("cmd"), str)
        for command in commands
    ):
        raise BridgeError("commands must be an object or nonempty array with cmd strings")
    return commands


def read_obj(path):
    """Capture the exact geometry records and resolve polygon point indices."""
    lines = Path(path).read_text(encoding="utf-8").splitlines(keepends=True)
    vertices = []
    faces = []
    face_lines = []
    for line_index, line in enumerate(lines):
        fields = line.split()
        if not fields:
            continue
        if fields[0] == "v":
            if len(fields) < 4:
                raise BridgeError("OBJ contains a malformed vertex")
            position = tuple(float(v) for v in fields[1:4])
            if not all(math.isfinite(v) for v in position):
                raise BridgeError("OBJ contains a non-finite vertex")
            vertices.append(position)
        elif fields[0] == "f":
            if len(fields) < 4:
                raise BridgeError("OBJ contains a polygon with fewer than three corners")
            face = []
            for token in fields[1:]:
                raw = int(token.split("/")[0])
                index = raw - 1 if raw > 0 else len(vertices) + raw
                if index < 0 or index >= len(vertices):
                    raise BridgeError("OBJ contains an invalid polygon point index")
                face.append(index)
            faces.append(tuple(face))
            face_lines.append(line_index)
    if not vertices or not faces:
        raise BridgeError("OBJ needs vertices and polygons")
    return lines, tuple(vertices), tuple(faces), tuple(face_lines)


def process_obj(source, destination, commands, session_factory=None):
    """Run UvCraft, validate point/corner topology, and replace only OBJ UV records."""
    if session_factory is None:
        import uvcraft
        session_factory = uvcraft.Session
    commands = parse_commands(commands)
    lines, vertices, faces, face_lines = read_obj(source)
    session = session_factory()
    session.load(Path(source))
    for command in commands:
        session.execute(dict(command))
    corners = [point for face in faces for point in face]
    if session.vertex_count() != len(vertices) or session.face_count() != len(faces):
        raise BridgeError("UvCraft changed OBJ point or polygon count")
    if session.corner_count() != len(corners) or session.corner_vertex() != corners:
        raise BridgeError("UvCraft changed OBJ corner topology")
    ranges = []
    start = 0
    for face in faces:
        ranges.append((start, start + len(face)))
        start += len(face)
    if [tuple(item) for item in session.face_corners()] != ranges:
        raise BridgeError("UvCraft changed OBJ polygon order")
    indices = session.corner_uv()
    uvs = session.uvs()
    if len(indices) != len(corners) or any(i < 0 or i >= len(uvs) for i in indices):
        raise BridgeError("UvCraft returned invalid UV indices")
    if any(len(uv) != 2 or not all(math.isfinite(float(v)) for v in uv) for uv in uvs):
        raise BridgeError("UvCraft returned invalid UVs")

    output = []
    first_face_line = face_lines[0]
    face_line_set = set(face_lines)
    face_number = 0
    for line_index, line in enumerate(lines):
        if line_index == first_face_line:
            output.extend(f"vt {float(u):.17g} {float(v):.17g}\n" for u, v in uvs)
        fields = line.split()
        if fields and fields[0] == "vt":
            continue
        if line_index in face_line_set:
            start, end = ranges[face_number]
            tokens = []
            for token, uv_index in zip(fields[1:], indices[start:end]):
                parts = token.split("/")
                normal = parts[2] if len(parts) > 2 else ""
                tokens.append(f"{parts[0]}/{uv_index + 1}/{normal}" if normal else
                              f"{parts[0]}/{uv_index + 1}")
            output.append("f " + " ".join(tokens) + "\n")
            face_number += 1
        else:
            output.append(line)
    Path(destination).write_text("".join(output), encoding="utf-8")
    return len(faces)


def run(commands, zbc=None, session_factory=None):
    """Export the active SubTool, process its OBJ, then import onto that SubTool."""
    if zbc is None:
        import zbrush.commands as zbc
    if zbc.get_subtool_count() < 1:
        raise BridgeError("ZBrush has no active SubTool")
    # Importing at an unknown or higher SDiv can replace the wrong mesh level.
    if not zbc.exists("Tool:Geometry:SDiv"):
        raise BridgeError("Cannot verify Tool > Geometry > SDiv; import cancelled")
    if float(zbc.get("Tool:Geometry:SDiv")) != 1.0:
        raise BridgeError("Set Tool > Geometry > SDiv to 1 before importing UVs")
    tool_path = zbc.get_active_tool_path()
    subtool_index = zbc.get_active_subtool_index()
    subtool_id = zbc.get_subtool_id()
    if not tool_path or subtool_index < 0 or subtool_id <= 0:
        raise BridgeError("Cannot identify the active SubTool; import cancelled")
    with TemporaryDirectory(prefix="uvcraft-zbrush-") as directory:
        source = Path(directory) / "source.obj"
        destination = Path(directory) / "uvcraft.obj"
        zbc.set_next_filename(str(source))
        zbc.press("Tool:Export")
        if not source.is_file():
            raise BridgeError("ZBrush did not export the active SubTool")
        count = process_obj(source, destination, commands, session_factory)
        if (zbc.get_active_tool_path() != tool_path
                or zbc.get_active_subtool_index() != subtool_index
                or zbc.get_subtool_id() != subtool_id):
            raise BridgeError("Active SubTool changed during UV processing; import cancelled")
        if not zbc.exists("Tool:Geometry:SDiv") or float(zbc.get("Tool:Geometry:SDiv")) != 1.0:
            raise BridgeError("Subdivision level changed during UV processing; import cancelled")
        zbc.set_next_filename(str(destination))
        zbc.press("Tool:Import")
        return count
