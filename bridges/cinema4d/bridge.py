"""Cinema 4D PolygonObject UVWTag ↔ UvCraft, without import-time c4d use."""

from __future__ import annotations

from dataclasses import dataclass
import json
import math
from pathlib import Path
from tempfile import TemporaryDirectory


class BridgeError(ValueError):
    pass


ISLAND_COMMANDS = {
    "auto_seams", "box_planar_seams", "unfold", "optimize", "refine", "pack",
    "pack_physical", "set_real_scale", "set_texel_density", "transform",
    "equalize_texel_density", "mirror", "orient", "align", "stack",
    "rectangularize", "project", "mosaic", "similar",
}


@dataclass(frozen=True)
class Snapshot:
    points: tuple
    faces: tuple
    uvw: tuple


@dataclass(frozen=True)
class Prepared:
    points: tuple
    faces: tuple
    uvw: tuple
    selected_polygons: frozenset | None


def _vector(value):
    return (float(value.x), float(value.y), float(value.z))


def snapshot(obj, c4d):
    """Read a PolygonObject's local points, CPolygons and per-corner UVW."""
    points = tuple(_vector(point) for point in obj.GetAllPoints())
    faces = []
    for polygon in obj.GetAllPolygons():
        face = (polygon.a, polygon.b, polygon.c) if polygon.IsTriangle() else (
            polygon.a, polygon.b, polygon.c, polygon.d
        )
        faces.append(tuple(int(index) for index in face))
    faces = tuple(faces)
    if not points or not faces or any(len(face) < 3 for face in faces):
        raise BridgeError("PolygonObject needs points and triangle or quad polygons")
    if any(not all(math.isfinite(value) for value in point) for point in points):
        raise BridgeError("PolygonObject contains non-finite point positions")
    if any(index < 0 or index >= len(points) for face in faces for index in face):
        raise BridgeError("PolygonObject contains invalid polygon point indices")
    tag = obj.GetTag(c4d.Tuvw)
    uvw = ()
    if tag is not None:
        if tag.GetDataCount() != len(faces):
            raise BridgeError("UVWTag polygon count differs from object polygon count")
        uvw = tuple(tuple(_vector(tag.GetSlow(i)[key]) for key in "abcd")
                    for i in range(len(faces)))
        if any(not all(math.isfinite(value) for value in corner)
               for polygon in uvw for corner in polygon):
            raise BridgeError("UVWTag contains non-finite coordinates")
    return Snapshot(points, faces, uvw)


def write_obj(state, path):
    """Export active triangle/quad corners; deduplicate continuous UV points."""
    lines = ["# UvCraft Cinema 4D bridge\n"]
    lines.extend(f"v {x:.17g} {y:.17g} {z:.17g}\n" for x, y, z in state.points)
    uv_refs = []
    if state.uvw:
        seen = {}
        values = []
        for face, polygon_uvw in zip(state.faces, state.uvw):
            refs = []
            for point, corner in zip(face, polygon_uvw):
                key = (point, corner[0], corner[1])
                if key not in seen:
                    seen[key] = len(values) + 1
                    values.append(corner)
                refs.append(seen[key])
            uv_refs.append(refs)
        lines.extend(f"vt {u:.17g} {v:.17g}\n" for u, v, _ in values)
    for index, face in enumerate(state.faces):
        tokens = [f"{point + 1}/{uv_refs[index][corner]}" if uv_refs else str(point + 1)
                  for corner, point in enumerate(face)]
        lines.append("f " + " ".join(tokens) + "\n")
    path.write_text("".join(lines), encoding="utf-8")


def parse_commands(commands):
    if isinstance(commands, str):
        try:
            commands = json.loads(commands)
        except json.JSONDecodeError as exc:
            raise BridgeError(f"invalid command JSON: {exc}") from exc
    if isinstance(commands, dict):
        commands = [commands]
    if not isinstance(commands, (tuple, list)) or not commands or any(
        not isinstance(command, dict) or not isinstance(command.get("cmd"), str)
        for command in commands
    ):
        raise BridgeError("commands must be an object or nonempty array with cmd strings")
    return commands


def prepare(obj, commands, c4d, selected_polygons=None, session_factory=None):
    """Execute UvCraft and stage validated UVW coordinates for each polygon."""
    if session_factory is None:
        import uvcraft
        session_factory = uvcraft.Session
    commands = parse_commands(commands)
    state = snapshot(obj, c4d)
    selected = None if selected_polygons is None else frozenset(selected_polygons)
    if selected is not None and (not selected or min(selected) < 0 or max(selected) >= len(state.faces)):
        raise BridgeError("select at least one valid polygon")
    with TemporaryDirectory(prefix="uvcraft-cinema4d-") as directory:
        path = Path(directory) / "mesh.obj"
        write_obj(state, path)
        session = session_factory()
        session.load(path)
        for source in commands:
            command = dict(source)
            if selected is not None and command["cmd"] in ISLAND_COMMANDS and "islands" not in command:
                command["islands"] = [island for island, faces in enumerate(session.islands())
                                      if selected.intersection(faces)]
            session.execute(command)

        corners = [point for face in state.faces for point in face]
        if session.vertex_count() != len(state.points) or session.face_count() != len(state.faces):
            raise BridgeError("UvCraft changed point or polygon count")
        if session.corner_count() != len(corners) or session.corner_vertex() != corners:
            raise BridgeError("UvCraft changed polygon corner topology")
        ranges = []
        start = 0
        for face in state.faces:
            ranges.append((start, start + len(face)))
            start += len(face)
        if [tuple(r) for r in session.face_corners()] != ranges:
            raise BridgeError("UvCraft changed polygon order")
        indices = session.corner_uv()
        uvs = session.uvs()
        if len(indices) != len(corners) or any(i < 0 or i >= len(uvs) for i in indices):
            raise BridgeError("UvCraft returned invalid UV indices")
        if any(len(uv) != 2 or not all(math.isfinite(float(value)) for value in uv) for uv in uvs):
            raise BridgeError("UvCraft returned invalid UVs")
        staged = []
        for face_index, (a, b) in enumerate(ranges):
            old = state.uvw[face_index] if state.uvw else ((0.0, 0.0, 0.0),) * 4
            current = []
            for offset, corner in enumerate(range(a, b)):
                u, v = uvs[indices[corner]]
                current.append((float(u), float(v), old[offset][2]))
            if len(current) == 3:
                current.append(current[2])
            staged.append(tuple(current))
        return Prepared(state.points, state.faces, tuple(staged), selected)


def apply(obj, prepared, c4d, doc=None):
    """Write only the UVWTag, optionally adding Cinema 4D undo records."""
    current = snapshot(obj, c4d)
    if current.points != prepared.points or current.faces != prepared.faces:
        raise BridgeError("PolygonObject changed after UV preparation")
    tag = obj.GetTag(c4d.Tuvw)
    if tag is None:
        tag = c4d.UVWTag(len(prepared.faces))
        if tag is None:
            raise BridgeError("Cinema 4D could not allocate UVWTag")
        obj.InsertTag(tag)
        if doc is not None:
            doc.AddUndo(c4d.UNDOTYPE_NEWOBJ, tag)
    elif doc is not None:
        doc.AddUndo(c4d.UNDOTYPE_CHANGE, tag)
    for index, coords in enumerate(prepared.uvw):
        if prepared.selected_polygons is None or index in prepared.selected_polygons:
            tag.SetSlow(index, *(c4d.Vector(*corner) for corner in coords))
    obj.Message(c4d.MSG_UPDATE)


def run(obj, commands, c4d, selected_polygons=None, session_factory=None, doc=None):
    prepared = prepare(obj, commands, c4d, selected_polygons, session_factory)
    apply(obj, prepared, c4d, doc)
    return len(prepared.faces) if selected_polygons is None else len(prepared.selected_polygons)
