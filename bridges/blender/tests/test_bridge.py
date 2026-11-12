"""Run with `python -m unittest discover -s bridges/blender/tests`."""

import importlib.util
from pathlib import Path
from types import SimpleNamespace
import unittest


SPEC = importlib.util.spec_from_file_location(
    "uvcraft_blender_bridge", Path(__file__).resolve().parents[1] / "bridge.py"
)
bridge = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bridge)


class Layers:
    def __init__(self, count, active=True):
        self.count = count
        self.active = (
            SimpleNamespace(
                uv=[SimpleNamespace(vector=[float(i), 0.0]) for i in range(count)],
                vertex_selection=[SimpleNamespace(value=i in (1, 4)) for i in range(count)],
            )
            if active else None
        )

    def new(self, name):
        self.active = SimpleNamespace(
            uv=[SimpleNamespace(vector=[0.0, 0.0]) for _ in range(self.count)]
        )
        return self.active


class Mesh:
    def __init__(self, with_uv=True):
        self.vertices = [SimpleNamespace(co=p) for p in (
            (0.0, 0.0, 0.0), (1.0, 0.0, 0.0), (1.0, 1.0, 0.0),
            (2.0, 0.0, 0.0), (2.0, 1.0, 0.0),
        )]
        self.loops = [SimpleNamespace(vertex_index=v) for v in (0, 1, 2, 1, 3, 4)]
        self.polygons = [
            SimpleNamespace(loop_indices=range(0, 3), index=0, select=False),
            SimpleNamespace(loop_indices=range(3, 6), index=1, select=True),
        ]
        self.uv_layers = Layers(6, with_uv)
        self.updated = False

    def update(self):
        self.updated = True


class Session:
    def __init__(self):
        self.commands = []
        self.bad_topology = False

    def load(self, path):
        self.obj = Path(path).read_text()

    def islands(self):
        return [[0], [1]]

    def execute(self, command):
        self.commands.append(command)

    def vertex_count(self):
        return 5

    def face_count(self):
        return 2

    def corner_count(self):
        return 6

    def corner_vertex(self):
        return [0, 1, 2, 1, 3, 4] if not self.bad_topology else [0] * 6

    def face_corners(self):
        return [(0, 3), (3, 6)]

    def corner_uv(self):
        return list(range(6))

    def uvs(self):
        return [(float(i), 0.25) for i in range(6)]


class BridgeTests(unittest.TestCase):
    def test_obj_preserves_connected_uvs_and_explicit_seams(self):
        mesh = Mesh()
        mesh.loops = [SimpleNamespace(vertex_index=v, edge_index=e) for v, e in (
            (0, 0), (1, 1), (2, 2), (2, 1), (1, 3), (3, 4),
        )]
        mesh.polygons = [
            SimpleNamespace(loop_indices=range(0, 3), index=0, select=False),
            SimpleNamespace(loop_indices=range(3, 6), index=1, select=True),
        ]
        mesh.edges = [SimpleNamespace(use_seam=False) for _ in range(5)]
        uv = [(0, 0), (1, 0), (1, 1), (1, 1), (1, 0), (0, 1)]
        for entry, point in zip(mesh.uv_layers.active.uv, uv):
            entry.vector = point
        indices, values = bridge.uv_vertex_indices(mesh, [tuple(p.loop_indices) for p in mesh.polygons], uv)
        self.assertEqual(len(values), 4)
        self.assertEqual(indices[1], indices[4])
        self.assertEqual(indices[2], indices[3])
        mesh.edges[1].use_seam = True
        _, values = bridge.uv_vertex_indices(mesh, [tuple(p.loop_indices) for p in mesh.polygons], uv)
        self.assertEqual(len(values), 6)

    def test_obj_and_uv_round_trip_preserve_corner_order(self):
        mesh = Mesh()
        session = Session()
        assignments = bridge.run(mesh, session_factory=lambda: session)
        self.assertIn("f 1/1 2/2 3/3", session.obj)
        self.assertIn("f 2/4 4/5 5/6", session.obj)
        self.assertEqual(session.commands, [{"cmd": "pack"}])
        bridge.apply(mesh, assignments)
        self.assertEqual(mesh.uv_layers.active.uv[4].vector, (4.0, 0.25))
        self.assertTrue(mesh.updated)

    def test_selected_faces_target_island_and_only_apply_those_loops(self):
        mesh = Mesh()
        session = Session()
        assignments = bridge.run(mesh, "UNFOLD_PACK", selected_faces={1}, session_factory=lambda: session)
        self.assertEqual([loop for loop, _ in assignments], [3, 4, 5])
        self.assertEqual(session.commands, [
            {"cmd": "unfold", "islands": [1]}, {"cmd": "pack", "islands": [1]},
        ])
        before = list(mesh.uv_layers.active.uv[0].vector)
        bridge.apply(mesh, assignments)
        self.assertEqual(mesh.uv_layers.active.uv[0].vector, before)
        self.assertEqual(mesh.uv_layers.active.uv[3].vector, (3.0, 0.25))

    def test_topology_mismatch_rejects_before_mutation(self):
        mesh = Mesh()
        session = Session()
        session.bad_topology = True
        with self.assertRaisesRegex(bridge.BridgeError, "topology"):
            bridge.run(mesh, session_factory=lambda: session)
        self.assertEqual(mesh.uv_layers.active.uv[0].vector, [0.0, 0.0])
        self.assertFalse(mesh.updated)

    def test_selected_uv_corners_limit_writeback(self):
        mesh = Mesh()
        session = Session()
        selected = bridge.selected_uv_loops(mesh)
        self.assertEqual(selected, {1, 4})
        assignments = bridge.run(mesh, selected_loops=selected, session_factory=lambda: session)
        self.assertEqual([loop for loop, _ in assignments], [1, 4])
        self.assertEqual(session.commands, [{"cmd": "pack", "islands": [0, 1]}])
        bridge.apply(mesh, assignments)
        self.assertEqual(mesh.uv_layers.active.uv[0].vector, [0.0, 0.0])
        self.assertEqual(mesh.uv_layers.active.uv[4].vector, (4.0, 0.25))

    def test_mesh_without_uv_layer_can_receive_uvs(self):
        mesh = Mesh(with_uv=False)
        session = Session()
        assignments = bridge.run(mesh, session_factory=lambda: session)
        self.assertIn("f 1 2 3", session.obj)
        bridge.apply(mesh, assignments)
        self.assertEqual(mesh.uv_layers.active.uv[5].vector, (5.0, 0.25))


if __name__ == "__main__":
    unittest.main()
