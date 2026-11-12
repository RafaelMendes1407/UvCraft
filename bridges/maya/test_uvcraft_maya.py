"""Bridge tests that run with plain CPython, without Maya or the extension."""

import unittest
from pathlib import Path

import uvcraft_maya


class Point:
    def __init__(self, x, y, z=0.0):
        self.x, self.y, self.z = x, y, z


class FakeMesh:
    def __init__(self, points, faces, uvs, assignments):
        self.points = [Point(*point) for point in points]
        self.faces = faces
        self.u, self.v = map(list, zip(*uvs)) if uvs else ([], [])
        self.assignments = [list(face) for face in assignments]
        self.other_uv_set = [(0.25, 0.75)]
        self.fail_assignment = False

    @property
    def numVertices(self):
        return len(self.points)

    @property
    def numPolygons(self):
        return len(self.faces)

    def currentUVSetName(self):
        return "map1"

    def getPolygonVertices(self, face):
        return self.faces[face]

    def getPolygonUVid(self, face, corner, uv_set):
        assert uv_set == "map1"
        uv_id = self.assignments[face][corner]
        if uv_id is None:
            raise RuntimeError("unassigned UV")
        return uv_id

    def getUVs(self, uv_set):
        assert uv_set == "map1"
        return self.u, self.v

    def getPoints(self, space):
        assert space == "object"
        return self.points

    def clearUVs(self, uv_set):
        self.u, self.v = [], []
        self.assignments = [[None] * len(face) for face in self.faces]

    def setUVs(self, u, v, uv_set):
        self.u, self.v = list(u), list(v)

    def assignUV(self, face, corner, uv_id, uv_set):
        if self.fail_assignment:
            self.fail_assignment = False
            raise RuntimeError("Maya assignment failed")
        self.assignments[face][corner] = uv_id


class FakeCmds:
    def __init__(self, selection):
        self.selection = selection

    def ls(self, **kwargs):
        assert kwargs["selection"] and kwargs["type"] == "mesh"
        return self.selection


class FakeOM:
    def __init__(self, meshes):
        self.meshes = meshes
        self.MSpace = type("MSpace", (), {"kObject": "object"})
        owner = self

        class Selection:
            def add(self, path):
                self.path = path

            def getDagPath(self, index):
                assert index == 0
                return self.path

        self.MSelectionList = Selection
        self.MFnMesh = lambda path: owner.meshes[path]


class FakeSession:
    def load(self, path):
        lines = Path(path).read_text().splitlines()
        self.obj = lines
        uvs = []
        for line in lines:
            if line.startswith("vt "):
                _, u, v = line.split()
                uvs.append((float(u), float(v)))
        ids = []
        faces = 0
        for line in lines:
            if line.startswith("f "):
                faces += 1
                for token in line.split()[1:]:
                    if "/" in token:
                        ids.append(int(token.split("/")[1]) - 1)
                    else:
                        ids.append(len(uvs))
                        uvs.append((0.0, 0.0))
        self.face_count = faces
        self.corner_count = len(ids)
        self.ids, self.points = ids, uvs

    def corner_uv(self):
        return list(self.ids)

    def uvs(self):
        return list(self.points)

    def execute(self, command):
        if command["cmd"] == "fail":
            raise ValueError("invalid command")
        if command["cmd"] == "shift":
            amount = command.get("amount", 1.0)
            self.points = [(u + amount, v) for u, v in self.points]
        elif command["cmd"] == "cut":
            original = self.ids[1]
            self.ids[1] = len(self.points)
            self.points.append(self.points[original])
        return {"status": "done"}


class BridgeTests(unittest.TestCase):
    def fixtures(self):
        first = FakeMesh(
            [(0, 0, 0), (1, 0, 0), (1, 1, 0), (0, 1, 0)],
            [(0, 1, 2), (0, 2, 3)],
            [(0, 0), (1, 0), (1, 1), (0, 1), (1, 1)],
            [[0, 1, 2], [0, 4, 3]],
        )
        second = FakeMesh(
            [(2, 0, 0), (3, 0, 0), (2, 1, 0)],
            [(0, 1, 2)],
            [(0, 0), (1, 0), (0, 1)],
            [[0, None, 2]],
        )
        meshes = {"|a|mesh": first, "|b|mesh": second}
        return first, second, FakeCmds(list(meshes)), FakeOM(meshes)

    def test_multi_mesh_round_trip_keeps_topology_seams_and_sparse_corners(self):
        first, second, cmds, om = self.fixtures()
        points = [list(mesh.points) for mesh in (first, second)]
        faces = [list(mesh.faces) for mesh in (first, second)]
        result = uvcraft_maya.run(
            [{"cmd": "shift", "amount": 0.5}, {"cmd": "cut"}],
            cmds=cmds, om=om, session_factory=FakeSession,
        )
        self.assertEqual(result, [{"status": "done"}, {"status": "done"}])
        self.assertEqual([first.points, second.points], points)
        self.assertEqual([first.faces, second.faces], faces)
        self.assertEqual(first.assignments[0][0], first.assignments[1][0])
        self.assertNotEqual(first.assignments[0][2], first.assignments[1][1])
        self.assertIsNone(second.assignments[0][1])
        self.assertEqual(second.other_uv_set, [(0.25, 0.75)])
        self.assertAlmostEqual(first.u[first.assignments[0][0]], 0.5)

    def test_no_edit_and_failed_command_leave_maya_unchanged(self):
        first, second, cmds, om = self.fixtures()
        before = [(mesh.u[:], mesh.v[:], [ids[:] for ids in mesh.assignments])
                  for mesh in (first, second)]
        uvcraft_maya.run({"cmd": "analyze"}, cmds=cmds, om=om, session_factory=FakeSession)
        with self.assertRaises(ValueError):
            uvcraft_maya.run({"cmd": "fail"}, cmds=cmds, om=om, session_factory=FakeSession)
        self.assertEqual(
            [(mesh.u, mesh.v, mesh.assignments) for mesh in (first, second)], before
        )

    def test_apply_failure_restores_all_selected_uv_sets(self):
        first, second, cmds, om = self.fixtures()
        before = [(mesh.u[:], mesh.v[:], [ids[:] for ids in mesh.assignments])
                  for mesh in (first, second)]
        second.fail_assignment = True
        with self.assertRaisesRegex(RuntimeError, "Maya assignment failed"):
            uvcraft_maya.run({"cmd": "shift"}, cmds=cmds, om=om, session_factory=FakeSession)
        self.assertEqual(
            [(mesh.u, mesh.v, mesh.assignments) for mesh in (first, second)], before
        )


if __name__ == "__main__":
    unittest.main()
