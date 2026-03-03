use bevy::prelude::*;
use bevy_ecs::world::WorldId;
use bevy_malek_async::{AsyncEcsPlugin, CreateEcsTask};

fn main() {
    App::new()
        // Keep Bevy minimal to avoid extra overhead
        .add_plugins(MinimalPlugins)
        .init_resource::<MyResource>()
        .add_plugins(AsyncEcsPlugin)
        // Spawn the Tokio web request from a Startup system
        .add_systems(Startup, spawn_tokio_web_request)
        .add_systems(Update, print_resource)
        .run();
}

fn print_resource(mut last: Local<String>, res: ResMut<MyResource>) {
    if last.as_str() != res.0.as_str() {
        *last = res.0.clone();
        println!("{}", res.0);
    }
}

fn spawn_tokio_web_request(world_id: WorldId) {
    println!("Starting Tokio web request in background task...");

    // Run the async task on its own OS thread with a dedicated Tokio runtime
    // so it doesn't require any global/runtime integration with Bevy.
    let _handle = std::thread::Builder::new()
        .name("tokio-http-task".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("create tokio runtime");

            rt.block_on(async move {
                if let Err(err) = fetch_example_com(world_id).await {
                    eprintln!("HTTP task error: {err}");
                }
            });
        })
        .expect("spawn tokio task thread");
}

async fn fetch_example_com(
    world_id: WorldId,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .user_agent("bevy-tokio-example/0.1")
        .build()?;

    let resp = client.get("https://www.google.com/").send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    println!("Fetched google.com: status={status}, bytes={}", body.len());
    world_id
        .ecs_task::<ResMut<MyResource>>()
        .run_system(Update, |mut my_resource| {
            my_resource.0 = body;
        })
        .await;

    Ok(())
}

#[derive(Default, Resource)]
struct MyResource(String);
