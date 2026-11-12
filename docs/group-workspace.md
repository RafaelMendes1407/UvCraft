# UV Island Groups and Group Workspace

## Create and edit a group

1. In the UV viewport, select at least two islands.
2. Choose **Create group** in the context menu or **Create Group** in the Group Workspace.
3. Select the group in the Group Workspace and use **Group name → Rename** in the right panel to name it. The group row also offers color, visibility and lock controls. The Group Workspace **Manage** menu adds or removes selected islands and offers **Ungroup** and **Delete group**. These actions do not delete UV geometry.

Each island belongs to at most one ordinary group. Groups organize islands; they do not weld geometry or create UV stacks. An island can still be edited alone while it belongs to a group.

## Navigate the lower strip

The **Group Workspace** sits below the 2D UV viewport. Expand it with the arrow at its left edge, then choose a group from the name menu. Drag the divider above the strip to resize it; use horizontal scrolling for long groups. The strip draws each thumbnail from the island's current UV edges.

Choose **Select Group** to enter group selection. Its members share one bounding box and one transform pivot. Choose **Individual**, click a thumbnail, or select a member in the UV viewport to edit one island. The active thumbnail follows the UV selection. Right click a thumbnail for **Remove from group**.

## Transform, scale, and pack

The UV gizmo moves, rotates, and uniformly scales a selected group as one unit. The right-side **Transform** panel offers numeric U/V translation, rotation in degrees (including negative values), uniform scale, and **Apply transform**. Each action creates one Undo/Redo entry. Individual island selection keeps the normal UV editing tools.

**Pack** treats a group as an indivisible packing unit and preserves its internal layout. Packing may translate, rotate, or uniformly scale the whole group according to the pack settings. Intentional UV stacks remain a separate relation and are preserved when stack preservation is enabled.

In group selection, **Average Island Scale** uses the aggregate UV and 3D areas and applies one factor about the group pivot. It compares the group's aggregate density with the other islands in the document. If there are no usable reference islands, the operation reports that and makes no change. To enter an exact aggregate target, use **Set Island Group Texel Density** in the command API. Because stacked UV surfaces each contribute area, members of a stack count toward surface density even when their occupied UV regions overlap.

Relax and Straighten are individual island operations. Select a member first; they do not silently change every member of a selected group.

## Save, Blender, and limits

Saving a model writes UvCraft metadata beside it as `<model>.uvcraft.json` (version 2). Keep the model and sidecar together to reopen groups and stacks. The OBJ, glTF/GLB, and FBX formats do not carry these UvCraft groups themselves. An older version 1 stack sidecar remains readable. Undo/Redo works for group membership, properties, transforms, scaling, and packing within the current session; reopen starts a new history.

Blender synchronization exchanges the real UV coordinates and seams. Group metadata stays in UvCraft. A compatible mesh update keeps it; if unwrap splits or joins an island without a reliable whole-island match, the affected group membership is removed rather than assigned to an unrelated island. Group IDs are internal persistent identifiers and should not be interpreted as current island indices.

No dedicated keyboard shortcut is assigned to group commands. Use the context menu, Group Workspace and **Island groups** panel, or invoke the serialized command API.
