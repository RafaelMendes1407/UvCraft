"""Run UvCraft commands on selected Maya meshes without changing their geometry.

Install the ``uvcraft`` Python extension for Maya's Python interpreter, then add
this directory to Maya's Python path. Example in Maya's Python Script Editor::

    import uvcraft_maya
    uvcraft_maya.run({"cmd": "unfold", "border_weight": 2.0})
    uvcraft_maya.run([{"cmd": "auto_seams"}, {"cmd": "unfold"}, {"cmd": "pack"}])

Meshes are serialized as a temporary OBJ in object space. Only the selected
UV set is written back, by polygon corner; positions and face topology stay in
Maya. Other Maya data, including materials and the remaining UV sets, stays on
the original mesh.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any


@dataclass
class _Mesh:
    path: str
    fn: Any
    uv_set: str
    faces: list[tuple[int, ...]]
    uv_ids: list[list[int | None]]
    u: list[float]
    v: list[float]
    corner_start: int

    @property
    def corner_count(self) -> int:
        return sum(map(len, self.faces))


def _maya_modules():
    try:
        import maya.cmds as cmds
        import maya.api.OpenMaya as om
    except ImportError as exc:
        raise RuntimeError("UvCraft Maya bridge must run inside Maya") from exc
    return cmds, om


def _selected_meshes(cmds, om, paths, uv_set):
    if paths is None:
        paths = cmds.ls(selection=True, dag=True, type="mesh", noIntermediate=True, long=True) or []
    paths = list(dict.fromkeys(paths))
    if not paths:
        raise ValueError("select at least one polygon mesh")

    result = []
    corner_start = 0
    for path in paths:
        selection = om.MSelectionList()
        selection.add(path)
        fn = om.MFnMesh(selection.getDagPath(0))
        name = uv_set or fn.currentUVSetName()
        faces = [tuple(fn.getPolygonVertices(i)) for i in range(fn.numPolygons)]
        u, v = fn.getUVs(name)
        uv_ids = []
        for face, vertices in enumerate(faces):
            ids = []
            for corner in range(len(vertices)):
                try:
                    uv_id = int(fn.getPolygonUVid(face, corner, name))
                    ids.append(uv_id if uv_id >= 0 else None)
                except RuntimeError:
                    ids.append(None)
            uv_ids.append(ids)
        item = _Mesh(path, fn, name, faces, uv_ids, list(u), list(v), corner_start)
        result.append(item)
        corner_start += item.corner_count
    return result


def _write_obj(path: Path, meshes, om):
    vertex_offset = 0
    uv_offset = 0
    lines = ["# Temporary UvCraft Maya exchange file\n"]
    for index, mesh in enumerate(meshes):
        lines.append("o maya_mesh_%d\n" % index)
        for p in mesh.fn.getPoints(om.MSpace.kObject):
            lines.append("v %.17g %.17g %.17g\n" % (p.x, p.y, p.z))
        for u, v in zip(mesh.u, mesh.v):
            lines.append("vt %.17g %.17g\n" % (u, v))
        for vertices, uv_ids in zip(mesh.faces, mesh.uv_ids):
            corners = []
            for vertex, uv_id in zip(vertices, uv_ids):
                obj_vertex = vertex_offset + vertex + 1
                corners.append(
                    str(obj_vertex) if uv_id is None else "%d/%d" % (obj_vertex, uv_offset + uv_id + 1)
                )
            lines.append("f %s\n" % " ".join(corners))
        vertex_offset += mesh.fn.numVertices
        uv_offset += len(mesh.u)
    path.write_text("".join(lines), encoding="utf-8")


def _apply_mesh(mesh: _Mesh, corner_uv, uvs):
    # The same UvCraft UV index may appear on several corners; remap it to a
    # compact Maya UV list for this mesh while keeping seam splits distinct.
    local = {}
    u_values = []
    v_values = []
    assignments = []
    corner = mesh.corner_start
    had_any_uv = any(uv is not None for face in mesh.uv_ids for uv in face)
    for face, old_ids in enumerate(mesh.uv_ids):
        for vertex_index, old_id in enumerate(old_ids):
            global_id = corner_uv[corner]
            corner += 1
            if old_id is None and had_any_uv:
                continue  # Keep sparse UV assignments sparse.
            if global_id not in local:
                local[global_id] = len(u_values)
                u, v = uvs[global_id]
                u_values.append(float(u))
                v_values.append(float(v))
            assignments.append((face, vertex_index, local[global_id]))

    mesh.fn.clearUVs(mesh.uv_set)
    if u_values:
        mesh.fn.setUVs(u_values, v_values, mesh.uv_set)
        for face, vertex_index, uv_id in assignments:
            mesh.fn.assignUV(face, vertex_index, uv_id, mesh.uv_set)


def _restore_mesh(mesh: _Mesh):
    mesh.fn.clearUVs(mesh.uv_set)
    if mesh.u:
        mesh.fn.setUVs(mesh.u, mesh.v, mesh.uv_set)
        for face, ids in enumerate(mesh.uv_ids):
            for corner, uv_id in enumerate(ids):
                if uv_id is not None:
                    mesh.fn.assignUV(face, corner, uv_id, mesh.uv_set)


def run(commands, *, meshes=None, uv_set=None, session_factory=None, cmds=None, om=None):
    """Execute one or more UvCraft command dictionaries on selected Maya meshes.

    Returns the command result (or a list of results). A failed UvCraft command
    leaves Maya untouched. If applying UVs fails, original UVs are restored.
    ``meshes`` may contain explicit Maya mesh paths; ``uv_set`` defaults to each
    mesh's current UV set. ``cmds``/``om`` and ``session_factory`` support tests.
    """
    if cmds is None or om is None:
        maya_cmds, maya_om = _maya_modules()
        cmds = cmds or maya_cmds
        om = om or maya_om
    if session_factory is None:
        import uvcraft
        session_factory = uvcraft.Session

    single = not isinstance(commands, (list, tuple))
    commands = [commands] if single else list(commands)
    items = _selected_meshes(cmds, om, meshes, uv_set)
    with TemporaryDirectory(prefix="uvcraft-maya-") as temporary:
        obj = Path(temporary) / "selection.obj"
        _write_obj(obj, items, om)
        session = session_factory()
        session.load(str(obj))
        if session.face_count != sum(len(item.faces) for item in items):
            raise RuntimeError("UvCraft changed the face count while importing Maya geometry")
        total_corners = sum(item.corner_count for item in items)
        if session.corner_count != total_corners:
            raise RuntimeError("UvCraft changed the corner count while importing Maya geometry")
        before_ids = session.corner_uv()
        before_uvs = session.uvs()
        results = [session.execute(command) for command in commands]
        after_ids = session.corner_uv()
        after_uvs = session.uvs()
        if len(after_ids) != total_corners:
            raise RuntimeError("UvCraft command changed mesh topology")
        if before_ids != after_ids or before_uvs != after_uvs:
            try:
                for item in items:
                    _apply_mesh(item, after_ids, after_uvs)
            except Exception:
                for item in items:
                    _restore_mesh(item)
                raise
    return results[0] if single else results
