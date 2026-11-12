"""Run with `python3 -m unittest discover -s bridges/houdini/tests`."""

import importlib.util
from pathlib import Path
from types import SimpleNamespace
import unittest


SPEC = importlib.util.spec_from_file_location(
    "uvcraft_houdini_bridge", Path(__file__).resolve().parents[1] / "bridge.py"
)
bridge = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bridge)
HOU = SimpleNamespace(
    primType=SimpleNamespace(Polygon="Polygon"),
    attribType=SimpleNamespace(Vertex="Vertex"),
    attribData=SimpleNamespace(Float="Float"),
)


class Attrib:
    def __init__(self, size):
        self._size = size

    def size(self):
        return self._size

    def dataType(self):
        return "Float"


class Point:
    def __init__(self, number, position):
        self._number = number
        self._position = position

    def number(self):
        return self._number

    def position(self):
        return self._position

    def attribValue(self, attrib):
        return (float(self._number), 0.0, 9.0)


class Vertex:
    def __init__(self, point, uv):
        self._point = point
        self.uv = uv

    def point(self):
        return self._point

    def attribValue(self, attrib):
        return self.uv

    def setAttribValue(self, attrib, value):
        self.uv = value


class Prim:
    def __init__(self, number, vertices, kind="Polygon"):
        self._number = number
        self._vertices = vertices
        self._kind = kind

    def number(self):
        return self._number

    def vertices(self):
        return self._vertices

    def type(self):
        return self._kind

    def isClosed(self):
        return True


class Geometry:
    def __init__(self, has_uv=True, point_uv=False):
        self._points = [Point(i, p) for i, p in enumerate((
            (0.0, 0.0, 0.0), (1.0, 0.0, 0.0),
            (1.0, 1.0, 0.0), (0.0, 1.0, 0.0),
        ))]
        self._prims = [
            Prim(0, [Vertex(self._points[i], uv) for i, uv in zip(
                (0, 1, 2), ((0.0, 0.0, 7.0), (1.0, 0.0, 7.0), (1.0, 1.0, 7.0))
            )]),
            Prim(1, [Vertex(self._points[i], uv) for i, uv in zip(
                (2, 1, 3), ((1.0, 1.0, 8.0), (1.0, 0.0, 8.0), (0.0, 1.0, 8.0))
            )]),
            Prim(2, [], "Volume"),
        ]
        self.uv_attrib = Attrib(3) if has_uv else None
        self.point_uv_attrib = Attrib(3) if point_uv else None
        self.point_uv_name = "uv" if point_uv else None

    def points(self):
        return self._points

    def prims(self):
        return self._prims

    def findVertexAttrib(self, name):
        return self.uv_attrib

    def findPointAttrib(self, name):
        return self.point_uv_attrib if name == self.point_uv_name else None

    def renamePointAttrib(self, old, new):
        if old != self.point_uv_name:
            raise ValueError(old)
        self.point_uv_name = new

    def addAttrib(self, kind, name, default):
        if name == self.point_uv_name:
            raise ValueError("duplicate attribute name")
        self.uv_attrib = Attrib(len(default))
        for prim in self._prims[:2]:
            for vertex in prim.vertices():
                vertex.uv = default
        return self.uv_attrib


class Session:
    def __init__(self, bad=False):
        self.commands = []
        self.bad = bad

    def load(self, path):
        self.obj = Path(path).read_text()

    def execute(self, command):
        self.commands.append(command)

    def islands(self):
        return [[0], [1]]

    def vertex_count(self):
        return 4

    def face_count(self):
        return 2

    def corner_count(self):
        return 6

    def corner_vertex(self):
        return [0, 1, 2, 2, 1, 3] if not self.bad else [0] * 6

    def face_corners(self):
        return [(0, 3), (3, 6)]

    def corner_uv(self):
        return [0, 1, 2, 2, 1, 3]

    def uvs(self):
        return [(float(i), 0.5) for i in range(4)]


class BridgeTests(unittest.TestCase):
    def test_obj_preserves_polygon_order_and_uv_connectivity(self):
        geo = Geometry()
        session = Session()
        edited = bridge.run(geo, [{"cmd": "pack"}], session_factory=lambda: session, hou_module=HOU)
        self.assertEqual(edited, 6)
        self.assertEqual(session.obj.count("\nvt "), 4)
        self.assertIn("f 1/1 2/2 3/3", session.obj)
        self.assertIn("f 3/3 2/2 4/4", session.obj)
        self.assertEqual(geo.prims()[0].vertices()[0].uv, (0.0, 0.5, 7.0))
        self.assertEqual(geo.prims()[1].vertices()[0].uv, (2.0, 0.5, 8.0))
        self.assertEqual(len(geo.points()), 4)
        self.assertEqual(len(geo.prims()), 3)

    def test_selected_primitive_limits_command_and_writeback(self):
        geo = Geometry()
        session = Session()
        edited = bridge.run(geo, '[{"cmd":"unfold"},{"cmd":"pack"}]',
                            selected_primitives={1}, session_factory=lambda: session, hou_module=HOU)
        self.assertEqual(edited, 3)
        self.assertEqual(session.commands, [
            {"cmd": "unfold", "islands": [1]}, {"cmd": "pack", "islands": [1]},
        ])
        self.assertEqual(geo.prims()[0].vertices()[1].uv, (1.0, 0.0, 7.0))
        self.assertEqual(geo.prims()[1].vertices()[1].uv, (1.0, 0.5, 8.0))

    def test_rejects_topology_mismatch_before_writing(self):
        geo = Geometry()
        session = Session(bad=True)
        before = geo.prims()[0].vertices()[0].uv
        with self.assertRaisesRegex(bridge.BridgeError, "topology"):
            bridge.run(geo, {"cmd": "pack"}, session_factory=lambda: session, hou_module=HOU)
        self.assertEqual(geo.prims()[0].vertices()[0].uv, before)

    def test_creates_vertex_uv_attribute_when_missing(self):
        geo = Geometry(has_uv=False)
        session = Session()
        bridge.run(geo, {"cmd": "unfold"}, session_factory=lambda: session, hou_module=HOU)
        self.assertIsNotNone(geo.uv_attrib)
        self.assertEqual(geo.prims()[0].vertices()[1].uv, (1.0, 0.5, 0.0))
        self.assertIn("f 1 2 3", session.obj)

    def test_promotes_point_uv_and_preserves_third_component(self):
        geo = Geometry(has_uv=False, point_uv=True)
        session = Session()
        bridge.run(geo, {"cmd": "pack"}, session_factory=lambda: session, hou_module=HOU)
        self.assertIn("vt 1 0", session.obj)
        self.assertEqual(geo.prims()[0].vertices()[1].uv, (1.0, 0.5, 9.0))
        self.assertEqual(geo.point_uv_name, "uvcraft_source_uv")


if __name__ == "__main__":
    unittest.main()
