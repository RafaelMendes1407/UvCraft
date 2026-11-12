"""Modo mesh UV map ↔ UvCraft; imports the host only when run inside Modo."""

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
    uvs: tuple | None
    map_name: str


@dataclass(frozen=True)
class Prepared:
    source: Snapshot
    uvs: tuple
    selected_polygons: frozenset | None


def snapshot(mesh, lx, map_name="Texture"):
    """Read point positions, polygon winding and effective UVs per corner."""
    point = mesh.PointAccessor()
    polygon = mesh.PolygonAccessor()
    maps = mesh.MeshMapAccessor()
    try:
        maps.SelectByName(lx.symbol.i_VMAP_TEXTUREUV, map_name)
        map_id = maps.ID()
    except Exception:  # Modo raises when a named map does not exist.
        map_id = None
    buffer = lx.object.storage("f", 2)
    points = []
    for index in range(mesh.PointCount()):
        point.SelectByIndex(index)
        points.append(tuple(float(value) for value in point.Pos()))
    faces = []
    uvs = [] if map_id is not None else None
    missing_uv = False
    for face_index in range(mesh.PolygonCount()):
        polygon.SelectByIndex(face_index)
        face = []
        polygon_uv = []
        for corner_index in range(polygon.VertexCount()):
            point_id = polygon.VertexByIndex(corner_index)
            point.Select(point_id)
            face.append(int(point.Index()))
            if uvs is not None:
                if polygon.MapEvaluate(map_id, point_id, buffer):
                    polygon_uv.append(tuple(float(value) for value in buffer.get()))
                else:
                    missing_uv = True
        faces.append(tuple(face))
        if uvs is not None:
            uvs.append(tuple(polygon_uv))
    if not points or not faces or any(len(face) < 3 for face in faces):
        raise BridgeError("mesh needs points and polygons with at least three corners")
    if any(i < 0 or i >= len(points) for face in faces for i in face):
        raise BridgeError("mesh has invalid polygon point indices")
    if any(not math.isfinite(value) for position in points for value in position):
        raise BridgeError("mesh has non-finite point positions")
    if missing_uv:
        raise BridgeError("UV map has unassigned corners; assign all corners or use a new map name")
    if uvs is not None and any(
        len(uv) != 2 or not all(math.isfinite(value) for value in uv)
        for face in uvs for uv in face
    ):
        raise BridgeError("UV map has invalid coordinates")
    return Snapshot(tuple(points), tuple(faces), None if uvs is None else tuple(uvs), map_name)


def write_obj(state, path):
    lines = ["# UvCraft Modo bridge\n"]
    lines.extend(f"v {x:.17g} {y:.17g} {z:.17g}\n" for x, y, z in state.points)
    uv_refs = []
    if state.uvs is not None:
        seen = {}
        values = []
        for face, face_uvs in zip(state.faces, state.uvs):
            refs = []
            for point, uv in zip(face, face_uvs):
                key = (point, uv[0], uv[1])
                if key not in seen:
                    seen[key] = len(values) + 1
                    values.append(uv)
                refs.append(seen[key])
            uv_refs.append(refs)
        lines.extend(f"vt {u:.17g} {v:.17g}\n" for u, v in values)
    for index, face in enumerate(state.faces):
        vertices = [f"{point + 1}/{uv_refs[index][corner]}" if uv_refs else str(point + 1)
                    for corner, point in enumerate(face)]
        lines.append("f " + " ".join(vertices) + "\n")
    path.write_text("".join(lines), encoding="utf-8")


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


def prepare(mesh, commands, lx, map_name="Texture", selected_polygons=None,
            session_factory=None):
    """Run commands and stage UVs, without changing the Modo mesh."""
    if session_factory is None:
        import uvcraft
        session_factory = uvcraft.Session
    commands = parse_commands(commands)
    state = snapshot(mesh, lx, map_name)
    selected = None if selected_polygons is None else frozenset(selected_polygons)
    if selected is not None and (not selected or min(selected) < 0 or max(selected) >= len(state.faces)):
        raise BridgeError("select at least one valid polygon")
    with TemporaryDirectory(prefix="uvcraft-modo-") as directory:
        path = Path(directory) / "mesh.obj"
        write_obj(state, path)
        session = session_factory()
        session.load(path)
        for source in commands:
            command = dict(source)
            if selected is not None and command["cmd"] in ISLAND_COMMANDS and "islands" not in command:
                command["islands"] = [index for index, faces in enumerate(session.islands())
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
        if [tuple(item) for item in session.face_corners()] != ranges:
            raise BridgeError("UvCraft changed polygon order")
        indices = session.corner_uv()
        uvs = session.uvs()
        if len(indices) != len(corners) or any(i < 0 or i >= len(uvs) for i in indices):
            raise BridgeError("UvCraft returned invalid UV indices")
        if any(len(uv) != 2 or not all(math.isfinite(float(v)) for v in uv) for uv in uvs):
            raise BridgeError("UvCraft returned invalid UVs")
        staged = tuple(tuple(tuple(float(v) for v in uvs[indices[corner]])
                             for corner in range(a, b)) for a, b in ranges)
    return Prepared(state, staged, selected)


def apply(mesh, prepared, lx):
    """Write only UV map values, using Modo's polygon/point pair setter."""
    current = snapshot(mesh, lx, prepared.source.map_name)
    if current.points != prepared.source.points or current.faces != prepared.source.faces:
        raise BridgeError("mesh topology or point positions changed after preparation")
    maps = mesh.MeshMapAccessor()
    try:
        maps.SelectByName(lx.symbol.i_VMAP_TEXTUREUV, prepared.source.map_name)
        map_id = maps.ID()
    except Exception:
        map_id = maps.New(lx.symbol.i_VMAP_TEXTUREUV, prepared.source.map_name)
    polygon = mesh.PolygonAccessor()
    buffer = lx.object.storage("f", 2)
    written = 0
    for face_index, face_uvs in enumerate(prepared.uvs):
        if prepared.selected_polygons is not None and face_index not in prepared.selected_polygons:
            continue
        polygon.SelectByIndex(face_index)
        for corner_index, uv in enumerate(face_uvs):
            buffer.set(uv)
            polygon.SetMapValue(polygon.VertexByIndex(corner_index), map_id, buffer)
        written += 1
    return written


def run(commands, lx=None, map_name="Texture", selected_polygons=None,
        session_factory=None, layer_index=0):
    """Edit the indexed active mesh layer and commit UV map changes."""
    if lx is None:
        import lx as modo_lx
        lx = modo_lx
    scan = lx.service.Layer().ScanAllocate(lx.symbol.f_LAYERSCAN_EDIT)
    if layer_index < 0 or layer_index >= scan.Count():
        raise BridgeError("layer index is outside the active editable mesh layers")
    mesh = scan.MeshEdit(layer_index)
    prepared = prepare(mesh, commands, lx, map_name, selected_polygons, session_factory)
    count = apply(mesh, prepared, lx)
    scan.SetMeshChange(layer_index, lx.symbol.f_MESHEDIT_MAP_UV)
    scan.Apply()
    return count
