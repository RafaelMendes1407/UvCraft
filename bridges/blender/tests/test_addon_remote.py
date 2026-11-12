"""Exercise the remote add-on workflow with Blender API doubles."""

import importlib.util
from pathlib import Path
import sys
from types import ModuleType, SimpleNamespace
import unittest

from test_bridge import Mesh


blender = ModuleType("bpy")
blender.types = SimpleNamespace(PropertyGroup=object, Operator=object, Panel=object, Scene=type("Scene", (), {}))
blender.props = ModuleType("bpy.props")
for name in ("BoolProperty", "EnumProperty", "PointerProperty", "StringProperty"):
    setattr(blender.props, name, lambda **kwargs: None)
sys.modules["bpy"] = blender
sys.modules["bpy.props"] = blender.props

directory = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "uvcraft_blender_addon_test", directory / "__init__.py",
    submodule_search_locations=[str(directory)],
)
addon = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = addon
spec.loader.exec_module(addon)


class Client:
    def __init__(self):
        self.sent = []
        self.disconnected = []
        self.fail_second_send = False

    def connect(self):
        return {"app_version": "0.0.1"}

    def send(self, mesh, name, version, preserve_existing_uvs):
        if self.fail_second_send and self.sent:
            raise addon.remote.RemoteError("second mesh failed")
        session_id = f"session-{len(self.sent)}"
        self.sent.append((mesh, name, version, preserve_existing_uvs))
        return session_id

    def disconnect(self, session_id):
        self.disconnected.append(session_id)

    def receive_data(self, session_id, mesh):
        return [(loop, (0.25, 0.75)) for loop in range(len(mesh.loops))], set()

    def status(self, session_id):
        mesh = self.sent[int(session_id.split("-")[1])][0]
        return {"topology": {
            "vertices": len(mesh.vertices),
            "faces": [[mesh.loops[loop].vertex_index for loop in poly.loop_indices]
                      for poly in mesh.polygons],
        }}

    def apply_seams(self, mesh, seams):
        pass


class AddonRemoteTests(unittest.TestCase):
    def setUp(self):
        addon.REMOTE_SESSIONS.clear()
        addon.REMOTE_STATUS.update(connected=False, last_sync="Never", topology="Not checked")
        self.client = Client()
        addon._remote_client = lambda settings: self.client

    def context(self, objects):
        return SimpleNamespace(
            selected_objects=objects,
            scene=SimpleNamespace(uvcraft_bridge_settings=SimpleNamespace(preserve_existing_uvs=True)),
        )

    def test_linked_objects_share_one_session_and_receive_only_uvs(self):
        mesh = Mesh()
        first = SimpleNamespace(name="Cube A", type="MESH", data=mesh)
        linked = SimpleNamespace(name="Cube B", type="MESH", data=mesh)
        context = self.context([first, linked])
        self.assertEqual(addon._remote_operation(context, "send"), 1)
        self.assertEqual(len(self.client.sent), 1)
        self.assertEqual(addon._remote_operation(context, "receive"), 1)
        self.assertEqual(addon._refresh_remote_status(context), 1)
        self.assertEqual(addon.REMOTE_STATUS["topology"], "Compatible")
        self.assertEqual(mesh.uv_layers.active.uv[0].vector, (0.25, 0.75))
        self.assertTrue(mesh.updated)

    def test_failed_multi_mesh_send_discards_new_remote_sessions(self):
        a, b = Mesh(), Mesh()
        context = self.context([
            SimpleNamespace(name="A", type="MESH", data=a),
            SimpleNamespace(name="B", type="MESH", data=b),
        ])
        self.client.fail_second_send = True
        with self.assertRaisesRegex(addon.remote.RemoteError, "second mesh failed"):
            addon._remote_operation(context, "send")
        self.assertEqual(addon.REMOTE_SESSIONS, {})
        self.assertEqual(self.client.disconnected, ["session-0"])

    def test_status_reports_topology_mismatch(self):
        mesh = Mesh()
        context = self.context([SimpleNamespace(name="Cube", type="MESH", data=mesh)])
        addon._remote_operation(context, "send")
        self.client.status = lambda session_id: {"topology": {"vertices": 0, "faces": []}}
        with self.assertRaisesRegex(addon.remote.RemoteError, "topology differs"):
            addon._refresh_remote_status(context)
        self.assertEqual(addon.REMOTE_STATUS["topology"], "Mismatch")


if __name__ == "__main__":
    unittest.main()
