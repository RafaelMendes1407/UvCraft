import importlib.util
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest


spec = importlib.util.spec_from_file_location(
    "zbrush_bridge", Path(__file__).resolve().parents[1] / "bridge.py"
)
bridge = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = bridge
spec.loader.exec_module(bridge)


SOURCE = """# ZBrush export
v 0 0 0
v 1 0 0
v 1 1 0
v 0 1 0
vt 0 0
vt 1 0
vn 0 0 1
g Sculpt
f 1/1/1 2/2/1 3/2/1
f 1/1/1 3/2/1 4/1/1
"""


class Session:
    def __init__(self):
        self.commands = []

    def load(self, path):
        self.source = Path(path).read_text()

    def execute(self, command):
        self.commands.append(command)

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
        return [0, 1, 2, 0, 3, 4]

    def uvs(self):
        return [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0),
                (0.8, 0.9), (0.0, 1.0)]


class Commands:
    def __init__(self, subdivision_level=1, has_sdiv=True):
        self.next_file = None
        self.calls = []
        self.imported = None
        self.subdivision_level = subdivision_level
        self.has_sdiv = has_sdiv
        self.active_path = "Sculpt/Head"
        self.active_subtool_index = 0
        self.active_subtool_id = 101

    def exists(self, path):
        return path == "Tool:Geometry:SDiv" and self.has_sdiv

    def get_active_tool_path(self):
        return self.active_path

    def get_active_subtool_index(self):
        return self.active_subtool_index

    def get_subtool_id(self):
        return self.active_subtool_id

    def get(self, path):
        assert path == "Tool:Geometry:SDiv"
        return float(self.subdivision_level)

    def get_subtool_count(self):
        return 1

    def set_next_filename(self, path):
        self.next_file = Path(path)

    def press(self, path):
        self.calls.append(path)
        if path == "Tool:Export":
            self.next_file.write_text(SOURCE)
        elif path == "Tool:Import":
            self.imported = self.next_file.read_text()


class BridgeTests(unittest.TestCase):
    def test_obj_uv_replacement_preserves_geometry_and_normals(self):
        with TemporaryDirectory() as directory:
            source = Path(directory) / "source.obj"
            destination = Path(directory) / "result.obj"
            source.write_text(SOURCE)
            self.assertEqual(bridge.process_obj(source, destination, {"cmd": "pack"}, Session), 2)
            result = destination.read_text()
            self.assertEqual(bridge.read_obj(source)[1:3], bridge.read_obj(destination)[1:3])
            self.assertIn("vn 0 0 1", result)
            self.assertIn("g Sculpt", result)
            self.assertIn("f 1/1/1 3/4/1 4/5/1", result)
            self.assertNotIn("vt 1 0\nvt 0", result)

    def test_host_export_import_flow(self):
        zbc = Commands()
        count = bridge.run([{"cmd": "unfold"}], zbc, Session)
        self.assertEqual(count, 2)
        self.assertEqual(zbc.calls, ["Tool:Export", "Tool:Import"])
        self.assertIn("f 1/1/1 3/4/1 4/5/1", zbc.imported)

    def test_rejects_higher_subdivision_before_export(self):
        zbc = Commands(subdivision_level=2)
        with self.assertRaisesRegex(bridge.BridgeError, "SDiv to 1"):
            bridge.run([{"cmd": "unfold"}], zbc, Session)
        self.assertEqual(zbc.calls, [])

    def test_rejects_unknown_subdivision_before_export(self):
        zbc = Commands(has_sdiv=False)
        with self.assertRaisesRegex(bridge.BridgeError, "Cannot verify"):
            bridge.run({"cmd": "unfold"}, zbc, Session)
        self.assertEqual(zbc.calls, [])

    def test_rejects_changed_subtool_before_import(self):
        zbc = Commands()

        class SwitchingSession(Session):
            def execute(self, command):
                super().execute(command)
                zbc.active_path = "Sculpt/Other"

        with self.assertRaisesRegex(bridge.BridgeError, "Active SubTool changed"):
            bridge.run({"cmd": "unfold"}, zbc, SwitchingSession)
        self.assertEqual(zbc.calls, ["Tool:Export"])

    def test_rejects_changed_subtool_inside_same_tool(self):
        zbc = Commands()

        class SwitchingSession(Session):
            def execute(self, command):
                super().execute(command)
                zbc.active_subtool_index = 1
                zbc.active_subtool_id = 202

        with self.assertRaisesRegex(bridge.BridgeError, "Active SubTool changed"):
            bridge.run({"cmd": "unfold"}, zbc, SwitchingSession)
        self.assertEqual(zbc.calls, ["Tool:Export"])

    def test_rejects_replaced_subtool_at_same_index(self):
        zbc = Commands()

        class ReplacingSession(Session):
            def execute(self, command):
                super().execute(command)
                zbc.active_subtool_id = 202

        with self.assertRaisesRegex(bridge.BridgeError, "Active SubTool changed"):
            bridge.run({"cmd": "unfold"}, zbc, ReplacingSession)
        self.assertEqual(zbc.calls, ["Tool:Export"])

    def test_rejects_changed_subdivision_before_import(self):
        zbc = Commands()

        class SwitchingSession(Session):
            def execute(self, command):
                super().execute(command)
                zbc.subdivision_level = 2

        with self.assertRaisesRegex(bridge.BridgeError, "Subdivision level changed"):
            bridge.run({"cmd": "unfold"}, zbc, SwitchingSession)
        self.assertEqual(zbc.calls, ["Tool:Export"])

    def test_reject_topology_change_before_import(self):
        class WrongSession(Session):
            def corner_vertex(self):
                return [0, 1, 2, 0, 3, 2]
        zbc = Commands()
        with self.assertRaisesRegex(bridge.BridgeError, "topology"):
            bridge.run({"cmd": "unfold"}, zbc, WrongSession)
        self.assertEqual(zbc.calls, ["Tool:Export"])

    def test_negative_indices(self):
        with TemporaryDirectory() as directory:
            path = Path(directory) / "negative.obj"
            path.write_text("v 0 0 0\nv 1 0 0\nv 0 1 0\nf -3 -2 -1\n")
            self.assertEqual(bridge.read_obj(path)[2], ((0, 1, 2),))

    def test_invalid_command(self):
        with self.assertRaisesRegex(bridge.BridgeError, "commands"):
            bridge.parse_commands([])


if __name__ == "__main__":
    unittest.main()
