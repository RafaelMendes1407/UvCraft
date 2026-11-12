"""Houdini polygon vertex UVs ↔ UvCraft, with no import-time HOM dependency."""

from __future__ import annotations

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
        raise BridgeError("commands must be a nonempty object or array with cmd strings")
    return commands


def snapshot(geometry, hou_module):
    """Return points, polygon primitives, ordered vertices and current UVs."""
    points = tuple(geometry.points())
    if not points:
        raise BridgeError("geometry has no points")
    point_indices = {point.number(): i for i, point in enumerate(points)}
    if len(point_indices) != len(points):
        raise BridgeError("duplicate point numbers")
    polygons = []
    for prim in geometry.prims():
        if prim.type() != hou_module.primType.Polygon:
            continue
        if not prim.isClosed():
            raise BridgeError(f"polygon {prim.number()} is an open curve")
        vertices = tuple(prim.vertices())
        if len(vertices) < 3:
            raise BridgeError(f"polygon {prim.number()} has fewer than three vertices")
        polygons.append((prim.number(), vertices))
    if not polygons:
        raise BridgeError("geometry has no closed polygon primitives")
    positions = [tuple(point.position())[:3] for point in points]
    if any(len(p) != 3 or not all(math.isfinite(float(x)) for x in p) for p in positions):
        raise BridgeError("geometry contains non-finite point positions")
    corner_vertices = [vertex for _, poly in polygons for vertex in poly]
    corner_points = [point_indices[vertex.point().number()] for vertex in corner_vertices]
    vertex_uv = geometry.findVertexAttrib("uv")
    point_uv = geometry.findPointAttrib("uv") if vertex_uv is None else None
    uv_attr = vertex_uv or point_uv
    if uv_attr is not None and uv_attr.size() not in (2, 3):
        raise BridgeError("uv attribute must have two or three float components")
    if uv_attr is not None and uv_attr.dataType() != hou_module.attribData.Float:
        raise BridgeError("uv attribute must contain floats")
    uv = None
    if uv_attr is not None:
        uv = [tuple((vertex if vertex_uv is not None else vertex.point()).attribValue(uv_attr))[:2]
              for vertex in corner_vertices]
        if any(len(value) != 2 or not all(math.isfinite(float(x)) for x in value) for value in uv):
            raise BridgeError("geometry contains non-finite UVs")
    return points, polygons, positions, corner_vertices, corner_points, uv


def write_obj(snapshot_data, path):
    """Export one face per polygon and shared vt indices for continuous UVs."""
    _, polygons, positions, _, corner_points, uv = snapshot_data
    lines = ["# UvCraft Houdini bridge\n"]
    lines.extend(f"v {x:.17g} {y:.17g} {z:.17g}\n" for x, y, z in positions)
    uv_indices = []
    if uv is not None:
        seen = {}
        values = []
        for point, coord in zip(corner_points, uv):
            key = (point, coord)
            if key not in seen:
                seen[key] = len(values) + 1
                values.append(coord)
            uv_indices.append(seen[key])
        lines.extend(f"vt {u:.17g} {v:.17g}\n" for u, v in values)
    corner = 0
    for _, vertices in polygons:
        tokens = []
        for _ in vertices:
            point = corner_points[corner] + 1
            tokens.append(f"{point}/{uv_indices[corner]}" if uv is not None else str(point))
            corner += 1
        lines.append("f " + " ".join(tokens) + "\n")
    path.write_text("".join(lines), encoding="utf-8")


def _islands_for_prims(session, polygons, selected_primitives):
    face_numbers = {face for face, (prim_number, _) in enumerate(polygons)
                    if prim_number in selected_primitives}
    return [island for island, faces in enumerate(session.islands())
            if face_numbers.intersection(faces)]


def prepare(geometry, commands, selected_primitives=None, session_factory=None, hou_module=None):
    """Run commands and validate all UVs before returning vertex assignments."""
    if hou_module is None:
        import hou as hou_module
    if session_factory is None:
        import uvcraft
        session_factory = uvcraft.Session
    commands = parse_commands(commands)
    selected_primitives = None if selected_primitives is None else set(selected_primitives)
    state = snapshot(geometry, hou_module)
    points, polygons, _, vertices, corner_points, _ = state
    if selected_primitives is not None and not selected_primitives.intersection(
        prim_number for prim_number, _ in polygons
    ):
        raise BridgeError("select at least one closed polygon primitive")
    with TemporaryDirectory(prefix="uvcraft-houdini-") as directory:
        path = Path(directory) / "geometry.obj"
        write_obj(state, path)
        session = session_factory()
        session.load(path)
        for source in commands:
            command = dict(source)
            if selected_primitives is not None and command["cmd"] in ISLAND_COMMANDS and "islands" not in command:
                command["islands"] = _islands_for_prims(session, polygons, selected_primitives)
            session.execute(command)

        if session.vertex_count() != len(points) or session.face_count() != len(polygons):
            raise BridgeError("UvCraft changed point or polygon count")
        if session.corner_count() != len(vertices) or session.corner_vertex() != corner_points:
            raise BridgeError("UvCraft changed polygon vertex topology")
        expected_ranges = []
        start = 0
        for _, polygon in polygons:
            expected_ranges.append((start, start + len(polygon)))
            start += len(polygon)
        if [tuple(value) for value in session.face_corners()] != expected_ranges:
            raise BridgeError("UvCraft changed polygon order")
        indices = session.corner_uv()
        uvs = session.uvs()
        if len(indices) != len(vertices) or any(i < 0 or i >= len(uvs) for i in indices):
            raise BridgeError("UvCraft returned invalid UV indices")
        if any(len(uv) != 2 or not all(math.isfinite(float(x)) for x in uv) for uv in uvs):
            raise BridgeError("UvCraft returned invalid UV coordinates")
        assignments = []
        corner = 0
        for prim_number, polygon in polygons:
            for vertex in polygon:
                if selected_primitives is None or prim_number in selected_primitives:
                    assignments.append((vertex, tuple(uvs[indices[corner]])))
                corner += 1
        return assignments


def apply(geometry, assignments, hou_module=None):
    """Write UVs to vertex attributes only. Existing UV w components survive."""
    if hou_module is None:
        import hou as hou_module
    attrib = geometry.findVertexAttrib("uv")
    created = attrib is None
    point_uv = geometry.findPointAttrib("uv") if created else None
    if attrib is None:
        # HOM does not allow point and vertex attributes with the same name.
        # Keep the source point UV under a unique backup name on SOP output.
        backup = None
        if point_uv is not None:
            backup = "uvcraft_source_uv"
            suffix = 2
            while geometry.findPointAttrib(backup) is not None or geometry.findVertexAttrib(backup) is not None:
                backup = f"uvcraft_source_uv_{suffix}"
                suffix += 1
            geometry.renamePointAttrib("uv", backup)
        try:
            attrib = geometry.addAttrib(hou_module.attribType.Vertex, "uv", (0.0, 0.0, 0.0))
        except Exception:
            if backup is not None:
                geometry.renamePointAttrib(backup, "uv")
            raise
    if attrib.size() not in (2, 3):
        raise BridgeError("vertex uv attribute must have two or three components")
    if attrib.dataType() != hou_module.attribData.Float:
        raise BridgeError("vertex uv attribute must contain floats")
    for vertex, uv in assignments:
        value = (float(uv[0]), float(uv[1]))
        if attrib.size() == 3:
            previous = (vertex.point().attribValue(point_uv) if point_uv is not None
                        else vertex.attribValue(attrib))
            value += (float(previous[2]),)
        vertex.setAttribValue(attrib, value)


def run(geometry, commands, selected_primitives=None, session_factory=None, hou_module=None):
    """Convenience entry point for a Python SOP. Returns edited vertex count."""
    assignments = prepare(geometry, commands, selected_primitives, session_factory, hou_module)
    apply(geometry, assignments, hou_module)
    return len(assignments)
