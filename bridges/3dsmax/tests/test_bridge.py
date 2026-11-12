"""Run with `python3 -m unittest discover -s bridges/3dsmax/tests`."""

import importlib.util
from pathlib import Path
from types import SimpleNamespace
import unittest


SPEC = importlib.util.spec_from_file_location(
    "uvcraft_3dsmax_bridge", Path(__file__).resolve().parents[1] / "bridge.py"
)
bridge = importlib.util.module_from_spec(SPEC)
import sys
sys.modules[SPEC.name] = bridge
SPEC.loader.exec_module(bridge)


class Poly:
    def __init__(self, mapped=True):
        self.positions = [(0., 0., 0.), (1., 0., 0.), (1., 1., 0.),
                          (0., 1., 0.), (2., 0., 0.)]
        self.faces = [(1, 2, 3, 4), (2, 5, 3)]
        self.support = mapped
        self.map_verts = [(0., 0., 7.), (1., 0., 7.), (1., 1., 7.),
                          (0., 1., 7.), (2., 0., 7.)] if mapped else []
        self.map_faces = [(1, 2, 3, 4), (2, 5, 3)] if mapped else []


class PolyOp:
    def getNumVerts(self, poly):
        return len(poly.positions)

    def getVert(self, poly, i):
        return SimpleNamespace(**dict(zip("xyz", poly.positions[i - 1])))

    def getNumFaces(self, poly):
        return len(poly.faces)

    def getFaceVerts(self, poly, i):
        return poly.faces[i - 1]

    def getMapSupport(self, poly, channel):
        return poly.support

    def getNumMapVerts(self, poly, channel):
        return len(poly.map_verts)

    def getNumMapFaces(self, poly, channel):
        return len(poly.map_faces)

    def getMapVert(self, poly, channel, i):
        return SimpleNamespace(**dict(zip("xyz", poly.map_verts[i - 1])))

    def getMapFace(self, poly, channel, i):
        return poly.map_faces[i - 1]

    def setMapSupport(self, poly, channel, support):
        poly.support = support
        poly.map_faces = [()] * len(poly.faces)

    def setNumMapVerts(self, poly, channel, count):
        poly.map_verts = [(0., 0., 0.)] * count

    def setMapVert(self, poly, channel, i, value):
        poly.map_verts[i - 1] = (value.x, value.y, value.z)

    def setMapFace(self, poly, channel, i, face):
        poly.map_faces[i - 1] = tuple(face)


RT = SimpleNamespace(polyop=PolyOp(), point3=lambda x, y, z: SimpleNamespace(x=x, y=y, z=z),
                     array=lambda *values: list(values))


class Session:
    def __init__(self, bad=False):
        self.bad = bad
        self.commands = []

    def load(self, path):
        self.obj = Path(path).read_text()

    def execute(self, command):
        self.commands.append(command)

    def islands(self):
        return [[0], [1]]

    def vertex_count(self):
        return 5

    def face_count(self):
        return 2

    def corner_count(self):
        return 7

    def corner_vertex(self):
        return [0, 1, 2, 3, 1, 4, 2] if not self.bad else [0] * 7

    def face_corners(self):
        return [(0, 4), (4, 7)]

    def corner_uv(self):
        return [0, 1, 2, 3, 1, 4, 2]

    def uvs(self):
        return [(i + 0.5, 0.25) for i in range(5)]


class BridgeTests(unittest.TestCase):
    def test_roundtrip_keeps_ngon_and_shared_uv_map_vertices(self):
        poly = Poly()
        session = Session()
        original_faces = list(poly.faces)
        original_positions = list(poly.positions)
        bridge.run(poly, {"cmd": "pack"}, RT, session_factory=lambda: session)
        self.assertIn("f 1/1 2/2 3/3 4/4", session.obj)
        self.assertIn("f 2/2 5/5 3/3", session.obj)
        self.assertEqual(poly.map_faces, original_faces)
        self.assertEqual(poly.map_verts[1], (1.5, 0.25, 7.0))
        self.assertEqual(poly.faces, original_faces)
        self.assertEqual(poly.positions, original_positions)

    def test_selected_face_keeps_other_map_face_and_vertices(self):
        poly = Poly()
        session = Session()
        before = list(poly.map_verts)
        bridge.run(poly, '[{"cmd":"unfold"},{"cmd":"pack"}]', RT,
                   selected_faces={2}, session_factory=lambda: session)
        self.assertEqual(session.commands, [
            {"cmd": "unfold", "islands": [1]}, {"cmd": "pack", "islands": [1]},
        ])
        self.assertEqual(poly.map_faces[0], (1, 2, 3, 4))
        self.assertEqual(poly.map_verts[:5], before)
        self.assertGreater(len(poly.map_verts), 5)

    def test_creates_map_support_without_resizing_face_array(self):
        poly = Poly(mapped=False)
        session = Session()
        bridge.run(poly, {"cmd": "unfold"}, RT, session_factory=lambda: session)
        self.assertTrue(poly.support)
        self.assertEqual(len(poly.map_faces), 2)
        self.assertEqual(poly.map_faces[0], (1, 2, 3, 4))
        self.assertIn("f 1 2 3 4", session.obj)

    def test_topology_mismatch_rejects_before_map_write(self):
        poly = Poly()
        session = Session(bad=True)
        before = list(poly.map_verts)
        with self.assertRaisesRegex(bridge.BridgeError, "topology"):
            bridge.run(poly, {"cmd": "pack"}, RT, session_factory=lambda: session)
        self.assertEqual(poly.map_verts, before)

    def test_apply_rejects_poly_changed_after_preparation(self):
        poly = Poly()
        session = Session()
        prepared = bridge.prepare(poly, {"cmd": "pack"}, RT, session_factory=lambda: session)
        before = list(poly.map_verts)
        poly.faces[0] = (1, 3, 2, 4)
        with self.assertRaisesRegex(bridge.BridgeError, "changed"):
            bridge.apply(poly, prepared, RT)
        self.assertEqual(poly.map_verts, before)


if __name__ == "__main__":
    unittest.main()
