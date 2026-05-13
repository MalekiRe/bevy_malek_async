use bevy::{
    feathers::{
        FeathersPlugins,
        controls::{ButtonProps, button},
        dark_theme::create_dark_theme,
        theme::{ThemeBackgroundColor, ThemedText, UiTheme},
        tokens,
    },
    prelude::*,
    scene::prelude::Scene,
    ui_widgets::Activate,
};
use bevy_malek_async::async_ui::{AsyncUiPlugin, Ctx};
use bevy_malek_async::{AsyncPlugin, bsn_ui};
use futures::FutureExt;
use std::time::Duration;

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
    world.spawn_scene(demo_root())?;
    Ok(())
}

#[derive(Component, FromTemplate)]
struct ButtonNumber(i32);

fn demo_root() -> impl Scene {
    bsn_ui! {
      Node {
          width: percent(100),
          height: percent(100),
          align_items: AlignItems::Center,
          justify_content: JustifyContent::Center,
      }
      ThemeBackgroundColor(tokens::WINDOW_BG)
      Children[(
          Node {
              align_items: AlignItems::Center,
              justify_content: JustifyContent::Center,
          }
          Children [
              (#Minus
                  button(ButtonProps::default())
                  Children[(Text::new("-1") ThemedText)] ),
              (#Counter
                    Text::new("0")
                    ThemedText
                    ButtonNumber(0)
                    Node { margin: UiRect::horizontal(px(10.0)) } ),
              (#Plus
                  button(ButtonProps::default())
                  Children[(Text::new("+1") ThemedText)] )
          ]
      )]
      async |ui: Ctx| {
          loop {
              let mut number = ui.bridge(|mut query: Query<(&mut Text, &ButtonNumber)>| {
                  let (mut text, number) = query.get_mut(#Counter).unwrap();
                  text.0 = format!("{}", number.0);
                  number.0
              }).await;
              futures::select! {
                  _ = ui.on::<Activate>(#Minus).fuse() => { number -= 1 }
                  _ = ui.on::<Activate>(#Plus).fuse() => { number += 1 }
                  _ = ui.on_mutation::<ButtonNumber>(#Counter).fuse() => {
                       ui.bridge(|query: Query<&ButtonNumber>| {
                          number = query.get(#Counter).unwrap().0;
                       }).await;
                  }
              }
              ui.bridge(|mut query: Query<&mut ButtonNumber>| {
                query.get_mut(#Counter).unwrap().bypass_change_detection().0 = number;
              }).await;
          }
      }
      async |ui: Ctx| {
            loop {
                futures_timer::Delay::new(Duration::from_secs(1)).await;
                ui.bridge(|mut query: Query<&mut ButtonNumber>| {
                    query.get_mut(#Counter).unwrap().0 += 1;
                }).await;
            }
        }
    }
}
