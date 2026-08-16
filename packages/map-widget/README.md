# Map Widget

`smudgy://official/map-widget` adds a simple map pane to a Smudgy session. Choose which side of the session it opens on, set its starting size, and decide whether to show the current area and room information.

The widget displays the maps available to the connected MUD. Cross-area destinations appear when you hover over their exits, with labels that use the active app background color. Labels on exits from your current room remain visible so area transitions are readable before you move. A mapper package or script is still responsible for identifying your current room and following you as you move.

Area and room information appears in a separate, scrollable **Notes** pane above the **Map** pane. Drag the divider between them to give either pane more room.

## Settings

| Setting | What it changes |
| --- | --- |
| Map pane position | Opens the pane above, left, below, or right of the main terminal. |
| Map area size | Sets the overall width on the left or right, or height on the top or bottom. You can also resize it by dragging the outer divider. |
| Show area name and notes | Shows or hides the current area information above the map. |
| Show room name and notes | Shows or hides the current room information above the map. |
| Allow editing area notes | Makes the area name a link that opens its notes in a text editor. |
| Allow editing room notes | Makes the room name a link that opens its notes in a text editor. |

Area and room names stay read-only. When note editing is enabled, select a name above the map to edit that area or room's notes. Notes support Markdown.

## Make it yours

This package is intentionally small. Choose **Edit a copy** in Smudgy, then fine-tune the copy to your liking.
