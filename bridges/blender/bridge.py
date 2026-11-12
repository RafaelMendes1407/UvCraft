"""Topology-preserving Blender mesh ↔ UvCraft transfer.

This module has no Blender dependency, so its transfer rules can be tested with
small mesh doubles. The addon in __init__.py supplies bpy objects and uvcraft.
"""

from __future__ import annotations

import json
import math
from pathlib import Path
from tempfile import TemporaryDirectory


class BridgeError(ValueError):
    pass


def _uv_get(layer, index):
    values = getattr(layer, "uv", None)
    if values is not None:
        return tuple(values[index].vector)
    return tuple(layer.data[index].uv)


def _uv_set(layer, index, value):
    values = getattr(layer, "uv", None)
    if values is not None:
        values[index].vector = value
    else:
        layer.data[index].uv = value


def selected_uv_loops(mesh):
    """Return selected UV corner indices on modern and legacy Blender layers."""
    layer = mesh.uv_layers.active
    if layer is None:
        raise BridgeError("mesh has no active UV layer")
    for attr in ("vertex_selection", "vertex_select"):
        values = getattr(layer, attr, None)
        if values is not None:
            return {i for i, item in enumerate(values) if item.value}
    return {i for i, item in enumerate(layer.data) if item.select}


def snapshot(mesh):
    """Capture ordered polygon corners and UVs from an object-mode mesh."""
    faces = [tuple(poly.loop_indices) for poly in mesh.polygons]
    if not faces or any(len(face) < 3 for face in faces):
        raise BridgeError("mesh needs polygon faces with at least three corners")
    corners = [mesh.loops[loop].vertex_index for face in faces for loop in face]
    if sorted(loop for face in faces for loop in face) != list(range(len(mesh.loops))):
        raise BridgeError("polygon loops are not a one-to-one ordered corner list")
    coords = [tuple(vertex.co) for vertex in mesh.vertices]
    if any(len(point) < 3 or not all(math.isfinite(float(x)) for x in point[:3]) for point in coords):
        raise BridgeError("mesh contains non-finite vertex positions")
    layer = mesh.uv_layers.active
    uv = None if layer is None else [_uv_get(layer, loop) for loop in range(len(mesh.loops))]
    if uv is not None and any(len(point) < 2 or not all(math.isfinite(float(x)) for x in point[:2]) for point in uv):
        raise BridgeError("mesh contains non-finite UVs")
    return faces, corners, coords, uv


def uv_vertex_indices(mesh, faces, uv):
    """Give continuous UV corners one OBJ vt index; retain Blender seam cuts."""
    parent = list(range(len(mesh.loops)))

    def root(index):
        while parent[index] != index:
            parent[index] = parent[parent[index]]
            index = parent[index]
        return index

    def join(a, b):
        parent[root(b)] = root(a)

    if hasattr(mesh, "edges") and all(hasattr(loop, "edge_index") for loop in mesh.loops):
        uses = {}
        for face in faces:
            for pos, start in enumerate(face):
                end = face[(pos + 1) % len(face)]
                uses.setdefault(mesh.loops[start].edge_index, []).append((start, end))
        for edge_index, sides in uses.items():
            if len(sides) != 2 or mesh.edges[edge_index].use_seam:
                continue
            (a, b), (c, d) = sides
            if (mesh.loops[a].vertex_index == mesh.loops[d].vertex_index
                and mesh.loops[b].vertex_index == mesh.loops[c].vertex_index
                and uv[a] == uv[d] and uv[b] == uv[c]):
                join(a, d)
                join(b, c)
    else:
        # Useful for simple mesh doubles and older APIs without edge flags.
        seen = {}
        for loop, point in enumerate(uv):
            key = (mesh.loops[loop].vertex_index, point)
            if key in seen:
                join(loop, seen[key])
            else:
                seen[key] = loop

    indices = []
    values = []
    root_index = {}
    for loop in range(len(mesh.loops)):
        representative = root(loop)
        if representative not in root_index:
            root_index[representative] = len(values) + 1
            values.append(uv[loop])
        indices.append(root_index[representative])
    return indices, values


def write_obj(mesh, path: Path, preserve_existing_uvs=True):
    """Write one OBJ face per Blender polygon, preserving face and loop order."""
    faces, corners, coords, uv = snapshot(mesh)
    if not preserve_existing_uvs:
        uv = None
    lines = ["# UvCraft Blender bridge\n"]
    lines.extend(f"v {x:.17g} {y:.17g} {z:.17g}\n" for x, y, z, *_ in coords)
    if uv is not None:
        uv_indices, values = uv_vertex_indices(mesh, faces, uv)
        lines.extend(f"vt {u:.17g} {v:.17g}\n" for u, v, *_ in values)
    for face in faces:
        tokens = []
        for loop in face:
            vertex = mesh.loops[loop].vertex_index + 1
            tokens.append(f"{vertex}/{uv_indices[loop]}" if uv is not None else str(vertex))
        lines.append("f " + " ".join(tokens) + "\n")
    path.write_text("".join(lines), encoding="utf-8")
    return faces, corners


def commands_for_preset(preset, custom_json):
    if preset == "PACK":
        return [{"cmd": "pack"}]
    if preset == "UNFOLD_PACK":
        return [{"cmd": "unfold"}, {"cmd": "pack"}]
    if preset == "AUTO_SEAMS_UNFOLD_PACK":
        return [{"cmd": "auto_seams"}, {"cmd": "unfold"}, {"cmd": "pack"}]
    if preset != "CUSTOM":
        raise BridgeError(f"unknown command preset: {preset}")
    try:
        commands = json.loads(custom_json)
    except json.JSONDecodeError as exc:
        raise BridgeError(f"invalid command JSON: {exc}") from exc
    if isinstance(commands, dict):
        commands = [commands]
    if not isinstance(commands, list) or not commands or any(
        not isinstance(command, dict) or not isinstance(command.get("cmd"), str)
        for command in commands
    ):
        raise BridgeError("commands must be a nonempty JSON object or array of objects with cmd")
    return commands


def selected_islands(session, selected_faces):
    selected = set(selected_faces)
    return [i for i, faces in enumerate(session.islands()) if selected.intersection(faces)]


# Commands that accept an island selection. A custom command with its own
# islands field keeps the caller's explicit choice.
ISLAND_COMMANDS = {
    "auto_seams", "box_planar_seams", "unfold", "optimize", "refine", "pack", "pack_physical", "set_real_scale",
    "set_texel_density", "equalize_texel_density", "mirror", "orient", "align",
    "stack", "rectangularize", "transform", "project", "mosaic", "similar",
}


def run(mesh, preset="PACK", custom_json="", selected_faces=None, selected_loops=None, session_factory=None):
    """Run commands and return (loop-index, UV) assignments without mutating mesh."""
    if session_factory is None:
        import uvcraft
        session_factory = uvcraft.Session
    selected_faces = None if selected_faces is None else set(selected_faces)
    selected_loops = None if selected_loops is None else set(selected_loops)
    commands = commands_for_preset(preset, custom_json)
    with TemporaryDirectory(prefix="uvcraft-blender-") as directory:
        path = Path(directory) / "mesh.obj"
        faces, corners = write_obj(mesh, path)
        if selected_loops is not None:
            if not selected_loops or min(selected_loops) < 0 or max(selected_loops) >= len(mesh.loops):
                raise BridgeError("select at least one valid UV corner")
            selected_faces = {
                face_index for face_index, face in enumerate(faces)
                if selected_loops.intersection(face)
            }
        if selected_faces is not None:
            if not selected_faces or max(selected_faces) >= len(faces) or min(selected_faces) < 0:
                raise BridgeError("select at least one valid face")
        session = session_factory()
        session.load(path)
        for source in commands:
            command = dict(source)
            if selected_faces is not None and command["cmd"] in ISLAND_COMMANDS and "islands" not in command:
                islands = selected_islands(session, selected_faces)
                if not islands:
                    raise BridgeError("selected faces do not belong to a UV island")
                command["islands"] = islands
            session.execute(command)

        # Validate the whole topology before touching the Blender data block.
        if session.vertex_count() != len(mesh.vertices) or session.face_count() != len(faces):
            raise BridgeError("UvCraft changed the mesh vertex or face count")
        if session.corner_count() != len(corners) or session.corner_vertex() != corners:
            raise BridgeError("UvCraft corner topology differs from the Blender mesh")
        expected_ranges = []
        start = 0
        for face in faces:
            expected_ranges.append((start, start + len(face)))
            start += len(face)
        if [tuple(r) for r in session.face_corners()] != expected_ranges:
            raise BridgeError("UvCraft face order differs from the Blender mesh")
        indices = session.corner_uv()
        uvs = session.uvs()
        if len(indices) != len(corners) or any(
            index < 0 or index >= len(uvs) for index in indices
        ) or any(len(uv) != 2 or not all(math.isfinite(float(v)) for v in uv) for uv in uvs):
            raise BridgeError("UvCraft returned invalid UV coordinates")
        return [
            (loop, tuple(uvs[indices[corner]]))
            for face_index, face in enumerate(faces)
            if selected_faces is None or face_index in selected_faces
            for corner, loop in zip(range(expected_ranges[face_index][0], expected_ranges[face_index][1]), face)
            if selected_loops is None or loop in selected_loops
        ]


def apply(mesh, assignments):
    """Apply validated UV assignments to the active layer, creating it if needed."""
    if any(loop < 0 or loop >= len(mesh.loops) for loop, _ in assignments):
        raise BridgeError("UV assignment references an invalid loop")
    layer = mesh.uv_layers.active or mesh.uv_layers.new(name="UVMap")
    for loop, uv in assignments:
        _uv_set(layer, loop, uv)
    mesh.update()
