"""Remote protocol validation without a Blender runtime."""

import importlib
from pathlib import Path
import sys
from types import ModuleType, SimpleNamespace
import unittest

from test_bridge import Mesh


package = ModuleType("uvcraft_blender_remote_test")
package.__path__ = [str(Path(__file__).resolve().parents[1])]
sys.modules[package.__name__] = package
remote = importlib.import_module(package.__name__ + ".remote")


class RemoteTests(unittest.TestCase):
    def test_only_loopback_urls_are_accepted(self):
        for url in ("http://example.org:8766", "https://127.0.0.1:8766", "http://127.0.0.1:8766/path"):
            with self.assertRaises(remote.RemoteError):
                remote.Client(url, "token")
        self.assertEqual(remote.Client("http://127.0.0.1:8766").url,
                         "http://127.0.0.1:8766")

    def test_send_and_receive_validate_topology_before_uv_write(self):
        mesh = Mesh()
        mesh.edges = [SimpleNamespace(vertices=(0, 1), use_seam=False)]
        client = remote.Client(token="secret")
        calls = []

        def request(method, path, payload=None):
            calls.append((method, path, payload))
            if path == "/v1/sessions":
                return {"session_id": "abc"}
            return {
                "topology": {"vertices": 5, "faces": [[0, 1, 2], [1, 3, 4]]},
                "uvs": [[float(i), 0.5] for i in range(6)],
                "seams": [[0, 1]],
            }

        client.request = request
        self.assertEqual(client.send(mesh, "Cube"), "abc")
        self.assertIn("f 1/1 2/2 3/3", calls[0][2]["obj"])
        self.assertEqual(calls[0][2]["seams"], [])
        mesh.edges[0].use_seam = True
        client.update("abc", mesh)
        self.assertEqual(calls[1][2]["seams"], [[0, 1]])
        client.send(mesh, "Cube", preserve_existing_uvs=False)
        self.assertNotIn("\nvt ", calls[2][2]["obj"])
        self.assertIn("\nf 1 2 3", calls[2][2]["obj"])
        assignments, seams = client.receive_data("abc", mesh)
        self.assertEqual(assignments[4], (4, (4.0, 0.5)))
        self.assertEqual(seams, {(0, 1)})
        self.assertEqual(mesh.uv_layers.active.uv[4].vector, [4.0, 0.0])
        client.apply_seams(mesh, seams)
        self.assertTrue(mesh.edges[0].use_seam)

        def wrong(method, path, payload=None):
            value = request(method, path, payload)
            value["topology"]["faces"][1][1] = 4
            return value

        client.request = wrong
        with self.assertRaisesRegex(remote.RemoteError, "topology changed"):
            client.receive("abc", mesh)
        self.assertFalse(mesh.updated)


if __name__ == "__main__":
    unittest.main()
