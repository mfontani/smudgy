import {
    createState,
    getSettings,
    mapper,
    session,
    type Pane,
} from "smudgy:core";
import { room as roomChanged } from "smudgy:events/map";
import { get } from "smudgy:params";
import {
    Button,
    Column,
    Container,
    createWidget,
    MapView,
    Markdown,
    Modal,
    removeWidget,
    Row,
    Scrollable,
    Text,
    TextEditor,
    type MapStyleApplication,
} from "smudgy:widgets";

type Position = "top" | "left" | "bottom" | "right";

const PANE_NAME = "Map";
const NOTES_PANE_NAME = "Notes";
const MAP_WIDGET = "00-map";
const INFO_WIDGET = "01-map-info";
const EDITOR_WIDGET = "02-notes-editor";
const LAYOUT_STORAGE_KEY = "map-widget-layout";
const MIN_SIZE = 160;
const CURRENT_ROOM_STYLE = "current-room";
const currentRoomMapApply = createState<MapStyleApplication[]>("currentRoomMapApply");

function refreshMapStyles() {
    try {
        const location = mapper.getCurrentLocation();
        if (!location || location.room === undefined) {
            currentRoomMapApply.set([]);
            return;
        }
        const room = mapper.getAreaById(location.area).room(location.room);
        if (!room || room.exits.length === 0) {
            currentRoomMapApply.set([]);
            return;
        }
        currentRoomMapApply.set([{
            style: CURRENT_ROOM_STYLE,
            exits: room.exits.map((exit) => ({
                room: room.room_number,
                direction: exit.from_direction,
            })),
        }]);
    } catch {
        currentRoomMapApply.set([]);
    }
}

function positionParam(): Position {
    const value = get("position");
    if (value === "top" || value === "left" || value === "bottom" || value === "right") {
        return value;
    }
    return "right";
}

function boolParam(key: string, fallback: boolean): boolean {
    const value = get(key);
    return typeof value === "boolean" ? value : fallback;
}

function sizeParam(): number {
    const value = get("size");
    return typeof value === "number" && Number.isFinite(value)
        ? Math.max(MIN_SIZE, Math.round(value))
        : 350;
}

const position = positionParam();
const paneSize = sizeParam();
const showAreaName = boolParam("showAreaName", true);
const showRoomName = boolParam("showRoomName", true);
const editAreaNotes = boolParam("editAreaNotes", true);
const editRoomNotes = boolParam("editRoomNotes", true);

function splitMapPane(): Pane {
    switch (position) {
        case "top":
            return session.mainPane.split("top", {
                name: PANE_NAME,
                height: paneSize,
                terminal: false,
            });
        case "left":
            return session.mainPane.split("left", {
                name: PANE_NAME,
                width: paneSize,
                terminal: false,
            });
        case "bottom":
            return session.mainPane.split("bottom", {
                name: PANE_NAME,
                height: paneSize,
                terminal: false,
            });
        case "right":
            return session.mainPane.split("right", {
                name: PANE_NAME,
                width: paneSize,
                terminal: false,
            });
    }
}

function relocateMapPane(pane: Pane) {
    switch (position) {
        case "top":
            pane.relocate("top", session.mainPane, { height: paneSize });
            break;
        case "left":
            pane.relocate("left", session.mainPane, { width: paneSize });
            break;
        case "bottom":
            pane.relocate("bottom", session.mainPane, { height: paneSize });
            break;
        case "right":
            pane.relocate("right", session.mainPane, { width: paneSize });
            break;
    }
}

const mapPane = splitMapPane();
const layout = `${position}:${paneSize}`;
const savedLayout = localStorage.getItem(LAYOUT_STORAGE_KEY);
const layoutChanged = savedLayout !== layout;

// Keep a player's divider adjustments across ordinary reloads. When the package
// settings change, apply the new position and size to the existing pane once.
if (!mapPane.created && layoutChanged) {
    relocateMapPane(mapPane);
}
localStorage.setItem(LAYOUT_STORAGE_KEY, layout);

const notesInitialHeight = position === "top" || position === "bottom"
    ? Math.max(80, Math.min(180, Math.round(paneSize * 0.35)))
    : 180;
const notesPane = mapPane.split("top", {
    name: NOTES_PANE_NAME,
    height: notesInitialHeight,
    terminal: false,
});
if (!notesPane.created && (layoutChanged || mapPane.created)) {
    notesPane.relocate("top", mapPane, { height: notesInitialHeight });
}

interface EditTarget {
    title: string;
    value: string;
    save(value: string): Promise<unknown>;
}

function currentNotesTarget(kind: "area" | "room"): EditTarget | undefined {
    const location = mapper.getCurrentLocation();
    if (!location) {
        return undefined;
    }

    const area = mapper.getAreaById(location.area);
    if (kind === "area") {
        return {
            title: `${area.name || "Area"} notes`,
            value: area.data("notes") ?? "",
            save: (value) => mapper.setAreaProperty(area, "notes", value),
        };
    }

    const roomNumber = location.room;
    if (roomNumber === undefined) {
        return undefined;
    }
    const currentRoom = area.room(roomNumber);
    if (!currentRoom) {
        return undefined;
    }
    return {
        title: `${currentRoom.title || "Room"} notes`,
        value: currentRoom.data("notes") ?? "",
        save: (value) => mapper.setRoomProperty(area, roomNumber, "notes", value),
    };
}

let editorSequence = 0;

function openNotesEditor(kind: "area" | "room") {
    const target = currentNotesTarget(kind);
    if (!target) {
        return;
    }

    let draft = target.value;
    const editorId = `${EDITOR_WIDGET}-${++editorSequence}`;

    const close = () => removeWidget(EDITOR_WIDGET);
    const render = (error?: string) => {
        const commit = async () => {
            try {
                await target.save(draft);
                close();
                renderInfoWidget();
            } catch (caught) {
                const message = caught instanceof Error ? caught.message : String(caught);
                render(`Could not save notes: ${message}`);
            }
        };

        createWidget(
            EDITOR_WIDGET,
            <Modal onDismiss={close}>
                <Container width="fill" height="fill" background="#1e1e1e">
                    <Column width="fill" height="fill" spacing={8} padding={16}>
                        <Text size={18}>{target.title}</Text>
                        <TextEditor
                            id={editorId}
                            value={target.value}
                            height="fill"
                            placeholder="Notes (Markdown supported)"
                            onChange={(value) => {
                                draft = value;
                            }}
                        />
                        {error ? <Text color="#ff8a80">{error}</Text> : null}
                        <Row spacing={8}>
                            <Button
                                variant="primary"
                                onPress={() => {
                                    void commit();
                                }}
                            >
                                Save
                            </Button>
                            <Button onPress={close}>Cancel</Button>
                        </Row>
                    </Column>
                </Container>
            </Modal>,
            { pane: notesPane },
        );
    };

    render();
}

function infoBlock(
    name: string,
    notes: string,
    size: number,
    color: string,
    onEdit?: () => void,
) {
    const label = (
        <Container width="fill" align_x="right">
            <Text size={size} color={color}>{name}</Text>
        </Container>
    );

    const heading = onEdit
        ? <Button width="fill" variant="link" onPress={onEdit}>{label}</Button>
        : label;
    return (
        <Column width="fill" spacing={2}>
            {heading}
            <Markdown size={14}>{notes}</Markdown>
        </Column>
    );
}

function renderInfoWidget() {
    try {
        const location = mapper.getCurrentLocation();
        if (!location || (!showAreaName && !showRoomName)) {
            removeWidget(INFO_WIDGET);
            return;
        }

        const area = mapper.getAreaById(location.area);
        const currentRoom = location.room === undefined ? undefined : area.room(location.room);
        const areaNotes = area.data("notes") ?? "";
        const roomNotes = currentRoom?.data("notes") ?? "";
        const areaInfo = showAreaName
            ? infoBlock(
                area.name,
                areaNotes,
                17,
                "white",
                editAreaNotes ? () => openNotesEditor("area") : undefined,
            )
            : null;
        const roomInfo = showRoomName && currentRoom
            ? infoBlock(
                currentRoom.title,
                roomNotes,
                14,
                "#a8a8a8",
                editRoomNotes ? () => openNotesEditor("room") : undefined,
            )
            : null;
        createWidget(
            INFO_WIDGET,
            <Scrollable width="fill" height="fill" direction="vertical">
                <Column width="fill" spacing={2} padding={8}>
                    {areaInfo}
                    {roomInfo}
                </Column>
            </Scrollable>,
            { pane: notesPane },
        );
    } catch {
        removeWidget(INFO_WIDGET);
    }
}

// Keep the MapView mounted while the separate Notes pane is refreshed. Re-mounting
// the map itself on every room change causes a visible flash.
createWidget(
    MAP_WIDGET,
    <Container width="fill" height="fill">
        <MapView
            defaultStyle={{
                crossAreaLabelVisibility: "hover",
                crossAreaLabelBackground: getSettings().palette?.background,
            }}
            styles={{
                [CURRENT_ROOM_STYLE]: {
                    crossAreaLabelVisibility: "always",
                },
            }}
            apply={currentRoomMapApply.bind()}
        />
    </Container>,
    { pane: mapPane },
);

refreshMapStyles();
renderInfoWidget();
roomChanged.on(() => {
    refreshMapStyles();
    renderInfoWidget();
});
