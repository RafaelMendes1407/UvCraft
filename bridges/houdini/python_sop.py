"""Example Python SOP body. Copy into a Houdini Python SOP's Code field."""

import hou
import uvcraft_houdini

node = hou.pwd()
geometry = node.geometry()
commands = '[{"cmd":"auto_seams"},{"cmd":"unfold"},{"cmd":"pack"}]'

# Optional spare string parameters: uvcraft_commands (JSON) and uvcraft_group
# (primitive group pattern). Avoid writing to node parameters during a cook.
if node.parm("uvcraft_commands") is not None:
    commands = node.evalParm("uvcraft_commands")
group = node.evalParm("uvcraft_group") if node.parm("uvcraft_group") is not None else ""
selected = [prim.number() for prim in geometry.globPrims(group)] if group else None

uvcraft_houdini.run(geometry, commands, selected_primitives=selected)
