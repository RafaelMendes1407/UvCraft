# UvCraft user guide

This guide describes the current desktop interface. Control names are written as they appear in the application. Sections 1–5 cover the usual Blender round trip; the remaining sections explain quality checks, shortcuts, and troubleshooting.

## 1. Start UvCraft and set up Blender

### Start the desktop app

From the repository root, run `cargo run -p uvcraft-app`, or open the built executable at `target/debug/uvcraft-app`. To open an OBJ, glTF, or GLB mesh at startup, run `cargo run -p uvcraft-app -- path/to/mesh.obj`. You can also drag a file onto the window or use **File → Open…**.

UvCraft has a 3D viewport and a 2D UV editor. The right panel contains **Unwrap**, **Pack**, **Relax**, **Seams**, **Island**, **Transform**, and **Visualization**. **Help → Controls and workflow…** provides an in-app summary.

### Install the Blender add-on

1. Open UvCraft and choose **File → Install Blender add-on…**.
2. Choose the detected Blender profile. If Blender uses a custom scripts location, choose **Choose add-ons folder…** and select that profile's `addons` directory.
3. Restart Blender, open its add-on preferences, and enable **UvCraft UV Bridge**.
4. In Blender's 3D View, press `N` to open the sidebar and choose the **UvCraft** tab.

The local connection does not require UvCraft's optional Python extension. See the [Blender add-on README](../bridges/blender/README.md) for the add-on's other workflows.

### Connect the two applications

1. Keep UvCraft open. In **File**, check the **Blender bridge** URL, usually `http://127.0.0.1:8766`.
2. Click **Copy bridge token**.
3. In Blender's **UvCraft** tab, enter the same **Bridge URL**, paste the token into **Session token**, and click **Connect**.
4. Check that the panel says **Connected**. This verifies access; it does not transfer a mesh yet.

The token changes when the UvCraft bridge restarts. Copy the new token if you reopen the app. The bridge is bound to the local computer.

## 2. Send a Blender mesh to UvCraft

1. Select one or more **Mesh** objects in Blender.
2. In the **UvCraft** tab, under **Mesh Transfer**, click **Send Selected**.
3. Return to UvCraft. The received mesh appears automatically, and the status bar shows **Blender object: object-name**.
4. When several objects were sent, UvCraft displays the most recently sent one. To work on another, select it in Blender and use **Update Mesh** or send it again.

Enable **Preserve Existing UVs** to send the mesh with its current UV coordinates. Linked objects that share a mesh datablock are processed once.

If you change the mesh in Blender while editing, **Update Mesh** sends Blender's current mesh and UVs to the existing session. It replaces UvCraft's current copy, so receive pending UvCraft edits first if you want to keep them.

## 3. Cut seams and unwrap

1. Choose **Edges** in the 3D or UV viewport header. Each viewport keeps its own selection mode.
2. Click the edges you want to cut. `Shift` adds to the selection. In Edges mode, `Shift` while hovering previews a connected shortest path; `Shift`-click selects it. Double-click selects an edge loop.
3. In the right-hand **Seams** panel, click **Mark Seam**. The context menu also offers **Mark Seam / Cut**, and the default shortcut is `C`. The cut line is highlighted in the UV editor, even while the edge remains selected.
4. To remove a cut, select the edge and use **Clear Seams**. In the **Island** panel, **Separate** splits UVs along selected edges; **Stitch** joins sides that already coincide; **Merge** aligns and welds seam sides.
5. For automatic cuts, set **Seam angle**, enable **Hard edges as seams**, and click **Unwrap**. The top **Unwrap → Auto seams** command is another option.
6. To unfold the UVs, click **Unwrap** in the right panel or **Unwrap → Unfold** in the top menu. Choose **Angle Based (ABF)** or **Conformal (LSCM)**. If you already marked seams manually, leave **Hard edges as seams** off to keep that seam plan.
7. Inspect the result in the UV editor. **Relax** can improve the layout with **SLIM** or **ARAP**.

UV cuts do not split the 3D mesh. Its vertices and faces retain their connectivity.

## 4. Edit, group, stack, and pack islands

### Select and transform

Choose **Islands** in the UV editor header. Click an island to select it, and drag it to move it. Enable the UV gizmo in **Transform**, then drag its colored handles to move, rotate, or scale the selection. The same panel offers 90° rotation buttons, 90% and 110% scale buttons, numeric X/Y position and scale, a rotation slider accepting negative angles, and **Apply transform**. A selected group scales uniformly. Use **Edit → Undo** or **Redo** to reverse session edits.

To straighten a curved quad strip, select one island and use **Island → Straighten**. For an open UV border path, select connected UV edges and use **Align path**.

### Create and manage groups

1. Select at least two islands in the UV editor.
2. Right-click and choose **Create group**, or click **Create Group** in the **Group Workspace** below the UV editor.
3. Select the group in the **Group Workspace**. **Select Group** moves, rotates, scales, and packs its members together; **Individual** returns to editing one member.
4. To rename it, select the group and use **Group name → Rename** in the right panel.
5. To change membership, use **Add to group** or **Remove from group** in the context menu, or use **Group Workspace → Manage**. **Manage** also provides **Ungroup** and **Delete group**.

The UV editor draws a colored container around a group. A group organizes islands; it does not merge 3D geometry and is separate from an intentional UV stack. See [Group Workspace](group-workspace.md) for selection and persistence details.

### Stack matching islands

Select at least two islands. In **Stack and island layout** on the right, choose **Stack reference**. Use **Preview stack** to inspect the match, then **Stack matching** to align the islands. Optional **Conform to reference** adjusts a compatible boundary when the internal UV layouts differ. **Find matching** only searches for candidates; **Unstack** separates registered stacks. Read the preview diagnosis before conforming because not every boundary is compatible.

### Set scale and pack

In **Pack**, set **Resolution**, **Padding**, **Allow 90° rotation**, **Fill available space**, **Keep island groups**, and **Preserve stacks** as needed. Click **Pack**. While packing is running, **Stop packing** interrupts it. A preserved group is packed as one unit; registered stacks remain together when preservation is enabled.

In **Stack and island layout**, **Average island scale** recalculates the selected islands' relative scale. With a selected group, choose **Confirm group recalculation** in its menu to apply the calculation to that group. For an exact texel density, use **Set TD** and enter the pixels-per-unit target in the density controls.

## 5. Return UVs to Blender

1. Finish the edit in UvCraft. You can use **File → Save** or **Save As…** to keep a separate file, but saving is not required for the local transfer.
2. In Blender, keep the same object selected.
3. In the **UvCraft** tab, click **Receive UVs**.
4. Check the UV islands in Blender's UV Editor and the seams in mesh Edit Mode.
5. Save the `.blend` file to keep the result in your Blender project.

**Receive UVs** transfers UV coordinates and seams. It does not modify 3D vertices or faces. **Refresh Status** checks session compatibility and topology. **Disconnect** ends the sessions. After disconnecting or restarting either app, connect and send the mesh again. The panel says **Auto Sync: unavailable** because each transfer must be triggered manually.

If you use **Update Mesh** after editing in UvCraft, Blender's state can replace the session's UVs. Receive UvCraft's result before updating the mesh.

## 6. Visualize UV quality

The UV editor's **Display** menu switches between **Checker**, **Texture**, **Islands**, **UV Distortion**, **Flipped UVs**, **Overlaps**, and **Texel Density**. **Grid** toggles the grid. In the right-hand **Visualization** panel, toggle seams and island borders, adjust the checker, or load a PNG to enable **Texture**. Edges remain visible over analysis colors.

Under **UV Distortion**, choose **Area**, **Conformal**, **Directional**, or **Combined**. Set a reference based on the whole object, selected islands/group, individual island, texel-density target, or raw scale. **Sensitivity** changes the legend's value range; **Heatmap opacity** adjusts transparency; **Show in 3D** displays the same analysis on the model; **Isolate active island** helps inspect overlaps. In this editor's convention, blue means less UV area per 3D surface area (the texture looks enlarged), red means more UV area, and magenta warns of inverted or degenerate triangles. The panel legend gives numeric thresholds for the selected metric.

**Flipped UVs** and **Overlaps** provide specific checks. **Texel Density** compares against a pixels-per-unit target. Revisit these displays after unwrapping, relaxing, transforming, and packing.

## 7. Navigation, shortcuts, and saved files

| Action | Default control |
| --- | --- |
| Select / move UV islands | Left mouse button |
| Add to selection | `Shift` + left mouse button |
| Orbit 3D view | Drag right mouse button |
| Pan 3D/UV view | Drag middle mouse button |
| Zoom | Mouse wheel |
| Frame all | `F` |
| Cut selected edges | `C` |
| Weld selected seam | `W` |
| Unfold | `U` |
| Pack | `P` |
| Open / Save | `Ctrl+O` / `Ctrl+S` |
| Undo / Redo | `Ctrl+Z` / `Ctrl+Shift+Z` |

You can customize shortcuts and mouse navigation in **Tools → Session defaults…**. Viewport headers also provide selection, visibility, and framing controls. The context menu shows actions relevant to the current selection.

When saving a mesh in UvCraft, keep the generated `<model>.uvcraft.json` sidecar next to it. That file stores UvCraft-specific metadata such as groups and stack relations. Common mesh formats do not carry this metadata. Blender receives UV coordinates and seams; group and stack records stay in UvCraft.

## 8. Troubleshooting

| Symptom | Check |
| --- | --- |
| **Connect** works, but nothing appears in UvCraft | Select a Mesh object in Blender and click **Send Selected**. |
| **Disconnected** or authentication error | Keep UvCraft open, verify the URL, and copy the current token from **File → Copy bridge token**. |
| **Receive UVs** reports incompatible topology | Check that the same object is selected and its vertex/face order has not changed in Blender. Receiving is cancelled to protect the mesh. Send it again after topology edits. |
| Blender shows older UVs | Click **Receive UVs** again; synchronization is manual. Check that the same object is selected. |
| **Texture** is unavailable | Use **Visualization → Load texture PNG…** before choosing **Display → Texture**. |
| Groups or stacks disappear after reopening a mesh | Reopen it with its `.uvcraft.json` sidecar. Those records are not sent to Blender. |
| Physical scale looks wrong | The bridge uses local mesh coordinates and does not evaluate modifiers. Apply object transforms in Blender before physical-scale operations. |
