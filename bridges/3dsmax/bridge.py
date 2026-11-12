"""Editable Poly UV map channel ↔ UvCraft, using injected pymxs.runtime."""

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
    positions: tuple
    faces: tuple
    map_verts: tuple
    map_faces: tuple


@dataclass(frozen=True)
class Prepared:
    positions: tuple
    faces: tuple
    map_verts: tuple
    map_faces: tuple
    channel: int


def _point(point):
    return (float(point.x), float(point.y), float(point.z))


def snapshot(poly, rt, channel=1):
    """Read an Editable Poly base object without changing its geometry."""
    if channel < 1:
        raise BridgeError("channel must be a texture map channel (>= 1)")
    op = rt.polyop
    positions = tuple(_point(op.getVert(poly, i)) for i in range(1, op.getNumVerts(poly) + 1))
    faces = tuple(tuple(int(v) for v in op.getFaceVerts(poly, i))
                  for i in range(1, op.getNumFaces(poly) + 1))
    if not positions or not faces or any(len(face) < 3 for face in faces):
        raise BridgeError("Editable Poly needs live faces with at least three corners")
    if any(not all(math.isfinite(value) for value in point) for point in positions):
        raise BridgeError("poly contains non-finite vertex positions")
    if any(v < 1 or v > len(positions) for face in faces for v in face):
        raise BridgeError("poly contains invalid face vertex indices")
    map_verts = ()
    map_faces = ()
    if op.getMapSupport(poly, channel) and op.getNumMapVerts(poly, channel) > 0:
        if op.getNumMapFaces(poly, channel) != len(faces):
            raise BridgeError("mapping face count differs from polygon count")
        map_verts = tuple(_point(op.getMapVert(poly, channel, i))
                          for i in range(1, op.getNumMapVerts(poly, channel) + 1))
        map_faces = tuple(tuple(int(v) for v in op.getMapFace(poly, channel, i))
                          for i in range(1, len(faces) + 1))
        if any(len(face) != len(mapped) for face, mapped in zip(faces, map_faces)):
            raise BridgeError("map face degree differs from polygon face degree")
        if any(v < 1 or v > len(map_verts) for face in map_faces for v in face):
            raise BridgeError("poly contains invalid map vertex indices")
        if any(not all(math.isfinite(value) for value in point) for point in map_verts):
            raise BridgeError("poly contains non-finite UVW values")
    return Snapshot(positions, faces, map_verts, map_faces)


def write_obj(state, path):
    """Keep MAXScript's 1-based face and map-vertex indices in the OBJ."""
    lines = ["# UvCraft 3ds Max bridge\n"]
    lines.extend(f"v {x:.17g} {y:.17g} {z:.17g}\n" for x, y, z in state.positions)
    lines.extend(f"vt {u:.17g} {v:.17g}\n" for u, v, _ in state.map_verts)
    for face_index, face in enumerate(state.faces):
        mapped = state.map_faces[face_index] if state.map_faces else None
        tokens = [f"{v}/{mapped[i]}" if mapped else str(v) for i, v in enumerate(face)]
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
    if not isinstance(commands, (list, tuple)) or not commands or any(
        not isinstance(command, dict) or not isinstance(command.get("cmd"), str)
        for command in commands
    ):
        raise BridgeError("commands must be an object or nonempty array with cmd strings")
    return commands


def prepare(poly, commands, rt, channel=1, selected_faces=None, session_factory=None):
    """Run UvCraft, validate topology, and stage a complete map channel edit."""
    if session_factory is None:
        import uvcraft
        session_factory = uvcraft.Session
    commands = parse_commands(commands)
    state = snapshot(poly, rt, channel)
    selected_faces = None if selected_faces is None else set(selected_faces)
    if selected_faces is not None and (
        not selected_faces or min(selected_faces) < 1 or max(selected_faces) > len(state.faces)
    ):
        raise BridgeError("select at least one valid polygon face")
    with TemporaryDirectory(prefix="uvcraft-3dsmax-") as directory:
        path = Path(directory) / "mesh.obj"
        write_obj(state, path)
        session = session_factory()
        session.load(path)
        for source in commands:
            command = dict(source)
            if selected_faces is not None and command["cmd"] in ISLAND_COMMANDS and "islands" not in command:
                chosen = {face - 1 for face in selected_faces}
                command["islands"] = [island for island, faces in enumerate(session.islands())
                                      if chosen.intersection(faces)]
            session.execute(command)

        corner_vertices = [v - 1 for face in state.faces for v in face]
        if session.vertex_count() != len(state.positions) or session.face_count() != len(state.faces):
            raise BridgeError("UvCraft changed vertex or polygon count")
        if session.corner_count() != len(corner_vertices) or session.corner_vertex() != corner_vertices:
            raise BridgeError("UvCraft changed polygon corner topology")
        expected_ranges = []
        start = 0
        for face in state.faces:
            expected_ranges.append((start, start + len(face)))
            start += len(face)
        if [tuple(value) for value in session.face_corners()] != expected_ranges:
            raise BridgeError("UvCraft changed polygon face order")
        indices = session.corner_uv()
        uvs = session.uvs()
        if len(indices) != len(corner_vertices) or any(i < 0 or i >= len(uvs) for i in indices):
            raise BridgeError("UvCraft returned invalid UV indices")
        if any(len(uv) != 2 or not all(math.isfinite(float(v)) for v in uv) for uv in uvs):
            raise BridgeError("UvCraft returned invalid UV coordinates")

        # Preserve the third map component by taking the original corner's W.
        old_w = [state.map_verts[ref - 1][2] for face in state.map_faces for ref in face]
        if not old_w:
            old_w = [0.0] * len(corner_vertices)
        if selected_faces is None:
            new_verts = [(float(u), float(v), 0.0) for u, v in uvs]
            assigned_w = set()
            for corner, uv_index in enumerate(indices):
                if uv_index not in assigned_w:
                    u, v, _ = new_verts[uv_index]
                    new_verts[uv_index] = (u, v, old_w[corner])
                    assigned_w.add(uv_index)
            new_faces = tuple(tuple(indices[corner] + 1 for corner in range(a, b))
                              for a, b in expected_ranges)
        else:
            # Keep unselected map faces and vertices exactly as they were.
            new_verts = list(state.map_verts) or [(0.0, 0.0, 0.0)]
            new_faces = []
            added = {}
            for face_number, (a, b) in enumerate(expected_ranges, 1):
                if face_number not in selected_faces:
                    old_face = state.map_faces[face_number - 1] if state.map_faces else (1,) * (b - a)
                    new_faces.append(tuple(old_face))
                    continue
                mapped = []
                for corner in range(a, b):
                    key = indices[corner]
                    if key not in added:
                        added[key] = len(new_verts) + 1
                        u, v = uvs[key]
                        new_verts.append((float(u), float(v), old_w[corner]))
                    mapped.append(added[key])
                new_faces.append(tuple(mapped))
            new_faces = tuple(new_faces)
        return Prepared(state.positions, state.faces, tuple(new_verts), new_faces, channel)


def apply(poly, prepared, rt):
    """Write only one UV map channel after checking geometry is unchanged."""
    current = snapshot(poly, rt, prepared.channel)
    if current.positions != prepared.positions or current.faces != prepared.faces:
        raise BridgeError("Editable Poly changed after UV preparation")
    op = rt.polyop
    if not op.getMapSupport(poly, prepared.channel):
        op.setMapSupport(poly, prepared.channel, True)
    if op.getNumMapFaces(poly, prepared.channel) != len(prepared.faces):
        raise BridgeError("map face array cannot represent the polygon faces")
    op.setNumMapVerts(poly, prepared.channel, len(prepared.map_verts))
    for index, (u, v, w) in enumerate(prepared.map_verts, 1):
        op.setMapVert(poly, prepared.channel, index, rt.point3(u, v, w))
    for index, face in enumerate(prepared.map_faces, 1):
        op.setMapFace(poly, prepared.channel, index, rt.array(*face))


def run(poly, commands, rt, channel=1, selected_faces=None, session_factory=None):
    prepared = prepare(poly, commands, rt, channel, selected_faces, session_factory)
    apply(poly, prepared, rt)
    return len(prepared.faces)
