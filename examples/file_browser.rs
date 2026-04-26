use bevy::{
    app::{AppExit, PluginGroup},
    feathers::{
        FeathersPlugins,
        constants::{fonts, size},
        containers::{
            flex_spacer, pane, pane_body, pane_header, pane_header_divider, subpane, subpane_body,
            subpane_header,
        },
        controls::{
            ButtonProps, ButtonVariant, MenuButtonProps, MenuItemProps, menu, menu_button,
            menu_divider, menu_item, menu_popup, tool_button,
        },
        cursor::EntityCursor,
        dark_theme::create_dark_theme,
        focus::FocusIndicator,
        font_styles::InheritableFont,
        theme::{InheritableThemeTextColor, ThemeBackgroundColor, ThemedText, UiTheme},
        tokens,
    },
    input::mouse::MouseScrollUnit,
    input_focus::{
        AutoFocus,
        tab_navigation::{TabGroup, TabIndex},
    },
    picking::hover::Hovered,
    prelude::*,
    scene::{bsn_list, prelude::Scene},
    text::FontWeight,
    ui_widgets::{Activate, Button, ControlOrientation, Scrollbar, ScrollbarThumb},
    window::SystemCursorIcon,
};
use bevy_malek_async::AsyncPlugin;
use bevy_malek_async::async_ui::{AsyncUi, AsyncUiPlugin, Ctx};
use bevy_malek_async_macros::bsn_ui;
use futures::{FutureExt, select};
use std::{
    cmp::Reverse,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, SystemTime},
};

const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(450);
fn main() {
    file_browser_app(None).run();
}

fn file_browser_app(auto_close_after: Option<Duration>) -> App {
    file_browser_app_with_plugins(DefaultPlugins, auto_close_after)
}

fn file_browser_app_with_plugins(
    default_plugins: impl PluginGroup,
    auto_close_after: Option<Duration>,
) -> App {
    let mut app = App::new();
    app.add_plugins(default_plugins);
    finish_file_browser_app(&mut app, auto_close_after);
    app
}

fn finish_file_browser_app(app: &mut App, auto_close_after: Option<Duration>) {
    app.add_plugins((FeathersPlugins, AsyncPlugin::default(), AsyncUiPlugin))
        .insert_resource(UiTheme(create_dark_theme()))
        .add_systems(Startup, setup);

    if let Some(duration) = auto_close_after {
        app.insert_resource(AutoCloseTimer(Timer::new(duration, TimerMode::Once)))
            .add_systems(Update, auto_close);
    }
}

fn setup(world: &mut World) -> Result {
    world.spawn(Camera2d);
    world.spawn_scene(file_browser_root())?;
    Ok(())
}

#[derive(Resource)]
struct AutoCloseTimer(Timer);

fn auto_close(
    mut timer: ResMut<AutoCloseTimer>,
    time: Res<Time>,
    mut app_exit: MessageWriter<AppExit>,
) {
    if timer.0.tick(time.delta()).just_finished() {
        app_exit.write(AppExit::Success);
    }
}

#[derive(Component, FromTemplate)]
struct BrowserRoot;

#[derive(Component, FromTemplate)]
struct FileListRoot;

#[derive(Component, FromTemplate)]
struct PathText;

#[derive(Component, FromTemplate)]
struct StatusText;

#[derive(Component, Clone, Default, FromTemplate)]
struct FileEntry {
    path: PathBuf,
    is_dir: bool,
}

#[derive(Component, Clone, Default, FromTemplate)]
struct CurrentDir(PathBuf);

#[derive(Component, Clone, Copy, Default, FromTemplate)]
struct SortOrder {
    newest_first: bool,
}

#[derive(Component, Clone, Copy, Default)]
struct LastClick(Option<SystemTime>);

#[derive(Component)]
struct LoadedEntries(Vec<EntryRow>);

#[derive(EntityEvent)]
struct RefreshDirectory(Entity);

fn file_browser_root() -> impl Scene {
    bsn_ui! {
        Node {
            width: percent(100),
            height: percent(100),
            padding: UiRect::all(px(12)),
        }
        #This
        TabGroup
        ThemeBackgroundColor(tokens::WINDOW_BG)
        BrowserRoot
        CurrentDir({std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))})
        SortOrder { newest_first: true }
        async |ui: Ctx| {
            ui.cached_state::<Commands>()
                .bridge(AsyncUi, |mut commands| {
                    let scrollbar = commands
                        .spawn((
                            Node {
                                min_width: px(12),
                                grid_column: GridPlacement::start(2),
                                ..default()
                            },
                            ThemeBackgroundColor(tokens::SCROLLBAR_BG),
                            Scrollbar {
                                orientation: ControlOrientation::Vertical,
                                target: #FileListRoot,
                                min_thumb_length: 24.,
                            },
                            children![(
                                ThemeBackgroundColor(tokens::SCROLLBAR_THUMB),
                                ScrollbarThumb {
                                    border_radius: BorderRadius::all(px(6)),
                                    border: UiRect::ZERO,
                                }
                            )],
                        ))
                        .id();
                    commands.entity(#ScrollFrame).add_child(scrollbar);
                }).await.unwrap();
        }
        async |ui: Ctx| {
            ui.bridge(|mut commands: Commands| {
                commands.entity(#This).trigger(RefreshDirectory);
            }).await;
        }
        async |ui: Ctx| {
            loop {
                ui.on::<Activate>(#UpButton).await;
                ui.cached_state::<(Commands, Query<(Entity, &mut CurrentDir), With<BrowserRoot>>)>()
                    .bridge(AsyncUi, |(mut commands, mut roots)| {
                    let Ok((root, mut current_dir)) = roots.single_mut() else {
                        return;
                    };

                    if current_dir.0.pop() {
                        commands.entity(root).trigger(RefreshDirectory);
                    }
                }).await.unwrap();
            }
        }
        async |ui: Ctx| {
            loop {
                ui.on::<Activate>(#RefreshButton).await;
                ui.cached_state::<(Commands, Query<Entity, With<BrowserRoot>>)>()
                    .bridge(AsyncUi, |(mut commands, roots)| {
                    let Ok(root) = roots.single() else {
                        return;
                    };
                    commands.entity(root).trigger(RefreshDirectory);
                }).await.unwrap();
            }
        }
        async |ui: Ctx| {
            loop {
                select! {
                    _ = ui.on::<Activate>(#NewestFirst).fuse() => {
                        ui.cached_state::<(
                            Commands,
                            Res<AssetServer>,
                            Query<(Entity, &mut SortOrder), With<BrowserRoot>>,
                            Query<&Children>,
                            Query<&LoadedEntries, With<FileListRoot>>,
                        )>()
                            .bridge(AsyncUi, |(mut commands, assets, mut roots, children, loaded_entries)| {
                            let Ok((_root, mut sort_order)) = roots.single_mut() else {
                                return;
                            };
                            *sort_order = SortOrder { newest_first: true };
                            queue_entries(&mut commands, assets.load(fonts::REGULAR), #FileListRoot, #This, *sort_order, &children, loaded_entries.single().ok().map(|entries| entries.0.clone()).unwrap_or_default());
                        }).await.unwrap();
                    }
                    _ = ui.on::<Activate>(#OldestFirst).fuse() => {
                        ui.cached_state::<(
                            Commands,
                            Res<AssetServer>,
                            Query<(Entity, &mut SortOrder), With<BrowserRoot>>,
                            Query<&Children>,
                            Query<&LoadedEntries, With<FileListRoot>>,
                        )>()
                            .bridge(AsyncUi, |(mut commands, assets, mut roots, children, loaded_entries)| {
                            let Ok((_root, mut sort_order)) = roots.single_mut() else {
                                return;
                            };
                            *sort_order = SortOrder { newest_first: false };
                            queue_entries(&mut commands, assets.load(fonts::REGULAR), #FileListRoot, #This, *sort_order, &children, loaded_entries.single().ok().map(|entries| entries.0.clone()).unwrap_or_default());
                        }).await.unwrap();
                    }
                }
            }
        }
        Children [(
            :pane
            Node {
                width: percent(100),
                height: percent(100),
            }
            Children [
                (
                    :pane_header
                    Children [
                        (
                            :tool_button(ButtonProps::default())
                            #UpButton
                            AutoFocus
                            Children [(Text::new("Up") ThemedText)]
                        ),
                        (
                            :tool_button(ButtonProps::default())
                            #RefreshButton
                            Children [(Text::new("Refresh") ThemedText)]
                        ),
                        :pane_header_divider,
                        (
                            :menu
                            Children [
                                (
                                    :menu_button(MenuButtonProps {
                                        caption: Box::new(bsn_list!((Text::new("Sort") ThemedText))),
                                        ..default()
                                    })
                                ),
                                (
                                    :menu_popup
                                    Children [
                                        (
                                            menu_item(MenuItemProps {
                                                caption: Box::new(bsn_list!((Text::new("Newest first") ThemedText))),
                                            })
                                            #NewestFirst
                                        ),
                                        (
                                            menu_item(MenuItemProps {
                                                caption: Box::new(bsn_list!((Text::new("Oldest first") ThemedText))),
                                            })
                                            #OldestFirst
                                        ),
                                        :menu_divider,
                                        (
                                            menu_item(MenuItemProps {
                                                caption: Box::new(bsn_list!((Text::new("Double-click rows to open") ThemedText))),
                                            })
                                        )
                                    ]
                                )
                            ]
                        ),
                        :flex_spacer,
                        (
                            Text::new("File Browser")
                            ThemedText
                        )
                    ]
                ),
                (
                    :pane_body
                    Node {
                        flex_grow: 1.0,
                        row_gap: px(8),
                    }
                    Children [
                        (
                            :subpane
                            Children [
                                (
                                    :subpane_header
                                    Children [
                                        (Text::new("Location") ThemedText),
                                        :flex_spacer,
                                        (
                                            Text::new("")
                                            ThemedText
                                            PathText
                                            #PathLabel
                                        )
                                    ]
                                ),
                                (
                                    :subpane_body
                                    Node {
                                        flex_grow: 1.0,
                                        padding: UiRect::all(px(0)),
                                        display: Display::Grid,
                                        grid_template_columns: {vec![
                                            GridTrack::flex(1.),
                                            GridTrack::px(12.),
                                        ]},
                                        column_gap: px(2),
                                    }
                                    #ScrollFrame
                                    Children [
                                        (
                                            Node {
                                                width: percent(100),
                                                height: percent(100),
                                                flex_direction: FlexDirection::Column,
                                                overflow: Overflow::scroll_y(),
                                                row_gap: px(2),
                                                grid_column: GridPlacement::start(1),
                                            }
                                            ScrollPosition(Vec2::ZERO)
                                            #FileListRoot
                                            FileListRoot
                                            on(|on_scroll: On<Pointer<Scroll>>,
                                                mut query: Query<(&mut ScrollPosition, &ComputedNode)>| {
                                                if let Ok((mut scroll_position, node)) = query.get_mut(on_scroll.entity) {
                                                    let dy = match on_scroll.unit {
                                                        MouseScrollUnit::Line => on_scroll.y * 24.,
                                                        MouseScrollUnit::Pixel => on_scroll.y,
                                                    };
                                                    let range = (node.content_size.y - node.size.y).max(0.)
                                                        * node.inverse_scale_factor;
                                                    scroll_position.y = (scroll_position.y - dy).clamp(0., range);
                                                }
                                            })
                                        ),
                                    ]
                                )
                            ]
                        ),
                        (
                            Text::new("")
                            ThemedText
                            StatusText
                            #StatusLabel
                        )
                    ]
                )
            ]
        )]
        on(|event: On<RefreshDirectory>,
            mut commands: Commands,
            assets: Res<AssetServer>,
            children: Query<&Children>,
            roots: Query<(&CurrentDir, &SortOrder), With<BrowserRoot>>,
            list_roots: Query<Entity, With<FileListRoot>>,
            mut path_texts: Query<&mut Text, (With<PathText>, Without<StatusText>)>,
            mut status_texts: Query<&mut Text, (With<StatusText>, Without<PathText>)>| {
            let Ok((current_dir, sort_order)) = roots.get(event.0) else {
                return;
            };
            let Ok(list_root) = list_roots.single() else {
                return;
            };

            if let Ok(mut path_text) = path_texts.single_mut() {
                *path_text = Text::new(current_dir.0.display().to_string());
            }

            match read_entries(&current_dir.0) {
                Ok(entries) => {
                    let count = entries.len();
                    queue_entries(&mut commands, assets.load(fonts::REGULAR), list_root, event.0, *sort_order, &children, entries);
                    if let Ok(mut status_text) = status_texts.single_mut() {
                        *status_text = Text::new(format!("{count} items"));
                    }
                }
                Err(err) => {
                    if let Ok(mut status_text) = status_texts.single_mut() {
                        *status_text = Text::new(format!("Could not read directory: {err}"));
                    }
                }
            }
        })
    }
}

#[derive(Clone)]
struct EntryRow {
    name: String,
    kind: &'static str,
    modified: String,
    modified_secs: u64,
    path: PathBuf,
    is_dir: bool,
}

fn read_entries(path: &Path) -> std::io::Result<Vec<EntryRow>> {
    let now = SystemTime::now();
    let mut entries = Vec::new();

    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let is_dir = metadata.is_dir();
        let name = entry
            .file_name()
            .to_string_lossy()
            .trim()
            .to_string()
            .if_empty_then(|| entry.path().display().to_string());

        let modified_secs = modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        entries.push(EntryRow {
            name,
            kind: if is_dir { "Folder" } else { "File" },
            modified: relative_modified_label(now, modified),
            modified_secs,
            path: entry.path(),
            is_dir,
        });
    }

    Ok(entries)
}

fn queue_entries(
    commands: &mut Commands,
    font: Handle<Font>,
    list_root: Entity,
    browser_root: Entity,
    sort_order: SortOrder,
    children: &Query<&Children>,
    mut entries: Vec<EntryRow>,
) {
    if sort_order.newest_first {
        entries.sort_by_key(|entry| Reverse(entry.modified_secs));
    } else {
        entries.sort_by_key(|entry| entry.modified_secs);
    }

    if let Ok(list_children) = children.get(list_root) {
        for child in list_children.iter() {
            commands.entity(child).despawn();
        }
    }

    let rows = entries
        .iter()
        .cloned()
        .map(|entry| spawn_entry_row(commands, browser_root, &font, entry))
        .collect::<Vec<_>>();

    commands
        .entity(list_root)
        .insert(LoadedEntries(entries))
        .replace_children(&rows);
}

fn spawn_entry_row(
    commands: &mut Commands,
    browser_root: Entity,
    font: &Handle<Font>,
    entry: EntryRow,
) -> Entity {
    let name = entry.name.clone();
    let kind = entry.kind;
    let modified = entry.modified.clone();
    let path = entry.path.clone();
    let is_dir = entry.is_dir;
    let glyph = if is_dir { ">" } else { "" };

    let row = commands
        .spawn((
            FileEntry {
                path: path.clone(),
                is_dir,
            },
            LastClick::default(),
            Node {
                width: percent(100),
                height: size::ROW_HEIGHT,
                padding: UiRect::axes(px(8), px(4)),
                display: Display::Grid,
                grid_template_columns: vec![
                    GridTrack::px(26.),
                    GridTrack::flex(1.),
                    GridTrack::px(88.),
                    GridTrack::px(120.),
                ],
                column_gap: px(8),
                align_items: AlignItems::Center,
                ..default()
            },
            Button,
            ButtonVariant::Normal,
            Hovered::default(),
            EntityCursor::System(SystemCursorIcon::Pointer),
            TabIndex(0),
            FocusIndicator,
            ThemeBackgroundColor(tokens::BUTTON_BG),
            InheritableThemeTextColor(tokens::BUTTON_TEXT),
            InheritableFont {
                font: font.clone(),
                font_size: size::MEDIUM_FONT,
                weight: FontWeight::NORMAL,
            },
        ))
        .observe(
            move |activate: On<Activate>,
                mut commands: Commands,
                mut rows: Query<(&FileEntry, &mut LastClick)>,
                mut roots: Query<&mut CurrentDir, With<BrowserRoot>>,
                mut status_texts: Query<&mut Text, With<StatusText>>| {
                let Ok((entry, mut last_click)) = rows.get_mut(activate.entity) else {
                    return;
                };

                let now = SystemTime::now();
                let is_double_click = last_click
                    .0
                    .and_then(|last| now.duration_since(last).ok())
                    .is_some_and(|age| age <= DOUBLE_CLICK_WINDOW);
                last_click.0 = Some(now);

                if !is_double_click {
                    return;
                }

                last_click.0 = None;

                if entry.is_dir {
                    let Ok(mut current_dir) = roots.single_mut() else {
                        return;
                    };
                    current_dir.0 = entry.path.clone();
                    commands.entity(browser_root).trigger(RefreshDirectory);
                } else {
                    if let Ok(mut status_text) = status_texts.single_mut() {
                        *status_text = Text::new(format!("Opening {}", entry.path.display()));
                    }
                    open_with_default_app(entry.path.clone());
                }
            },
        )
        .id();

    let cells = [
        spawn_row_cell(commands, glyph),
        spawn_row_cell(commands, name),
        spawn_row_cell(commands, kind),
        spawn_row_cell(commands, modified),
    ];
    commands.entity(row).add_children(&cells);

    row
}

fn spawn_row_cell(commands: &mut Commands, text: impl Into<String>) -> Entity {
    commands
        .spawn((
            Text::new(text),
            TextLayout::new_with_no_wrap(),
            ThemedText,
        ))
        .id()
}

fn open_with_default_app(path: PathBuf) {
    thread::spawn(move || {
        if let Err(err) = open_with_default_app_blocking(&path) {
            error!("Could not open {}: {err}", path.display());
        }
    });
}

fn open_with_default_app_blocking(path: &Path) -> std::io::Result<()> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    #[cfg(target_os = "macos")]
    {
        return run_default_opener("open", [path.as_os_str()]);
    }

    #[cfg(target_os = "windows")]
    {
        return run_default_opener(
            "cmd",
            [
                OsStr::new("/C"),
                OsStr::new("start"),
                OsStr::new(""),
                path.as_os_str(),
            ],
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // Linux and other freedesktop desktops do not expose this through Wayland.
        // gio and xdg-open both hand off to the user's configured default app;
        // gio tends to report launch failures more reliably, while xdg-open is
        // the broad freedesktop fallback.
        run_default_opener("gio", [OsStr::new("open"), path.as_os_str()])
            .or_else(|_| run_default_opener("xdg-open", [path.as_os_str()]))
    }
}

fn run_default_opener<'a>(
    program: &str,
    args: impl IntoIterator<Item = &'a OsStr>,
) -> std::io::Result<()> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

fn relative_modified_label(now: SystemTime, modified: SystemTime) -> String {
    let age = now.duration_since(modified).unwrap_or_default();
    let secs = age.as_secs();

    match secs {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", secs / 60),
        3_600..=86_399 => format!("{}h ago", secs / 3_600),
        86_400..=2_592_000 => format!("{}d ago", secs / 86_400),
        _ => format!("{}mo ago", secs / 2_592_000),
    }
}

trait EmptyStringExt {
    fn if_empty_then(self, fallback: impl FnOnce() -> String) -> String;
}

impl EmptyStringExt for String {
    fn if_empty_then(self, fallback: impl FnOnce() -> String) -> String {
        if self.is_empty() { fallback() } else { self }
    }
}
