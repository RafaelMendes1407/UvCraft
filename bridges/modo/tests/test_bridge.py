import importlib.util
from pathlib import Path
from types import SimpleNamespace
import unittest


spec = importlib.util.spec_from_file_location(
    "modo_bridge", Path(__file__).resolve().parents[1] / "bridge.py"
)
bridge = importlib.util.module_from_spec(spec)
import sys
sys.modules[spec.name] = bridge
spec.loader.exec_module(bridge)


class Buffer:
    def __init__(self):
        self.value = (0.0, 0.0)

    def get(self):
        return self.value

    def set(self, value):
        self.value = tuple(value)


class Points:
    def __init__(self, mesh):
        self.mesh = mesh
        self.index = 0

    def SelectByIndex(self, index):
        self.index = index

    def Select(self, point_id):
        self.index = point_id

    def Index(self):
        return self.index

    def Pos(self):
        return self.mesh.points[self.index]


class Polygons:
    def __init__(self, mesh):
        self.mesh = mesh
        self.index = 0

    def SelectByIndex(self, index):
        self.index = index

    def VertexCount(self):
        return len(self.mesh.faces[self.index])

    def VertexByIndex(self, index):
        return self.mesh.faces[self.index][index]

    def MapEvaluate(self, map_id, point_id, buffer):
        if self.mesh.uvs is None:
            return False
        index = self.mesh.faces[self.index].index(point_id)
        buffer.set(self.mesh.uvs[self.index][index])
        return True

    def SetMapValue(self, point_id, map_id, buffer):
        index = self.mesh.faces[self.index].index(point_id)
        self.mesh.uvs[self.index][index] = buffer.get()
        self.mesh.writes.append((self.index, index, buffer.get()))


class Maps:
    def __init__(self, mesh):
        self.mesh = mesh

    def SelectByName(self, map_type, name):
        if self.mesh.uvs is None:
            raise ValueError("map not found")

    def ID(self):
        return 123

    def New(self, map_type, name):
        self.mesh.uvs = [[(0.0, 0.0) for _ in face] for face in self.mesh.faces]
        return 123


class Mesh:
    def __init__(self, uv=True):
        self.points = [(0, 0, 0), (1, 0, 0), (1, 1, 0), (0, 1, 0)]
        self.faces = [(0, 1, 2), (0, 2, 3)]
        self.uvs = ([[(0, 0), (1, 0), (1, 1)],
                     [(0, 0), (0.8, 0.9), (0, 1)]] if uv else None)
        self.writes = []

    def PointCount(self):
        return len(self.points)

    def PolygonCount(self):
        return len(self.faces)

    def PointAccessor(self):
        return Points(self)

    def PolygonAccessor(self):
        return Polygons(self)

    def MeshMapAccessor(self):
        return Maps(self)


class Scan:
    def __init__(self, mesh):
        self.mesh = mesh
        self.changed = None
        self.applied = False

    def Count(self):
        return 1

    def MeshEdit(self, index):
        return self.mesh

    def SetMeshChange(self, index, change):
        self.changed = index, change

    def Apply(self):
        self.applied = True


def fake_lx(scan):
    return SimpleNamespace(
        symbol=SimpleNamespace(i_VMAP_TEXTUREUV=10, f_LAYERSCAN_EDIT=20,
                               f_MESHEDIT_MAP_UV=30),
        object=SimpleNamespace(storage=lambda kind, size: Buffer()),
        service=SimpleNamespace(Layer=lambda: SimpleNamespace(ScanAllocate=lambda flags: scan)),
    )


class Session:
    def __init__(self):
        self.commands = []

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
        return [0, 1, 2, 0, 2, 3]

    def face_corners(self):
        return [(0, 3), (3, 6)]

    def corner_uv(self):
        return list(range(6))

    def uvs(self):
        return [(0.1 * i, 0.2 * i) for i in range(6)]


class BridgeTests(unittest.TestCase):
    def test_reads_discontinuous_corner_uv_and_obj(self):
        mesh = Mesh()
        state = bridge.snapshot(mesh, fake_lx(Scan(mesh)))
        self.assertEqual(state.uvs[0][2], (1.0, 1.0))
        self.assertEqual(state.uvs[1][1], (0.8, 0.9))
        from tempfile import TemporaryDirectory
        with TemporaryDirectory() as directory:
            path = Path(directory) / "mesh.obj"
            bridge.write_obj(state, path)
            output = path.read_text()
            self.assertIn("f 1/1 2/2 3/3", output)
            self.assertIn("f 1/1 3/4 4/5", output)

    def test_run_writes_uv_only_and_commits(self):
        mesh = Mesh()
        scan = Scan(mesh)
        points, faces = list(mesh.points), list(mesh.faces)
        result = bridge.run({"cmd": "unfold"}, fake_lx(scan), session_factory=Session)
        self.assertEqual(result, 2)
        self.assertEqual(mesh.points, points)
        self.assertEqual(mesh.faces, faces)
        self.assertEqual(mesh.uvs[1][1], (0.4, 0.8))
        self.assertEqual(len(mesh.writes), 6)
        self.assertEqual(scan.changed, (0, 30))
        self.assertTrue(scan.applied)

    def test_selected_polygons_only(self):
        mesh = Mesh()
        lx = fake_lx(Scan(mesh))
        sessions = []
        def factory():
            result = Session()
            sessions.append(result)
            return result
        original = list(mesh.uvs[1])
        result = bridge.run([{"cmd": "unfold"}], lx, selected_polygons=[0],
                            session_factory=factory)
        self.assertEqual(result, 1)
        self.assertEqual(sessions[0].commands[0]["islands"], [0])
        self.assertEqual(mesh.uvs[1], original)
        self.assertEqual(len(mesh.writes), 3)

    def test_new_uv_map(self):
        mesh = Mesh(uv=False)
        scan = Scan(mesh)
        bridge.run({"cmd": "auto_seams"}, fake_lx(scan), session_factory=Session,
                   map_name="UvCraft")
        self.assertEqual(mesh.uvs[1][1], (0.4, 0.8))

    def test_reject_topology_change_before_host_write(self):
        class WrongSession(Session):
            def corner_vertex(self):
                return [0, 1, 2, 0, 3, 2]
        mesh = Mesh()
        scan = Scan(mesh)
        with self.assertRaisesRegex(bridge.BridgeError, "topology"):
            bridge.run({"cmd": "unfold"}, fake_lx(scan), session_factory=WrongSession)
        self.assertEqual(mesh.writes, [])
        self.assertFalse(scan.applied)

    def test_reject_changed_host_mesh_before_write(self):
        mesh = Mesh()
        lx = fake_lx(Scan(mesh))
        staged = bridge.prepare(mesh, {"cmd": "pack"}, lx, session_factory=Session)
        mesh.points[0] = (9, 0, 0)
        with self.assertRaisesRegex(bridge.BridgeError, "changed"):
            bridge.apply(mesh, staged, lx)
        self.assertEqual(mesh.writes, [])


if __name__ == "__main__":
    unittest.main()
