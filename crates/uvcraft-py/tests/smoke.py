"""Smoke test for the `uvcraft` Python module.

Run after `maturin develop` (from crates/uvcraft-py), or with the built
extension on PYTHONPATH:  python3 crates/uvcraft-py/tests/smoke.py
"""

import json
import os
import tempfile

import uvcraft

QUADS = """v 0 0 0
v 1 0 0
v 1 1 0
v 0 1 0
v 2 0 0
v 2 1 0
vt 0 0
vt 0.5 0
vt 0.5 1
vt 0 1
vt 0.5 0
vt 1 0
vt 1 1
vt 0.5 1
f 1/1 2/2 3/3 4/4
f 2/5 5/6 6/7 3/8
"""


def main():
    with tempfile.TemporaryDirectory() as tmp:
        mesh = os.path.join(tmp, "in.obj")
        with open(mesh, "w") as f:
            f.write(QUADS)

        s = uvcraft.Session()
        assert not s.has_document
        try:
            s.undo()
        except uvcraft.UvCraftError as e:
            assert "undo" in str(e)
        else:
            raise AssertionError("undo without a mesh must fail")
        try:
            s.execute({"cmd": "bogus"})
        except ValueError:
            pass
        else:
            raise AssertionError("unknown commands must raise ValueError")

        assert s.load(mesh) == {"cmd": "load", "status": "done"}
        assert (s.face_count, s.vertex_count, s.corner_count, s.uv_count) == (2, 6, 8, 8)
        assert s.islands() == [[0], [1]]
        assert len(s.corner_uv()) == 8 and len(s.uvs()) == 8

        before = s.uvs()
        s.transform([1], translate=[0, 1])
        assert s.uvs()[5] == (1.0, 1.0)
        s.undo()
        assert s.uvs() == before
        s.redo()

        s.execute('{"cmd":"project","mode":{"planar":"z"}}')
        s.project("box", islands=[0])
        s.auto_seams(angle_deg=60)
        s.unfold()
        s.refine(iterations=3)
        s.optimize(method="arap", iterations=5)
        s.optimize_angle_area(0.5, iterations=3)
        s.pack(resolution=256, padding_px=2, method="shelf", groups=[[0, 1]])
        s.equalize_texel_density(resolution=1024)
        s.set_texel_density(100, resolution=1024)
        s.pin([0, 1])
        s.clear_pins()
        selection = s.select_islands("flipped")
        assert selection["status"] == "selection"
        assert isinstance(selection["islands"], list)
        density_selection = s.select_islands_by_metric(
            "texel_density", min=0, resolution=1024
        )
        assert density_selection["islands"] == [0, 1]
        physical = s.measure_physical("meter", "centimeter", islands=[0])
        assert abs(physical["report"]["island_areas"][0]["area"] - 10000) < 1e-6
        report = s.analyze(resolution=1024)
        assert report["island_count"] == 2, report
        assert all(abs(i["texel_density"] - 100) < 1e-6 for i in report["islands"])

        log = s.log()
        assert log[0] == {"cmd": "load", "path": mesh}
        assert [c["cmd"] for c in log][-1] == "analyze"
        assert json.loads(s.log_jsonl().splitlines()[1])["cmd"] == "transform"

        macro_path = os.path.join(tmp, "m.jsonl")
        s.save_macro(macro_path)
        assert "analyze" not in [c["cmd"] for c in s.macro_commands()]

        other = uvcraft.Session()
        other.load(mesh)
        results = other.run_file(macro_path)
        assert len(results) == len(s.macro_commands())
        assert other.uvs() == s.uvs()

        out = os.path.join(tmp, "out.obj")
        s.save(out)
        assert os.path.exists(out)
        print("uvcraft", uvcraft.__version__, "smoke test OK:", repr(s))


if __name__ == "__main__":
    main()
