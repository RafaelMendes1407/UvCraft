"""Run with `python3 -m unittest discover -s bridges/cinema4d/tests`."""

import importlib.util
from pathlib import Path
from types import SimpleNamespace
import sys
import unittest


SPEC = importlib.util.spec_from_file_location(
    "uvcraft_cinema4d_bridge", Path(__file__).resolve().parents[1] / "bridge.py"
)
bridge = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = bridge
SPEC.loader.exec_module(bridge)


class Vector:
    def __init__(self, x, y, z):
        self.x, self.y, self.z = x, y, z


class Polygon:
    def __init__(self, a, b, c, d):
        self.a, self.b, self.c, self.d = a, b, c, d

    def IsTriangle(self):
        return self.c == self.d


class Tag:
    def __init__(self, count):
        self.data = [dict(zip("abcd", (Vector(0, 0, 0),) * 4)) for _ in range(count)]

    def GetDataCount(self):
        return len(self.data)

    def GetSlow(self, index):
        return self.data[index]

    def SetSlow(self, index, a, b, c, d):
        self.data[index] = dict(zip("abcd", (a, b, c, d)))


C4D = SimpleNamespace(Tuvw=1234, UVWTag=Tag, Vector=Vector,
                      UNDOTYPE_NEWOBJ=1, UNDOTYPE_CHANGE=2, MSG_UPDATE=3)


class Object:
    def __init__(self, mapped=True):
        self.points = [Vector(*value) for value in (
            (0, 0, 0), (1, 0, 0), (1, 1, 0), (0, 1, 0), (2, 0, 0),
        )]
        self.polygons = [Polygon(0, 1, 2, 2), Polygon(1, 4, 3, 2)]
        self.tag = Tag(2) if mapped else None
        self.message = None
        if mapped:
            self.tag.SetSlow(0, Vector(0, 0, 7), Vector(1, 0, 7),
                             Vector(1, 1, 7), Vector(1, 1, 7))
            self.tag.SetSlow(1, Vector(1, 0, 8), Vector(2, 0, 8),
                             Vector(0, 1, 8), Vector(1, 1, 8))

    def GetAllPoints(self):
        return self.points

    def GetAllPolygons(self):
        return self.polygons

    def GetTag(self, tag_type):
        return self.tag

    def InsertTag(self, tag):
        self.tag = tag

    def Message(self, message):
        self.message = message


class Document:
    def __init__(self):
        self.undo = []

    def AddUndo(self, kind, obj):
        self.undo.append((kind, obj))


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
        return [0, 1, 2, 1, 4, 3, 2] if not self.bad else [0] * 7

    def face_corners(self):
        return [(0, 3), (3, 7)]

    def corner_uv(self):
        return [0, 1, 2, 1, 4, 3, 2]

    def uvs(self):
        return [(float(i), 0.25) for i in range(5)]


class BridgeTests(unittest.TestCase):
    def test_roundtrip_preserves_tri_quad_and_third_uv_component(self):
        obj = Object()
        doc = Document()
        session = Session()
        old_points = obj.GetAllPoints()[:]
        old_polygons = obj.GetAllPolygons()[:]
        bridge.run(obj, {"cmd": "pack"}, C4D, session_factory=lambda: session, doc=doc)
        self.assertIn("f 1/1 2/2 3/3", session.obj)
        self.assertIn("f 2/2 5/4 4/5 3/3", session.obj)
        self.assertEqual((obj.tag.GetSlow(0)["c"].x, obj.tag.GetSlow(0)["d"].x), (2.0, 2.0))
        self.assertEqual(obj.tag.GetSlow(1)["a"].z, 8.0)
        self.assertEqual(obj.points, old_points)
        self.assertEqual(obj.polygons, old_polygons)
        self.assertEqual(doc.undo[0][0], C4D.UNDOTYPE_CHANGE)
        self.assertEqual(obj.message, C4D.MSG_UPDATE)

    def test_selected_polygon_only_and_island_target(self):
        obj = Object()
        session = Session()
        old = obj.tag.GetSlow(0)["a"].x
        bridge.run(obj, [{"cmd": "unfold"}, {"cmd": "pack"}], C4D,
                   selected_polygons={1}, session_factory=lambda: session)
        self.assertEqual(session.commands, [
            {"cmd": "unfold", "islands": [1]}, {"cmd": "pack", "islands": [1]},
        ])
        self.assertEqual(obj.tag.GetSlow(0)["a"].x, old)
        self.assertEqual(obj.tag.GetSlow(1)["b"].x, 4.0)

    def test_creates_uv_tag_when_missing(self):
        obj = Object(mapped=False)
        doc = Document()
        session = Session()
        bridge.run(obj, {"cmd": "unfold"}, C4D, session_factory=lambda: session, doc=doc)
        self.assertIsNotNone(obj.tag)
        self.assertEqual(obj.tag.GetDataCount(), 2)
        self.assertIn("f 1 2 3", session.obj)
        self.assertEqual(doc.undo[0][0], C4D.UNDOTYPE_NEWOBJ)

    def test_mismatched_topology_is_rejected_before_tag_write(self):
        obj = Object()
        session = Session(bad=True)
        original = obj.tag.GetSlow(0)["a"].x
        with self.assertRaisesRegex(bridge.BridgeError, "topology"):
            bridge.run(obj, {"cmd": "pack"}, C4D, session_factory=lambda: session)
        self.assertEqual(obj.tag.GetSlow(0)["a"].x, original)


if __name__ == "__main__":
    unittest.main()
