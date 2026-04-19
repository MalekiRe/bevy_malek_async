# bevy_malek_async

Runtime‑agnostic async ECS access for Bevy. Run async work on any executor (Tokio, Bevy task pools, or another runtime) and safely hop into Bevy’s world to read or mutate ECS state using normal `SystemParam`s.


## Features

- Runtime‑agnostic: use with Tokio, Bevy task pools, or any async executor
- Familiar ECS access: acquire `Res`, `ResMut`, `Query`, and other `SystemParam`s
- Persistent state across calls: reuse an `EcsTask<P>` to preserve `Local`, `Changed`, etc.

## Have not updated the How to Use
Please look at the examples, `todo_list` show's off in-progress async ui and async observers.
