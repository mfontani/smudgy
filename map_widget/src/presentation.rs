//! View-local map appearance: named styles applied to rooms and exits.
//!
//! These values belong to a `<MapView />` instance, not to an area. Scripts
//! can therefore highlight a speedwalk, restyle a shared map, or override a
//! live door state without writing collaborative map data.
//!
//! The input model ([`MapViewPresentation`]) carries author-supplied strings.
//! [`MapViewPresentation::resolve`] folds `default_style` + `apply` into
//! per-room / per-exit lookup tables with every color pre-parsed to
//! [`iced::Color`], so drawing performs pure hash lookups — no per-frame
//! string parsing and no linear scans. Malformed colors and unknown style
//! names are reported once (per distinct message) and skipped, never
//! silently nulling their siblings.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

use iced::Color;
use smudgy_cloud::{AreaId, ExitDirection, RoomNumber};

/// Rendered fallback for door glyphs when no style names a `door_color`.
pub const DEFAULT_DOOR_COLOR: Color = Color::from_rgb8(63, 63, 70);

/// When a cross-area destination label is rendered. This is a connection
/// style channel so a route can promote only its selected exits to
/// [`Always`](Self::Always) over a hover-only `defaultStyle`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CrossAreaLabelVisibility {
    /// Preserve the legacy behavior: draw the destination label every frame.
    #[default]
    Always,
    /// Draw the label only while its in-area anchor room is hovered.
    Hover,
    /// Keep the connection marker discoverable but never draw its label.
    Never,
}

impl CrossAreaLabelVisibility {
    /// Resolve the policy for one connection's current hover state.
    #[must_use]
    pub const fn is_visible(self, anchor_hovered: bool) -> bool {
        match self {
            Self::Always => true,
            Self::Hover => anchor_hovered,
            Self::Never => false,
        }
    }
}

/// Log a warning exactly once per distinct message. Presentation inputs come
/// from scripts and mutable stores, so the same bad value is re-resolved on
/// every area switch and presentation change; without the gate one malformed
/// color would warn per resolution.
fn warn_once(message: &str) {
    static WARNED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
    let mut warned = match WARNED.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if warned.insert(message.to_string()) {
        log::warn!("{message}");
    }
}

/// Presentation channels for individual rooms, connections, and doors.
/// Absent fields inherit `default_style`, then the widget default. Colors stay
/// as authored strings here; they are parsed (and validated) at resolution.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MapStyle {
    pub room_fill: Option<String>,
    pub room_stroke: Option<String>,
    pub room_stroke_width: Option<f32>,
    /// Rounded-room corner radius in map units (`0..=0.25`).
    pub room_border_radius: Option<f32>,
    pub connection_color: Option<String>,
    pub connection_width: Option<f32>,
    pub door_color: Option<String>,
    /// Visibility policy for named and redacted cross-area destination
    /// labels. The connection stub and marker remain visible in every mode.
    pub cross_area_label_visibility: Option<CrossAreaLabelVisibility>,
    /// Optional CSS color for a padded background behind cross-area labels.
    pub cross_area_label_background: Option<String>,
}

impl MapStyle {
    /// Clamp untrusted script/store values to finite renderer-safe ranges.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.room_stroke_width = self
            .room_stroke_width
            .map(|value| finite_clamp(value, 0.0, 20.0, 2.0));
        self.room_border_radius = self
            .room_border_radius
            .map(|value| finite_clamp(value, 0.0, 0.25, 0.1));
        self.connection_width = self
            .connection_width
            .map(|value| finite_clamp(value, 0.0, 20.0, 1.0));
        self
    }
}

/// One Connection selected from either endpoint. room+direction (never
/// ConnectionId) disambiguates parallel Connections, reaches the visible
/// half of outbound cross-area links, and survives JSON store bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MapExitRef {
    pub room: RoomNumber,
    pub direction: ExitDirection,
}

/// Associates a named style with rooms and/or exits. Later entries win
/// field-by-field over earlier ones and over `default_style`.
#[derive(Debug, Clone, PartialEq)]
pub struct MapStyleApplication {
    /// Key into [`MapViewPresentation::styles`].
    pub style: String,
    pub rooms: Vec<RoomNumber>,
    pub exits: Vec<MapExitRef>,
    /// Optional scope; entries for other areas are ignored at resolution.
    pub area: Option<AreaId>,
}

/// Semantic door-state override — state, not style.
#[derive(Debug, Clone, PartialEq)]
pub struct MapDoorState {
    pub exit: MapExitRef,
    pub closed: Option<bool>,
    pub locked: Option<bool>,
}

/// The full author-supplied presentation for one map view. Nothing here is
/// persisted or synced.
#[derive(Debug, Clone, PartialEq)]
pub struct MapViewPresentation {
    /// Multiplies room coordinates while keeping room glyphs the same size.
    pub room_spacing: f32,
    pub player_color: Option<String>,
    /// Render persisted or overridden closed/locked door state.
    pub show_doors: bool,
    pub default_style: MapStyle,
    /// The named-style palette `apply` entries reference.
    pub styles: HashMap<String, MapStyle>,
    pub apply: Vec<MapStyleApplication>,
    pub doors: Vec<MapDoorState>,
}

impl Default for MapViewPresentation {
    fn default() -> Self {
        Self {
            room_spacing: 1.0,
            player_color: None,
            show_doors: true,
            default_style: MapStyle::default(),
            styles: HashMap::new(),
            apply: Vec::new(),
            doors: Vec::new(),
        }
    }
}

impl MapViewPresentation {
    /// Clamp untrusted script/store values to finite renderer-safe ranges.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.room_spacing = finite_clamp(self.room_spacing, 0.25, 4.0, 1.0);
        self.default_style = self.default_style.normalized();
        self.styles = self
            .styles
            .into_iter()
            .map(|(name, style)| (name, style.normalized()))
            .collect();
        self
    }

    /// Fold the palette and `apply` entries into per-item lookup tables for
    /// `active_area`, parsing every color once. Precedence: `default_style`
    /// forms the base; `apply` entries merge per-field in array order, later
    /// wins. Entries scoped to another area, entries naming an unknown
    /// style, and unparseable colors are skipped with a warn-once report.
    #[must_use]
    pub fn resolve(&self, active_area: AreaId) -> ResolvedPresentation {
        let palette: HashMap<&str, (ResolvedRoomStyle, ResolvedConnStyle)> = self
            .styles
            .iter()
            .map(|(name, style)| (name.as_str(), resolve_style(style, name)))
            .collect();
        let (base_room, base_conn) = resolve_style(&self.default_style, "defaultStyle");

        let mut rooms: HashMap<RoomNumber, ResolvedRoomStyle> = HashMap::new();
        let mut conns: HashMap<MapExitRef, ResolvedConnStyle> = HashMap::new();
        for entry in &self.apply {
            if entry.area.is_some_and(|area| area != active_area) {
                continue;
            }
            let Some((room_patch, conn_patch)) = palette.get(entry.style.as_str()) else {
                warn_once(&format!(
                    "smudgy MapView: apply entry references unknown style {:?}",
                    entry.style
                ));
                continue;
            };
            for &room in &entry.rooms {
                rooms.entry(room).or_default().merge(room_patch);
            }
            for &exit in &entry.exits {
                conns.entry(exit).or_default().merge(conn_patch);
            }
        }

        let mut doors: HashMap<MapExitRef, DoorOverride> = HashMap::new();
        for door in &self.doors {
            let state = doors.entry(door.exit).or_default();
            if door.closed.is_some() {
                state.closed = door.closed;
            }
            if door.locked.is_some() {
                state.locked = door.locked;
            }
        }

        ResolvedPresentation {
            room_spacing: self.room_spacing,
            player_color: self
                .player_color
                .as_deref()
                .and_then(|color| parse_color_reported(color, "playerColor")),
            show_doors: self.show_doors,
            base_room,
            base_conn,
            rooms,
            conns,
            doors,
        }
    }
}

fn finite_clamp(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        fallback
    }
}

fn parse_color_reported(color: &str, context: &str) -> Option<Color> {
    let parsed = smudgy_cloud::parse_css_color(color);
    if parsed.is_none() {
        warn_once(&format!(
            "smudgy MapView: {context} color {color:?} did not parse; field ignored"
        ));
    }
    parsed
}

/// Split one named style into its parsed room and connection channels.
fn resolve_style(style: &MapStyle, name: &str) -> (ResolvedRoomStyle, ResolvedConnStyle) {
    let color = |field: &Option<String>, channel: &str| {
        field
            .as_deref()
            .and_then(|value| parse_color_reported(value, &format!("style {name:?} {channel}")))
    };
    (
        ResolvedRoomStyle {
            fill: color(&style.room_fill, "roomFill"),
            stroke: color(&style.room_stroke, "roomStroke"),
            stroke_width: style.room_stroke_width,
            border_radius: style.room_border_radius,
        },
        ResolvedConnStyle {
            color: color(&style.connection_color, "connectionColor"),
            width: style.connection_width,
            door_color: color(&style.door_color, "doorColor"),
            cross_area_label_visibility: style.cross_area_label_visibility,
            cross_area_label_background: color(
                &style.cross_area_label_background,
                "crossAreaLabelBackground",
            ),
        },
    )
}

/// A room's paint channels with colors parsed. `None` fields fall through to
/// the renderer defaults (the room's stored color, the faint outline).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ResolvedRoomStyle {
    pub fill: Option<Color>,
    pub stroke: Option<Color>,
    pub stroke_width: Option<f32>,
    pub border_radius: Option<f32>,
}

impl ResolvedRoomStyle {
    /// Per-field overlay: `patch`'s set fields replace this style's.
    pub fn merge(&mut self, patch: &Self) {
        if patch.fill.is_some() {
            self.fill = patch.fill;
        }
        if patch.stroke.is_some() {
            self.stroke = patch.stroke;
        }
        if patch.stroke_width.is_some() {
            self.stroke_width = patch.stroke_width;
        }
        if patch.border_radius.is_some() {
            self.border_radius = patch.border_radius;
        }
    }
}

/// A connection's appearance channels with colors parsed. `None` fields fall
/// through to the Connection's stored color/thickness and legacy label policy.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ResolvedConnStyle {
    pub color: Option<Color>,
    pub width: Option<f32>,
    pub door_color: Option<Color>,
    pub cross_area_label_visibility: Option<CrossAreaLabelVisibility>,
    pub cross_area_label_background: Option<Color>,
}

impl ResolvedConnStyle {
    /// Per-field overlay: `patch`'s set fields replace this style's.
    pub fn merge(&mut self, patch: &Self) {
        if patch.color.is_some() {
            self.color = patch.color;
        }
        if patch.width.is_some() {
            self.width = patch.width;
        }
        if patch.door_color.is_some() {
            self.door_color = patch.door_color;
        }
        if patch.cross_area_label_visibility.is_some() {
            self.cross_area_label_visibility = patch.cross_area_label_visibility;
        }
        if patch.cross_area_label_background.is_some() {
            self.cross_area_label_background = patch.cross_area_label_background;
        }
    }
}

/// Door-state override for one exit ref; `None` fields keep the persisted
/// member state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DoorOverride {
    pub closed: Option<bool>,
    pub locked: Option<bool>,
}

/// The draw-ready form of a presentation: view globals plus per-item lookup
/// tables, colors parsed. Rebuilt in `set_presentation` and on area change
/// (`apply` entries can be area-scoped), never per frame.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedPresentation {
    pub room_spacing: f32,
    pub player_color: Option<Color>,
    pub show_doors: bool,
    pub base_room: ResolvedRoomStyle,
    pub base_conn: ResolvedConnStyle,
    pub rooms: HashMap<RoomNumber, ResolvedRoomStyle>,
    pub conns: HashMap<MapExitRef, ResolvedConnStyle>,
    pub doors: HashMap<MapExitRef, DoorOverride>,
}

impl Default for ResolvedPresentation {
    fn default() -> Self {
        MapViewPresentation::default().resolve(AreaId(smudgy_cloud::Uuid::nil()))
    }
}

impl ResolvedPresentation {
    /// A room's effective paint: the per-room override folded over the
    /// `default_style` base.
    #[must_use]
    pub fn room_paint(&self, room: RoomNumber) -> ResolvedRoomStyle {
        let mut paint = self.base_room;
        if let Some(over) = self.rooms.get(&room) {
            paint.merge(over);
        }
        paint
    }

    /// A connection's effective paint given its candidate exit refs: the
    /// first matching override folded over the `default_style` base.
    #[must_use]
    pub fn conn_paint(&self, anchor: MapExitRef, other: Option<MapExitRef>) -> ResolvedConnStyle {
        let mut paint = self.base_conn;
        if let Some(over) = self
            .conns
            .get(&anchor)
            .or_else(|| other.and_then(|key| self.conns.get(&key)))
        {
            paint.merge(over);
        }
        paint
    }

    /// A connection's door-state override given its candidate exit refs.
    #[must_use]
    pub fn door_override(&self, anchor: MapExitRef, other: Option<MapExitRef>) -> DoorOverride {
        self.doors
            .get(&anchor)
            .or_else(|| other.and_then(|key| self.doors.get(&key)))
            .copied()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smudgy_cloud::Uuid;

    fn area(n: u64) -> AreaId {
        AreaId(Uuid::from_u64_pair(0, n))
    }

    fn exit(room: i32, direction: ExitDirection) -> MapExitRef {
        MapExitRef {
            room: RoomNumber(room),
            direction,
        }
    }

    #[test]
    fn normalization_rejects_non_finite_and_extreme_values() {
        let presentation = MapViewPresentation {
            room_spacing: f32::NAN,
            default_style: MapStyle {
                room_border_radius: Some(8.0),
                connection_width: Some(-2.0),
                room_stroke_width: Some(f32::INFINITY),
                ..MapStyle::default()
            },
            styles: HashMap::from([(
                "hot".to_string(),
                MapStyle {
                    connection_width: Some(500.0),
                    ..MapStyle::default()
                },
            )]),
            ..MapViewPresentation::default()
        }
        .normalized();
        assert_eq!(presentation.room_spacing, 1.0);
        assert_eq!(presentation.default_style.room_border_radius, Some(0.25));
        assert_eq!(presentation.default_style.connection_width, Some(0.0));
        assert_eq!(presentation.default_style.room_stroke_width, Some(2.0));
        assert_eq!(presentation.styles["hot"].connection_width, Some(20.0));
    }

    #[test]
    fn later_apply_entries_win_per_field_over_earlier_and_default() {
        let presentation = MapViewPresentation {
            default_style: MapStyle {
                connection_color: Some("#101010".to_string()),
                connection_width: Some(1.0),
                ..MapStyle::default()
            },
            styles: HashMap::from([
                (
                    "visited".to_string(),
                    MapStyle {
                        room_fill: Some("#111111".to_string()),
                        connection_color: Some("#222222".to_string()),
                        ..MapStyle::default()
                    },
                ),
                (
                    "route".to_string(),
                    MapStyle {
                        room_stroke: Some("#00ff00".to_string()),
                        room_stroke_width: Some(5.0),
                        connection_color: Some("#00ff00".to_string()),
                        ..MapStyle::default()
                    },
                ),
            ]),
            apply: vec![
                MapStyleApplication {
                    style: "visited".to_string(),
                    rooms: vec![RoomNumber(3)],
                    exits: vec![exit(3, ExitDirection::North)],
                    area: None,
                },
                MapStyleApplication {
                    style: "route".to_string(),
                    rooms: vec![RoomNumber(3)],
                    exits: vec![exit(3, ExitDirection::North)],
                    area: None,
                },
            ],
            ..MapViewPresentation::default()
        };
        let resolved = presentation.resolve(area(1));

        // The room layers compose per-field: the earlier fill survives under
        // the later stroke accent.
        let paint = resolved.room_paint(RoomNumber(3));
        assert_eq!(paint.fill, smudgy_cloud::parse_css_color("#111111"));
        assert_eq!(paint.stroke, smudgy_cloud::parse_css_color("#00ff00"));
        assert_eq!(paint.stroke_width, Some(5.0));

        // The later connection color wins; the default width shows through.
        let conn = resolved.conn_paint(exit(3, ExitDirection::North), None);
        assert_eq!(conn.color, smudgy_cloud::parse_css_color("#00ff00"));
        assert_eq!(conn.width, Some(1.0));
    }

    /// Port of `explicit_room_fill_does_not_erase_route_stroke` from the
    /// route-overlay model: an explicit fill on a routed room keeps the
    /// route's stroke accent.
    #[test]
    fn explicit_room_fill_does_not_erase_route_stroke() {
        let presentation = MapViewPresentation {
            styles: HashMap::from([
                (
                    "route".to_string(),
                    MapStyle {
                        room_stroke: Some("#00ff00".to_string()),
                        room_stroke_width: Some(5.0),
                        ..MapStyle::default()
                    },
                ),
                (
                    "explicit".to_string(),
                    MapStyle {
                        room_fill: Some("#111111".to_string()),
                        ..MapStyle::default()
                    },
                ),
            ]),
            apply: vec![
                MapStyleApplication {
                    style: "route".to_string(),
                    rooms: vec![RoomNumber(3)],
                    exits: vec![],
                    area: None,
                },
                MapStyleApplication {
                    style: "explicit".to_string(),
                    rooms: vec![RoomNumber(3)],
                    exits: vec![],
                    area: None,
                },
            ],
            ..MapViewPresentation::default()
        };
        let paint = presentation.resolve(area(1)).room_paint(RoomNumber(3));
        assert_eq!(paint.fill, smudgy_cloud::parse_css_color("#111111"));
        assert_eq!(paint.stroke, smudgy_cloud::parse_css_color("#00ff00"));
        assert_eq!(paint.stroke_width, Some(5.0));
    }

    #[test]
    fn apply_entries_scoped_to_another_area_are_ignored() {
        let presentation = MapViewPresentation {
            styles: HashMap::from([(
                "route".to_string(),
                MapStyle {
                    room_stroke: Some("#00ff00".to_string()),
                    ..MapStyle::default()
                },
            )]),
            apply: vec![
                MapStyleApplication {
                    style: "route".to_string(),
                    rooms: vec![RoomNumber(1)],
                    exits: vec![],
                    area: Some(area(7)),
                },
                MapStyleApplication {
                    style: "route".to_string(),
                    rooms: vec![RoomNumber(2)],
                    exits: vec![],
                    area: None,
                },
            ],
            ..MapViewPresentation::default()
        };

        let elsewhere = presentation.resolve(area(1));
        assert!(!elsewhere.rooms.contains_key(&RoomNumber(1)));
        assert!(elsewhere.rooms.contains_key(&RoomNumber(2)));

        let scoped = presentation.resolve(area(7));
        assert!(scoped.rooms.contains_key(&RoomNumber(1)));
        assert!(scoped.rooms.contains_key(&RoomNumber(2)));
    }

    #[test]
    fn unknown_style_names_and_bad_colors_degrade_per_field() {
        let presentation = MapViewPresentation {
            styles: HashMap::from([(
                "route".to_string(),
                MapStyle {
                    room_stroke: Some("not-a-color".to_string()),
                    room_stroke_width: Some(5.0),
                    ..MapStyle::default()
                },
            )]),
            apply: vec![
                MapStyleApplication {
                    style: "missing".to_string(),
                    rooms: vec![RoomNumber(1)],
                    exits: vec![],
                    area: None,
                },
                MapStyleApplication {
                    style: "route".to_string(),
                    rooms: vec![RoomNumber(2)],
                    exits: vec![],
                    area: None,
                },
            ],
            ..MapViewPresentation::default()
        };
        let resolved = presentation.resolve(area(1));

        // The unknown-style entry vanishes without affecting others.
        assert!(!resolved.rooms.contains_key(&RoomNumber(1)));
        // The bad color drops its own field only; the width still applies.
        let paint = resolved.room_paint(RoomNumber(2));
        assert_eq!(paint.stroke, None);
        assert_eq!(paint.stroke_width, Some(5.0));
    }

    #[test]
    fn route_style_promotes_one_cross_area_label_over_hover_only_default() {
        let routed = exit(3, ExitDirection::North);
        let presentation = MapViewPresentation {
            default_style: MapStyle {
                cross_area_label_visibility: Some(CrossAreaLabelVisibility::Hover),
                cross_area_label_background: Some("rgba(7, 7, 6, 0.88)".to_string()),
                ..MapStyle::default()
            },
            styles: HashMap::from([(
                "route".to_string(),
                MapStyle {
                    cross_area_label_visibility: Some(CrossAreaLabelVisibility::Always),
                    ..MapStyle::default()
                },
            )]),
            apply: vec![MapStyleApplication {
                style: "route".to_string(),
                rooms: vec![RoomNumber(3)],
                exits: vec![routed],
                area: None,
            }],
            ..MapViewPresentation::default()
        };
        let resolved = presentation.resolve(area(1));

        let routed_paint = resolved.conn_paint(routed, None);
        assert_eq!(
            routed_paint.cross_area_label_visibility,
            Some(CrossAreaLabelVisibility::Always)
        );
        assert_eq!(
            routed_paint.cross_area_label_background,
            smudgy_cloud::parse_css_color("rgba(7, 7, 6, 0.88)")
        );

        let ordinary = resolved.conn_paint(exit(4, ExitDirection::East), None);
        assert_eq!(
            ordinary.cross_area_label_visibility,
            Some(CrossAreaLabelVisibility::Hover)
        );
        assert_eq!(
            ordinary.cross_area_label_background,
            smudgy_cloud::parse_css_color("rgba(7, 7, 6, 0.88)")
        );
    }

    #[test]
    fn door_overrides_merge_later_wins_per_field() {
        let key = exit(4, ExitDirection::East);
        let presentation = MapViewPresentation {
            doors: vec![
                MapDoorState {
                    exit: key,
                    closed: Some(true),
                    locked: Some(true),
                },
                MapDoorState {
                    exit: key,
                    closed: Some(false),
                    locked: None,
                },
            ],
            ..MapViewPresentation::default()
        };
        let resolved = presentation.resolve(area(1));
        let state = resolved.door_override(key, None);
        assert_eq!(state.closed, Some(false));
        assert_eq!(state.locked, Some(true));
    }

    #[test]
    fn conn_paint_prefers_the_anchor_key_and_falls_back_to_the_other_end() {
        let anchor = exit(1, ExitDirection::North);
        let far = exit(2, ExitDirection::South);
        let presentation = MapViewPresentation {
            styles: HashMap::from([(
                "route".to_string(),
                MapStyle {
                    connection_color: Some("#00ff00".to_string()),
                    ..MapStyle::default()
                },
            )]),
            apply: vec![MapStyleApplication {
                style: "route".to_string(),
                rooms: vec![],
                exits: vec![far],
                area: None,
            }],
            ..MapViewPresentation::default()
        };
        let resolved = presentation.resolve(area(1));
        // A ref on the far endpoint still selects the connection when the
        // draw pass offers it as the second candidate key.
        let paint = resolved.conn_paint(anchor, Some(far));
        assert_eq!(paint.color, smudgy_cloud::parse_css_color("#00ff00"));
        // Without the far candidate (outbound/dangling ends), no match.
        assert_eq!(resolved.conn_paint(anchor, None).color, None);
    }
}
