use bevy::{
    feathers::{
        FeathersPlugins,
        controls::{
            ButtonVariant, FeathersButton, FeathersButtonProps, FeathersCheckbox,
            FeathersCheckboxProps, FeathersTextInput, FeathersTextInputContainer,
            FeathersTextInputProps,
        },
        dark_theme::create_dark_theme,
        theme::{ThemeBackgroundColor, ThemedText, UiTheme},
        tokens,
    },
    input_focus::{AutoFocus, tab_navigation::TabGroup},
    prelude::*,
    scene::{SceneComponent, prelude::Scene},
    text::EditableText,
    ui::Checked,
    ui_widgets::{Activate, ValueChange},
};
use bevy_malek_async::async_ui::{AsyncUi, AsyncUiPlugin, Ctx};
use bevy_malek_async::{AsyncPlugin, bsn_ui};
use futures::FutureExt;

fn button(props: FeathersButtonProps) -> impl Scene {
    FeathersButton::scene(props)
}

fn checkbox(props: FeathersCheckboxProps) -> impl Scene {
    FeathersCheckbox::scene(props)
}

fn text_input(props: FeathersTextInputProps) -> impl Scene {
    FeathersTextInput::scene(props)
}

fn text_input_container() -> impl Scene {
    FeathersTextInputContainer::scene(Default::default())
}

fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins,
            FeathersPlugins,
            AsyncPlugin::default(),
            AsyncUiPlugin,
        ))
        .insert_resource(UiTheme(create_dark_theme()))
        .add_systems(Startup, setup)
        .run();
}
fn setup(world: &mut World) -> Result {
    world.spawn(Camera2d);
    world.spawn_scene(todo_root())?;
    Ok(())
}

fn todo_root() -> impl Scene {
    #[derive(Component, FromTemplate)]
    struct ListItem;

    #[derive(EntityEvent)]
    struct RefreshList(Entity);

    #[derive(Component, FromTemplate)]
    enum FilterState {
        #[default]
        All,
        Completed,
        Active,
    }

    #[derive(Component, FromTemplate)]
    struct CheckBox(Entity);

    bsn_ui! {
        Node {
            width: percent(100),
            height: percent(100),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::FlexStart,
            padding: UiRect::axes(px(24), px(24)),
        }
        TabGroup
        ThemeBackgroundColor(tokens::WINDOW_BG)
        async |world: Ctx| {
            loop {
                 world.on::<Activate>(#AddTodoButton).await;
                 world.cached_state::<(Commands, Query<&EditableText>)>().bridge(AsyncUi, |(mut commands, texts)| {
                    let text = texts.get(#TodoInput).unwrap().value().to_string();
                    let todo_list_root: Entity = #TodoListRoot;
                    let child =
                        commands.spawn_scene(bsn_ui! {
                        #This
                        ListItem
                        CheckBox(#Checkbox)
                        Node {
                            width: percent(100),
                            align_items: AlignItems::Center,
                            justify_content: JustifyContent::SpaceBetween,
                            column_gap: px(8),
                            padding: UiRect::axes(px(8), px(6)),
                        }
                        Children [
                            (#Checkbox checkbox(FeathersCheckboxProps::default())),
                            (Text::new(text.clone()) ThemedText ),
                            (#Delete button(FeathersButtonProps::default()) Text("Delete") ThemedText),
                        ]
                        async |world: Ctx| {
                            world.on::<Activate>(#Delete).await;
                            world.cached_state::<Commands>().bridge(AsyncUi, |mut commands| {
                                commands.entity(#This).despawn();
                            }).await.unwrap();
                        }
                        async |ui: Ctx| {
                            loop {
                                let value_change = ui.on_cloned::<ValueChange<bool>, ()>(#Checkbox).await;
                                ui.bridge(|mut commands: Commands| {
                                    if value_change.value {
                                        commands.entity(#Checkbox).insert(Checked);
                                    } else {
                                        commands.entity(#Checkbox).remove::<Checked>();
                                    }
                                    commands.entity(todo_list_root).trigger(RefreshList);
                                }).await;
                            }
                        }
                    }).id();
                    commands.entity(#TodoListRoot).add_child(child);
                }).await.unwrap();
            }
        }
        async |world: Ctx| {
            loop {
                let filter_state;
                futures::select! {
                    _ = world.on::<Activate>(#AllFilter).fuse() => {
                        filter_state = FilterState::All;
                    }
                    _ = world.on::<Activate>(#ActiveFilter).fuse() => {
                        filter_state = FilterState::Active;
                    }
                    _ = world.on::<Activate>(#CompletedFilter).fuse() => {
                        filter_state = FilterState::Completed;
                    }
                }
                world.bridge(|mut commands: Commands| {
                    commands.entity(#TodoListRoot).insert(filter_state).trigger(RefreshList);
                }).await;
            }
        }
        Children [(
            Node {
                width: percent(100),
                max_width: px(760),
                flex_direction: FlexDirection::Column,
                row_gap: px(12),
                padding: UiRect::all(px(16)),
            }
            Children [
                (
                    Text::new("Todo List")
                    TextFont {
                        font_size: FontSize::Px(28.0),
                    }
                    ThemedText
                ),
                (
                    Text::new("Built with Bevy Feathers widgets and Async Reactivity.")
                    ThemedText
                ),
                (
                    Node {
                        column_gap: px(8),
                        align_items: AlignItems::Center,
                    }
                    Children [
                        (
                            :text_input_container
                            Node { flex_grow: 1.0 }
                            Children [(
                                text_input(FeathersTextInputProps {
                                    visible_width: None,
                                    max_characters: Some(120),
                                })
                                #TodoInput
                                AutoFocus
                            )]
                        ),
                        (
                            button(FeathersButtonProps {
                                variant: ButtonVariant::Primary,
                                ..default()
                            })
                            #AddTodoButton
                            Children [(Text::new("Add") ThemedText)]
                        )
                    ]
                ),
                (
                    Node {
                        align_items: AlignItems::Center,
                        justify_content: JustifyContent::SpaceBetween,
                        column_gap: px(8),
                    }
                    Children [
                        (
                            Node {
                                column_gap: px(8),
                            }
                            Children [
                                (
                                    button(FeathersButtonProps::default())
                                    #AllFilter
                                    Children [(Text::new("All") ThemedText)]
                                ),
                                (
                                    button(FeathersButtonProps::default())
                                    #ActiveFilter
                                    Children [(Text::new("Active") ThemedText)]
                                ),
                                (
                                    button(FeathersButtonProps::default())
                                    #CompletedFilter
                                    Children [(Text::new("Completed") ThemedText)]
                                ),
                            ]
                        ),
                    ]
                ),
                (
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: px(6),
                        min_height: px(240),
                        width: percent(100),
                        padding: UiRect::all(px(8)),
                    }
                    FilterState
                    #TodoListRoot
                    on(|event: On<RefreshList>,
                        filter_states: Query<&FilterState>,
                        children: Query<&Children>,
                        mut nodes: Query<(&mut Node, &CheckBox), With<ListItem>>,
                        has_checked: Query<Has<Checked>>| {
                        for child in children.iter_descendants(event.0) {
                            if let Ok((mut node, checkbox)) = nodes.get_mut(child) {
                                node.display = match (filter_states.get(event.0).unwrap(), has_checked.get(checkbox.0).unwrap())
                                {
                                    (FilterState::All, _) => Display::DEFAULT,
                                    (FilterState::Active, true) => Display::DEFAULT,
                                    (FilterState::Active, false) => Display::None,
                                    (FilterState::Completed, false) => Display::DEFAULT,
                                    (FilterState::Completed, true) => Display::None,
                                };
                            }
                        }
                    })
                ),
            ]
        )]
    }
}
