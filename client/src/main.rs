use bevy::pbr::prelude::{MeshMaterial3d, StandardMaterial};
use bevy::prelude::Mesh3d;
use bevy::prelude::*;
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::{SinkExt, StreamExt};
#[cfg(target_arch = "wasm32")]
use js_sys::Date;
use shared::{
    ClientToServer, GameConfig, GameSim, PlayerId, ServerToClient, TurnInput, Vec3 as SharedVec3,
    WorldState,
};
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[derive(Resource, Default)]
struct NetChannels {
    to_server: Option<UnboundedSender<String>>,
    from_server: Option<UnboundedReceiver<String>>,
}

#[derive(Resource)]
struct ClientInfo {
    id: Option<Uuid>,
    world_size: f32,
}

#[derive(Resource, Default)]
struct WorldCache {
    state: Option<WorldState>,
    last_tick: u64,
}

#[derive(Resource)]
struct LoadingState {
    welcome_received: bool,
    first_state_received: bool,
    state_count: u32,
    min_display_timer: Option<Timer>,
    loading_screen_entity: Option<Entity>,
}

impl Default for LoadingState {
    fn default() -> Self {
        Self {
            welcome_received: false,
            first_state_received: false,
            state_count: 0,
            min_display_timer: None,
            loading_screen_entity: None,
        }
    }
}

impl LoadingState {
    fn is_ready(&self) -> bool {
        if !self.welcome_received || !self.first_state_received {
            return false;
        }

        // Wait for at least 3 state updates to ensure we're synced
        if self.state_count < 3 {
            return false;
        }

        // Also ensure minimum display time of 1.5 seconds after first state
        if let Some(timer) = &self.min_display_timer {
            if !timer.finished() {
                return false;
            }
        } else {
            // Timer not started yet
            return false;
        }

        true
    }
}

#[derive(Resource)]
struct LocalSim {
    sim: GameSim,
    last_server_tick: u64,
    just_respawned: bool,
}

// Test player resources (for testing with arrow keys)
#[derive(Resource)]
struct TestPlayerInfo {
    id: Option<Uuid>,
    world_size: f32,
}

#[derive(Resource, Default)]
struct TestPlayerCache {
    state: Option<WorldState>,
    last_tick: u64,
}

#[derive(Resource, Default)]
struct TestPlayerChannels {
    to_server: Option<UnboundedSender<String>>,
    from_server: Option<UnboundedReceiver<String>>,
}

#[derive(Resource)]
struct TestPlayerSim {
    sim: GameSim,
    last_server_tick: u64,
    just_respawned: bool,
}

fn main() {
    #[cfg(target_arch = "wasm32")]
    {
        console_error_panic_hook::set_once();
        console_log::init_with_level(log::Level::Info).ok();
    }

    let mut app = App::new();
    #[cfg(target_arch = "wasm32")]
    {
        // Disable LogPlugin for WASM since we're using console_log
        app.add_plugins(DefaultPlugins.build().disable::<bevy::log::LogPlugin>());
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        app.add_plugins(DefaultPlugins);
    }

    app.insert_resource(ClientInfo {
        id: None,
        world_size: 0.0,
    })
    .insert_resource(WorldCache::default())
    .insert_resource(NetChannels::default())
    .insert_resource(PingTracker::default())
    .insert_resource(FpsCounter::default())
    .insert_resource(LoadingState::default())
    // Test player resources (always exist, but only populated in debug builds)
    .insert_resource(TestPlayerInfo {
        id: None,
        world_size: 0.0,
    })
    .insert_resource(TestPlayerCache::default())
    .insert_resource(TestPlayerChannels::default())
    .insert_resource(ClearColor(Color::srgb(0.05, 0.06, 0.09)))
    .add_systems(
        Startup,
        (
            setup_scene_3d,
            net_connect,
            #[cfg(debug_assertions)]
            net_connect_test_player,
            setup_loading_screen,
        ),
    )
    .add_systems(Update, (spawn_grid_once, update_loading_screen))
    .add_systems(
        Update,
        (
            net_pump,
            #[cfg(debug_assertions)]
            net_pump_test_player,
            send_player_input,
            #[cfg(debug_assertions)]
            send_test_player_input,
            local_player_move,
            #[cfg(debug_assertions)]
            test_player_move,
            update_truck_trailers,
            reconcile_server_state,
            #[cfg(debug_assertions)]
            reconcile_test_player_state,
            update_follow_cam,
            send_ping,
            update_hud,
            update_player_boost_visuals,
            update_boost_ui,
            interpolate_server_players,
            update_trailer_lines,
            update_minimap,
        ),
    )
    .add_systems(Update, sync_world_state.after(reconcile_server_state))
    .run();
}

#[derive(Component)]
struct SceneTag;

#[derive(Component)]
struct ServerPlayer {
    id: PlayerId,
}

#[derive(Component)]
struct ServerPlayerInterpolation {
    target_pos: Vec3,
    target_rot: Quat,
    prev_pos: Vec3,
    prev_rot: Quat,
    time_since_update: f32,
}

#[derive(Component)]
struct LocalPlayer {
    id: PlayerId,
}

#[derive(Component)]
struct TestPlayer {
    id: PlayerId,
}

#[derive(Component)]
struct ServerCollectible {
    id: Uuid,
}

#[derive(Component)]
struct ServerTruckTrailer {
    player_id: PlayerId,
    order: usize,
}

#[derive(Component)]
struct TrailerLine {
    player_id: PlayerId,
    from_order: usize, // 0 = player, 1+ = trailer order
}

#[derive(Component)]
struct FollowCam {
    offset: Vec3,
}

// Grid size will be set from server Welcome message

// WASM-compatible time tracking
#[cfg(not(target_arch = "wasm32"))]
type TimeInstant = std::time::Instant;
#[cfg(target_arch = "wasm32")]
#[derive(Clone, Copy)]
struct TimeInstant(f64); // milliseconds since epoch

#[cfg(not(target_arch = "wasm32"))]
fn time_now() -> TimeInstant {
    TimeInstant::now()
}

#[cfg(target_arch = "wasm32")]
fn time_now() -> TimeInstant {
    TimeInstant(Date::now())
}

#[cfg(not(target_arch = "wasm32"))]
fn time_elapsed(start: TimeInstant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0 // convert to milliseconds
}

#[cfg(target_arch = "wasm32")]
fn time_elapsed(start: TimeInstant) -> f64 {
    Date::now() - start.0
}

#[derive(Resource, Default)]
struct PingTracker {
    last_id: u64,
    in_flight: HashMap<u64, TimeInstant>,
    rtt_ms: f32,
}

#[derive(Resource, Default)]
struct FpsCounter {
    accum_time: f32,
    accum_frames: u32,
    fps: f32,
}

// Convert shared Vec3 to Bevy Vec3
fn shared_to_bevy_vec3(v: SharedVec3) -> Vec3 {
    Vec3::new(v.x, v.y, v.z)
}

fn setup_scene_3d(mut commands: Commands) {
    // Camera (3D)
    commands.spawn((
        Camera::default(),
        Camera3d::default(),
        Transform::from_xyz(0.0, 10.0, -16.0).looking_at(Vec3::new(0.0, 0.0, 8.0), Vec3::Y),
        GlobalTransform::default(),
        Visibility::default(),
        InheritedVisibility::default(),
        FollowCam {
            offset: Vec3::new(0.0, 12.0, -18.0),
        },
    ));
    // Light
    commands.spawn((
        PointLight {
            intensity: 4000.0,
            range: 200.0,
            shadows_enabled: true,
            ..default()
        },
        Transform::from_xyz(20.0, 30.0, 10.0),
        GlobalTransform::default(),
        Visibility::default(),
        InheritedVisibility::default(),
    ));
    // Wire grid will be spawned after we get grid_size from server
}

fn net_connect(mut chans: ResMut<NetChannels>) {
    if chans.to_server.is_some() {
        return;
    }
    let (tx_out, mut rx_out) = unbounded::<String>();
    let (tx_in, rx_in) = unbounded::<String>();
    chans.to_server = Some(tx_out.clone());
    chans.from_server = Some(rx_in);

    #[cfg(not(target_arch = "wasm32"))]
    let url =
        std::env::var("SERVER_WS_URL").unwrap_or_else(|_| "ws://127.0.0.1:4001/ws".to_string());
    #[cfg(target_arch = "wasm32")]
    let url = {
        let window = web_sys::window().expect("no global `window` exists");
        let location = window.location();

        // Check if custom server URL is provided via query parameter
        let custom_url = if let Ok(url_params) =
            web_sys::UrlSearchParams::new_with_str(location.search().unwrap_or_default().as_str())
        {
            url_params.get("server")
        } else {
            None
        };

        if let Some(server_url) = custom_url {
            server_url
        } else {
            // Determine WebSocket URL based on environment
            let hostname = location.hostname().unwrap_or_default();
            let port = location.port().unwrap_or_default();
            let protocol = if location.protocol().unwrap_or_default() == "https:" {
                "wss:"
            } else {
                "ws:"
            };

            // For localhost development on non-standard port, connect directly to server
            if (hostname == "127.0.0.1" || hostname == "localhost")
                && port != "80"
                && !port.is_empty()
            {
                "ws://127.0.0.1:4001/ws".to_string()
            } else {
                // Production or localhost on port 80: use nginx proxy (same host/port as page)
                format!("{}//{}/ws", protocol, location.host().unwrap_or_default())
            }
        }
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(async move {
                use tokio_tungstenite::connect_async;
                match connect_async(&url).await {
                    Ok((ws, _)) => {
                        let (mut write, mut read) = ws.split();
                        // read loop
                        let mut tx_in2 = tx_in.clone();
                        tokio::spawn(async move {
                            while let Some(msg) = read.next().await {
                                if let Ok(msg) = msg {
                                    if msg.is_text() {
                                        let _ = tx_in2.send(msg.into_text().unwrap()).await;
                                    }
                                }
                            }
                        });
                        // write loop
                        while let Some(out) = rx_out.next().await {
                            let _ = write.send(tungstenite::Message::Text(out)).await;
                        }
                    }
                    Err(e) => {
                        log::error!("websocket connect error: {e}");
                    }
                }
            });
        });
    }
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::prelude::*;
        use wasm_bindgen::JsCast;
        use wasm_bindgen_futures::spawn_local;
        use web_sys::{ErrorEvent, MessageEvent, WebSocket};
        spawn_local(async move {
            log::info!("Attempting to connect to WebSocket: {}", url);
            let ws = match WebSocket::new(&url) {
                Ok(ws) => ws,
                Err(e) => {
                    log::error!("Failed to create WebSocket: {:?}", e);
                    return;
                }
            };
            ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

            // Add onopen handler to log successful connection
            {
                let url_for_log = url.clone();
                let onopen = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                    log::info!("WebSocket connected to {}", url_for_log);
                });
                ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
                onopen.forget();
            }

            // Add onclose handler
            {
                let onclose = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                    log::warn!("WebSocket connection closed");
                });
                ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
                onclose.forget();
            }

            {
                let mut tx_in = tx_in.clone();
                let onmessage = Closure::<dyn FnMut(_)>::new(move |e: MessageEvent| {
                    if let Ok(txt) = e.data().dyn_into::<js_sys::JsString>() {
                        let _ = tx_in.unbounded_send(String::from(txt));
                    }
                });
                ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
                onmessage.forget();
            }
            {
                let onerror = Closure::<dyn FnMut(_)>::new(move |_e: ErrorEvent| {
                    // ErrorEvent.message() may not be available in all browsers
                    log::error!("WebSocket error occurred");
                });
                ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
                onerror.forget();
            }
            // write
            let ws_clone = ws.clone();
            spawn_local(async move {
                while let Some(out) = rx_out.next().await {
                    // Check if WebSocket is still open before sending
                    if ws_clone.ready_state() == web_sys::WebSocket::OPEN {
                        if let Err(e) = ws_clone.send_with_str(&out) {
                            log::error!("Failed to send WebSocket message: {:?}", e);
                            break;
                        }
                    } else {
                        log::warn!("WebSocket is not open, dropping message");
                        break;
                    }
                }
            });
        });
    }
}

// Test player connection (separate WebSocket) - debug builds only
#[cfg(debug_assertions)]
fn net_connect_test_player(mut chans: ResMut<TestPlayerChannels>) {
    if chans.to_server.is_some() {
        return;
    }
    let (tx_out, mut rx_out) = unbounded::<String>();
    let (tx_in, rx_in) = unbounded::<String>();
    chans.to_server = Some(tx_out.clone());
    chans.from_server = Some(rx_in);

    #[cfg(not(target_arch = "wasm32"))]
    let url =
        std::env::var("SERVER_WS_URL").unwrap_or_else(|_| "ws://127.0.0.1:4001/ws".to_string());
    #[cfg(target_arch = "wasm32")]
    let url = {
        let window = web_sys::window().expect("no global `window` exists");
        let location = window.location();
        if location.hostname().unwrap_or_default() == "127.0.0.1"
            || location.hostname().unwrap_or_default() == "localhost"
        {
            "ws://127.0.0.1:4001/ws".to_string()
        } else {
            let protocol = if location.protocol().unwrap_or_default() == "https:" {
                "wss:"
            } else {
                "ws:"
            };
            format!(
                "{}//{}:4001/ws",
                protocol,
                location.hostname().unwrap_or_default()
            )
        }
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(async move {
                use tokio_tungstenite::connect_async;
                match connect_async(&url).await {
                    Ok((ws, _)) => {
                        let (mut write, mut read) = ws.split();
                        let mut tx_in2 = tx_in.clone();
                        tokio::spawn(async move {
                            while let Some(msg) = read.next().await {
                                if let Ok(msg) = msg {
                                    if msg.is_text() {
                                        let _ = tx_in2.send(msg.into_text().unwrap()).await;
                                    }
                                }
                            }
                        });
                        while let Some(out) = rx_out.next().await {
                            let _ = write.send(tungstenite::Message::Text(out)).await;
                        }
                    }
                    Err(e) => {
                        log::error!("test player websocket connect error: {e}");
                    }
                }
            });
        });
    }
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::prelude::*;
        use wasm_bindgen::JsCast;
        use wasm_bindgen_futures::spawn_local;
        use web_sys::{ErrorEvent, MessageEvent, WebSocket};
        spawn_local(async move {
            log::info!("Test player: Attempting to connect to WebSocket: {}", url);
            let ws = match WebSocket::new(&url) {
                Ok(ws) => ws,
                Err(e) => {
                    log::error!("Test player: Failed to create WebSocket: {:?}", e);
                    return;
                }
            };
            ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

            {
                let url_for_log = url.clone();
                let onopen = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                    log::info!("Test player: WebSocket connected to {}", url_for_log);
                });
                ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
                onopen.forget();
            }

            {
                let onclose = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                    log::warn!("Test player: WebSocket connection closed");
                });
                ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
                onclose.forget();
            }

            {
                let mut tx_in = tx_in.clone();
                let onmessage = Closure::<dyn FnMut(_)>::new(move |e: MessageEvent| {
                    if let Ok(txt) = e.data().dyn_into::<js_sys::JsString>() {
                        let _ = tx_in.unbounded_send(String::from(txt));
                    }
                });
                ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
                onmessage.forget();
            }
            {
                let onerror = Closure::<dyn FnMut(_)>::new(move |_e: ErrorEvent| {
                    log::error!("Test player: WebSocket error occurred");
                });
                ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
                onerror.forget();
            }
            let ws_clone = ws.clone();
            spawn_local(async move {
                while let Some(out) = rx_out.next().await {
                    if ws_clone.ready_state() == web_sys::WebSocket::OPEN {
                        if let Err(e) = ws_clone.send_with_str(&out) {
                            log::error!("Test player: Failed to send WebSocket message: {:?}", e);
                            break;
                        }
                    } else {
                        log::warn!("Test player: WebSocket is not open, dropping message");
                        break;
                    }
                }
            });
        });
    }
}

fn net_pump(
    mut commands: Commands,
    mut chans: ResMut<NetChannels>,
    mut cache: ResMut<WorldCache>,
    mut client: ResMut<ClientInfo>,
    mut ping: ResMut<PingTracker>,
    mut loading: ResMut<LoadingState>,
) {
    if let Some(rx) = chans.from_server.as_mut() {
        let mut msgs = Vec::new();
        while let Ok(Some(m)) = rx.try_next() {
            msgs.push(m);
        }
        for m in msgs {
            if let Ok(msg) = serde_json::from_str::<ServerToClient>(&m) {
                match msg {
                    ServerToClient::Welcome { id, world_size } => {
                        client.id = Some(id);
                        client.world_size = world_size;
                        cache.state = None;
                        loading.welcome_received = true;
                        // Initialize local simulation
                        let mut local_sim = LocalSim {
                            sim: GameSim::new(GameConfig {
                                world_size,
                                player_speed: 6.0,
                                turn_speed: 2.5,
                                initial_length: 3,
                                item_spawn_every_ticks: 20,
                            }),
                            last_server_tick: 0,
                            just_respawned: false,
                        };
                        // Add local player to sim
                        local_sim.sim.state.players.insert(
                            id,
                            shared::PlayerState {
                                id,
                                position: SharedVec3 {
                                    x: 0.0,
                                    y: 0.5,
                                    z: 0.0,
                                },
                                rotation_y: 0.0,
                                trailer: std::collections::VecDeque::new(),
                                alive: true,
                                boost_meter: 1.0,
                            },
                        );
                        commands.insert_resource(local_sim);
                    }
                    ServerToClient::State(world) => {
                        if !loading.first_state_received {
                            loading.first_state_received = true;
                            loading.state_count = 1;
                            // Start timer for minimum display time
                            loading.min_display_timer =
                                Some(Timer::from_seconds(1.5, TimerMode::Once));
                        } else {
                            loading.state_count += 1;
                        }
                        cache.state = Some(world);
                    }
                    ServerToClient::Pong(id) => {
                        if let Some(start) = ping.in_flight.remove(&id) {
                            let rtt_ms = time_elapsed(start);
                            ping.rtt_ms = rtt_ms as f32;
                        }
                    }
                    ServerToClient::YouDied => {}
                }
            }
        }
    }
}

// Test player net pump - debug builds only
#[cfg(debug_assertions)]
fn net_pump_test_player(
    mut commands: Commands,
    mut chans: ResMut<TestPlayerChannels>,
    mut cache: ResMut<TestPlayerCache>,
    mut test_client: ResMut<TestPlayerInfo>,
) {
    if let Some(rx) = chans.from_server.as_mut() {
        let mut msgs = Vec::new();
        while let Ok(Some(m)) = rx.try_next() {
            msgs.push(m);
        }
        for m in msgs {
            if let Ok(msg) = serde_json::from_str::<ServerToClient>(&m) {
                match msg {
                    ServerToClient::Welcome { id, world_size } => {
                        test_client.id = Some(id);
                        test_client.world_size = world_size;
                        cache.state = None;
                        // Initialize test player simulation
                        let mut test_sim = TestPlayerSim {
                            sim: GameSim::new(GameConfig {
                                world_size,
                                player_speed: 6.0,
                                turn_speed: 2.5,
                                initial_length: 3,
                                item_spawn_every_ticks: 20,
                            }),
                            last_server_tick: 0,
                            just_respawned: false,
                        };
                        // Add test player to sim
                        test_sim.sim.state.players.insert(
                            id,
                            shared::PlayerState {
                                id,
                                position: SharedVec3 {
                                    x: 0.0,
                                    y: 0.5,
                                    z: 0.0,
                                },
                                rotation_y: 0.0,
                                trailer: std::collections::VecDeque::new(),
                                alive: true,
                                boost_meter: 1.0,
                            },
                        );
                        commands.insert_resource(test_sim);
                    }
                    ServerToClient::State(world) => {
                        cache.state = Some(world);
                    }
                    _ => {}
                }
            }
        }
    }
}

// Send player input to server and apply locally immediately (client-side prediction)
fn send_player_input(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    client: Res<ClientInfo>,
    chans: ResMut<NetChannels>,
    mut local_sim: Option<ResMut<LocalSim>>,
    mut timer: Local<Option<Timer>>,
) {
    if client.id.is_none() {
        return;
    }
    if chans.to_server.is_none() {
        return;
    }
    let Some(mut sim) = local_sim else {
        return;
    };

    // Send input at a fixed rate (every 50ms = 20 times per second)
    if timer.is_none() {
        *timer = Some(Timer::from_seconds(0.05, TimerMode::Repeating));
    }
    let t = timer.as_mut().unwrap();
    t.tick(time.delta());
    if !t.just_finished() {
        return;
    }

    // Determine turn input from keys (A/D only, arrow keys are for test player)
    let turn = if keys.pressed(KeyCode::KeyA) {
        TurnInput::Left
    } else if keys.pressed(KeyCode::KeyD) {
        TurnInput::Right
    } else {
        TurnInput::Straight
    };

    // Check for boost input (W key)
    let boost = keys.pressed(KeyCode::KeyW);

    // Apply input locally immediately (client-side prediction)
    if let Some(my_id) = client.id {
        sim.sim.submit_input(my_id, turn);
        sim.sim.submit_boost(my_id, boost);
    }

    // Send input to server
    if let Some(tx) = &chans.to_server {
        let msg = ClientToServer::Input { turn, boost };
        if let Ok(json) = serde_json::to_string(&msg) {
            let _ = tx.unbounded_send(json);
        }
    }
}

// Send test player input (arrow keys only) - debug builds only
#[cfg(debug_assertions)]
fn send_test_player_input(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    test_client: Res<TestPlayerInfo>,
    chans: ResMut<TestPlayerChannels>,
    mut test_sim: Option<ResMut<TestPlayerSim>>,
    mut timer: Local<Option<Timer>>,
) {
    if test_client.id.is_none() {
        return;
    }
    if chans.to_server.is_none() {
        return;
    }
    let Some(mut sim) = test_sim else {
        return;
    };

    // Send input at a fixed rate (every 50ms = 20 times per second)
    if timer.is_none() {
        *timer = Some(Timer::from_seconds(0.05, TimerMode::Repeating));
    }
    let t = timer.as_mut().unwrap();
    t.tick(time.delta());
    if !t.just_finished() {
        return;
    }

    // Determine turn input from arrow keys only
    let turn = if keys.pressed(KeyCode::ArrowLeft) {
        TurnInput::Left
    } else if keys.pressed(KeyCode::ArrowRight) {
        TurnInput::Right
    } else {
        TurnInput::Straight
    };

    // Check for boost input (W key for test player too)
    let boost = keys.pressed(KeyCode::KeyW);

    // Apply input locally immediately (client-side prediction)
    if let Some(test_id) = test_client.id {
        sim.sim.submit_input(test_id, turn);
        sim.sim.submit_boost(test_id, boost);
    }

    // Send input to server
    if let Some(tx) = &chans.to_server {
        let msg = ClientToServer::Input { turn, boost };
        if let Ok(json) = serde_json::to_string(&msg) {
            let _ = tx.unbounded_send(json);
        }
    }
}

// Helper function to update trailer with actual cart positions (matching server logic)
fn update_trailer_positions(player: &mut shared::PlayerState) {
    let gap = 0.8;
    let player_back_offset = 0.9;
    let cart_front_offset = 0.7;
    let cart_back_offset = 0.7;
    let hitch_length = gap + cart_front_offset;

    let player_forward = shared::Vec3 {
        x: player.rotation_y.sin(),
        y: 0.0,
        z: player.rotation_y.cos(),
    };

    // Calculate new cart positions based on current player state
    let mut new_trailer = std::collections::VecDeque::new();
    new_trailer.push_back(player.position); // First element is always player position

    let trailer_length = player.trailer.len();
    if trailer_length > 1 {
        // Get previous cart positions for direction calculation
        let mut prev_cart_pos: Option<shared::Vec3> = None;
        let mut prev_cart_forward: Option<shared::Vec3> = None;

        // Use previous trailer positions to get direction, but recalculate actual positions
        let mut old_trailer_iter = player.trailer.iter().skip(1);

        for order in 0..(trailer_length - 1) {
            let old_pos = old_trailer_iter.next();

            let (cart_pos, cart_forward) = if order == 0 {
                // First cart: attached to player
                let hitch_point = shared::Vec3 {
                    x: player.position.x - player_forward.x * player_back_offset,
                    y: 0.5,
                    z: player.position.z - player_forward.z * player_back_offset,
                };

                // Use old position to determine direction if available
                if let Some(&old_cart_pos) = old_pos {
                    let to_hitch = shared::Vec3 {
                        x: hitch_point.x - old_cart_pos.x,
                        y: 0.0,
                        z: hitch_point.z - old_cart_pos.z,
                    };
                    let to_hitch_dist = (to_hitch.x * to_hitch.x + to_hitch.z * to_hitch.z).sqrt();

                    if to_hitch_dist > 0.001 {
                        let to_hitch_dir = shared::Vec3 {
                            x: to_hitch.x / to_hitch_dist,
                            y: 0.0,
                            z: to_hitch.z / to_hitch_dist,
                        };
                        let cart_pos = shared::Vec3 {
                            x: hitch_point.x - to_hitch_dir.x * hitch_length,
                            y: 0.5,
                            z: hitch_point.z - to_hitch_dir.z * hitch_length,
                        };
                        (cart_pos, to_hitch_dir)
                    } else {
                        let backward = shared::Vec3 {
                            x: -player_forward.x,
                            y: 0.0,
                            z: -player_forward.z,
                        };
                        let cart_pos = shared::Vec3 {
                            x: hitch_point.x + backward.x * hitch_length,
                            y: 0.5,
                            z: hitch_point.z + backward.z * hitch_length,
                        };
                        (cart_pos, player_forward)
                    }
                } else {
                    let backward = shared::Vec3 {
                        x: -player_forward.x,
                        y: 0.0,
                        z: -player_forward.z,
                    };
                    let cart_pos = shared::Vec3 {
                        x: hitch_point.x + backward.x * hitch_length,
                        y: 0.5,
                        z: hitch_point.z + backward.z * hitch_length,
                    };
                    (cart_pos, player_forward)
                }
            } else {
                // Subsequent carts
                if let (Some(prev_pos), Some(prev_fwd)) = (prev_cart_pos, prev_cart_forward) {
                    let hitch_point = shared::Vec3 {
                        x: prev_pos.x - prev_fwd.x * cart_back_offset,
                        y: 0.5,
                        z: prev_pos.z - prev_fwd.z * cart_back_offset,
                    };

                    if let Some(&old_cart_pos) = old_pos {
                        let to_hitch = shared::Vec3 {
                            x: hitch_point.x - old_cart_pos.x,
                            y: 0.0,
                            z: hitch_point.z - old_cart_pos.z,
                        };
                        let to_hitch_dist =
                            (to_hitch.x * to_hitch.x + to_hitch.z * to_hitch.z).sqrt();

                        if to_hitch_dist > 0.001 {
                            let to_hitch_dir = shared::Vec3 {
                                x: to_hitch.x / to_hitch_dist,
                                y: 0.0,
                                z: to_hitch.z / to_hitch_dist,
                            };
                            let cart_pos = shared::Vec3 {
                                x: hitch_point.x - to_hitch_dir.x * hitch_length,
                                y: 0.5,
                                z: hitch_point.z - to_hitch_dir.z * hitch_length,
                            };
                            (cart_pos, to_hitch_dir)
                        } else {
                            let backward = shared::Vec3 {
                                x: -prev_fwd.x,
                                y: 0.0,
                                z: -prev_fwd.z,
                            };
                            let cart_pos = shared::Vec3 {
                                x: hitch_point.x + backward.x * hitch_length,
                                y: 0.5,
                                z: hitch_point.z + backward.z * hitch_length,
                            };
                            (cart_pos, prev_fwd)
                        }
                    } else {
                        let backward = shared::Vec3 {
                            x: -prev_fwd.x,
                            y: 0.0,
                            z: -prev_fwd.z,
                        };
                        let cart_pos = shared::Vec3 {
                            x: hitch_point.x + backward.x * hitch_length,
                            y: 0.5,
                            z: hitch_point.z + backward.z * hitch_length,
                        };
                        (cart_pos, prev_fwd)
                    }
                } else {
                    // Fallback
                    let backward = shared::Vec3 {
                        x: -player_forward.x,
                        y: 0.0,
                        z: -player_forward.z,
                    };
                    let cart_pos = shared::Vec3 {
                        x: player.position.x + backward.x * hitch_length * (order as f32 + 1.0),
                        y: 0.5,
                        z: player.position.z + backward.z * hitch_length * (order as f32 + 1.0),
                    };
                    (cart_pos, player_forward)
                }
            };

            new_trailer.push_back(cart_pos);
            prev_cart_pos = Some(cart_pos);
            prev_cart_forward = Some(cart_forward);
        }
    }

    // Update trailer with new positions
    player.trailer = new_trailer;
}

// Move local player every frame (client-side prediction)
fn local_player_move(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    client: Res<ClientInfo>,
    mut local_sim: Option<ResMut<LocalSim>>,
    mut q_local_player: Query<&mut Transform, (With<LocalPlayer>, Without<Camera>)>,
) {
    let Some(mut sim) = local_sim else {
        return;
    };
    let Some(my_id) = client.id else {
        return;
    };

    // Skip transform update if we just respawned (sync_world_state will handle it)
    let just_respawned = sim.just_respawned;

    let dt = time.delta_secs();
    let world_size = sim.sim.cfg.world_size;
    let turn_speed = sim.sim.cfg.turn_speed;
    let player_speed = sim.sim.cfg.player_speed;

    // Get local player from sim
    let Some(player) = sim.sim.state.players.get_mut(&my_id) else {
        return;
    };
    if !player.alive {
        return;
    }

    // Apply turn input every frame based on current key state (smooth turning)
    // A/D only, arrow keys are for test player
    if keys.pressed(KeyCode::KeyA) {
        player.rotation_y += turn_speed * dt;
    } else if keys.pressed(KeyCode::KeyD) {
        player.rotation_y -= turn_speed * dt;
    }

    // Handle boost input and update boost meter (same logic as server)
    let boost_pressed = keys.pressed(KeyCode::KeyW);
    let boost_active = boost_pressed && player.boost_meter > 0.0;

    if boost_active {
        // Deplete boost meter while boosting (depletes in 2 seconds at full speed)
        let deplete_rate = 1.0 / 2.0; // Deplete full meter in 2 seconds
        player.boost_meter -= deplete_rate * dt;
        if player.boost_meter < 0.0 {
            player.boost_meter = 0.0;
        }
    } else {
        // Regenerate boost meter slowly when not boosting (regenerates in 5 seconds)
        let regen_rate = 1.0 / 5.0; // Regenerate full meter in 5 seconds
        player.boost_meter += regen_rate * dt;
        if player.boost_meter > 1.0 {
            player.boost_meter = 1.0;
        }
    }

    // Apply movement (same logic as server) with boost multiplier
    let speed_multiplier = if boost_active { 2.0 } else { 1.0 };
    let forward_x = player.rotation_y.sin();
    let forward_z = player.rotation_y.cos();
    player.position.x += forward_x * player_speed * speed_multiplier * dt;
    player.position.z += forward_z * player_speed * speed_multiplier * dt;

    // Clamp position to world bounds (walls will kill on server, but prevent visual glitches)
    let player_radius = 0.5;
    player.position.x = player
        .position
        .x
        .clamp(-world_size + player_radius, world_size - player_radius);
    player.position.z = player
        .position
        .z
        .clamp(-world_size + player_radius, world_size - player_radius);
    player.position.y = 0.5;

    // Don't update trailer positions here - let the server be authoritative
    // The server will update trailer positions, and we sync from it in reconcile_server_state
    // This prevents desync issues where client has different trailer length than server

    // Update visual transform immediately, but skip if we just respawned
    // (sync_world_state will handle the instant update)
    if !just_respawned {
        if let Ok(mut transform) = q_local_player.single_mut() {
            let pos = shared_to_bevy_vec3(player.position);
            let rot = Quat::from_rotation_y(player.rotation_y);
            transform.translation = pos;
            transform.rotation = rot;
        }
    }
}

// Move test player every frame (client-side prediction) - debug builds only
#[cfg(debug_assertions)]
fn test_player_move(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    test_client: Res<TestPlayerInfo>,
    mut test_sim: Option<ResMut<TestPlayerSim>>,
    mut q_test_player: Query<
        &mut Transform,
        (With<TestPlayer>, Without<Camera>, Without<LocalPlayer>),
    >,
) {
    let Some(mut sim) = test_sim else {
        return;
    };
    let Some(test_id) = test_client.id else {
        return;
    };

    // Skip transform update if we just respawned (sync_world_state will handle it)
    let just_respawned = sim.just_respawned;

    let dt = time.delta_secs();
    let world_size = sim.sim.cfg.world_size;
    let turn_speed = sim.sim.cfg.turn_speed;
    let player_speed = sim.sim.cfg.player_speed;

    // Get test player from sim
    let Some(player) = sim.sim.state.players.get_mut(&test_id) else {
        return;
    };
    if !player.alive {
        return;
    }

    // Apply turn input every frame based on arrow keys (smooth turning)
    if keys.pressed(KeyCode::ArrowLeft) {
        player.rotation_y += turn_speed * dt;
    } else if keys.pressed(KeyCode::ArrowRight) {
        player.rotation_y -= turn_speed * dt;
    }

    // Handle boost input and update boost meter (same logic as server)
    let boost_pressed = keys.pressed(KeyCode::KeyW);
    let boost_active = boost_pressed && player.boost_meter > 0.0;

    if boost_active {
        // Deplete boost meter while boosting (depletes in 2 seconds at full speed)
        let deplete_rate = 1.0 / 2.0; // Deplete full meter in 2 seconds
        player.boost_meter -= deplete_rate * dt;
        if player.boost_meter < 0.0 {
            player.boost_meter = 0.0;
        }
    } else {
        // Regenerate boost meter slowly when not boosting (regenerates in 5 seconds)
        let regen_rate = 1.0 / 5.0; // Regenerate full meter in 5 seconds
        player.boost_meter += regen_rate * dt;
        if player.boost_meter > 1.0 {
            player.boost_meter = 1.0;
        }
    }

    // Apply movement (same logic as server) with boost multiplier
    let speed_multiplier = if boost_active { 2.0 } else { 1.0 };
    let forward_x = player.rotation_y.sin();
    let forward_z = player.rotation_y.cos();
    player.position.x += forward_x * player_speed * speed_multiplier * dt;
    player.position.z += forward_z * player_speed * speed_multiplier * dt;

    // Clamp position to world bounds (walls will kill on server, but prevent visual glitches)
    let player_radius = 0.5;
    player.position.x = player
        .position
        .x
        .clamp(-world_size + player_radius, world_size - player_radius);
    player.position.z = player
        .position
        .z
        .clamp(-world_size + player_radius, world_size - player_radius);
    player.position.y = 0.5;

    // Don't update trailer positions here - let the server be authoritative
    // The server will update trailer positions, and we sync from it in reconcile_server_state
    // This prevents desync issues where client has different trailer length than server

    // Update visual transform immediately, but skip if we just respawned
    // (sync_world_state will handle the instant update)
    if !just_respawned {
        if let Ok(mut transform) = q_test_player.single_mut() {
            let pos = shared_to_bevy_vec3(player.position);
            let rot = Quat::from_rotation_y(player.rotation_y);
            transform.translation = pos;
            transform.rotation = rot;
        }
    }
}

// Update truck trailer positions every frame - truck trailer physics with dynamic swinging
fn update_truck_trailers(
    time: Res<Time>,
    client: Res<ClientInfo>,
    test_client: Res<TestPlayerInfo>,
    local_sim: Option<Res<LocalSim>>,
    test_sim: Option<Res<TestPlayerSim>>,
    mut q_carts: Query<(&ServerTruckTrailer, &mut Transform)>,
    q_local_player: Query<(&LocalPlayer, &Transform), Without<ServerTruckTrailer>>,
    q_test_player: Query<(&TestPlayer, &Transform), Without<ServerTruckTrailer>>,
    q_server_players: Query<(&ServerPlayer, &Transform), Without<ServerTruckTrailer>>,
) {
    // Use main sim for most players, but also check test sim for test player
    let Some(sim) = local_sim else {
        return;
    };
    let Some(_my_id) = client.id else {
        return;
    };
    let test_id = test_client.id;

    let dt = time.delta_secs();

    // Physics parameters for truck trailer behavior
    let gap = 0.8;
    let player_back_offset = 0.9; // Distance from player center to player back
    let cart_front_offset = 0.7; // Distance from cart center to cart front
    let cart_back_offset = 0.7; // Distance from cart center to cart back
    let hitch_length = gap + cart_front_offset; // Total distance from attachment point to cart center

    // Build a map of player transforms (rendered positions)
    let mut player_transforms: HashMap<PlayerId, Transform> = HashMap::new();

    // Get local player transform
    if let Ok((local_player, transform)) = q_local_player.single() {
        player_transforms.insert(local_player.id, *transform);
    }

    // Get test player transform (debug builds only)
    #[cfg(debug_assertions)]
    {
        if let Ok((test_player, transform)) = q_test_player.single() {
            player_transforms.insert(test_player.id, *transform);
        }
    }

    // Get server player transforms
    for (server_player, transform) in q_server_players.iter() {
        player_transforms.insert(server_player.id, *transform);
    }

    // Group carts by player and sort by order
    let mut carts_by_player: std::collections::HashMap<_, Vec<_>> =
        std::collections::HashMap::new();
    for (cart, transform) in q_carts.iter() {
        carts_by_player
            .entry(cart.player_id)
            .or_insert_with(Vec::new)
            .push((cart.order, transform.translation, transform.rotation));
    }

    // Calculate target positions for all carts (process in order to build chain)
    let mut cart_targets: std::collections::HashMap<(PlayerId, usize), (Vec3, Quat)> =
        std::collections::HashMap::new();

    for (player_id, cart_list) in carts_by_player.iter() {
        // Get player transform (rendered position)
        let Some(player_transform) = player_transforms.get(player_id) else {
            continue;
        };

        // Check if this player belongs to test player, if so use test sim (debug builds only)
        let player_state = {
            #[cfg(debug_assertions)]
            {
                if test_id.is_some() && *player_id == test_id.unwrap() {
                    test_sim
                        .as_ref()
                        .and_then(|ts| ts.sim.state.players.get(player_id))
                } else {
                    sim.sim.state.players.get(player_id)
                }
            }
            #[cfg(not(debug_assertions))]
            {
                sim.sim.state.players.get(player_id)
            }
        };

        if let Some(player_state) = player_state {
            if !player_state.alive {
                continue;
            }

            // Sort by order to process sequentially
            let mut sorted_carts: Vec<_> = cart_list.iter().collect();
            sorted_carts.sort_by_key(|(order, _, _)| *order);

            // Process carts in order, building the chain with truck trailer physics
            for (order, cart_pos, cart_rot) in sorted_carts {
                let (target_world_pos, target_rot) = if *order == 1 {
                    // First trailer: attached to truck (player)
                    // Calculate hitch point on the truck (back of player)
                    let player_forward = player_transform.rotation * Vec3::Z;
                    let hitch_point =
                        player_transform.translation - player_forward * player_back_offset;

                    // Direction from current cart position to hitch point
                    let to_hitch = hitch_point - *cart_pos;
                    let to_hitch_dist = to_hitch.length();

                    if to_hitch_dist > 0.001 {
                        let to_hitch_dir = to_hitch / to_hitch_dist;

                        // Target position: hitch point minus hitch_length along the direction
                        // This creates a natural swinging motion
                        let target_pos = hitch_point - to_hitch_dir * hitch_length;
                        let target_pos = Vec3::new(target_pos.x, 0.4, target_pos.z);

                        // Rotation: align with the direction from cart to hitch (trailer follows path)
                        let target_rotation = Quat::from_rotation_arc(Vec3::Z, to_hitch_dir);

                        (target_pos, target_rotation)
                    } else {
                        // Fallback: straight line behind player
                        let target_pos = hitch_point - player_forward * hitch_length;
                        let target_pos = Vec3::new(target_pos.x, 0.4, target_pos.z);
                        (target_pos, player_transform.rotation)
                    }
                } else {
                    // Subsequent trailers: attached to previous trailer
                    let prev_cart_order = *order - 1;
                    let prev_cart_key = (*player_id, prev_cart_order);

                    if let Some((prev_cart_target_pos, prev_cart_target_rot)) =
                        cart_targets.get(&prev_cart_key)
                    {
                        // Calculate hitch point on previous trailer (back of previous trailer)
                        let prev_forward = *prev_cart_target_rot * Vec3::Z;
                        let hitch_point = *prev_cart_target_pos - prev_forward * cart_back_offset;

                        // Direction from current cart position to hitch point
                        let to_hitch = hitch_point - *cart_pos;
                        let to_hitch_dist = to_hitch.length();

                        if to_hitch_dist > 0.001 {
                            let to_hitch_dir = to_hitch / to_hitch_dist;

                            // Target position: hitch point minus hitch_length along the direction
                            let target_pos = hitch_point - to_hitch_dir * hitch_length;
                            let target_pos = Vec3::new(target_pos.x, 0.4, target_pos.z);

                            // Rotation: align with the direction from cart to hitch
                            let target_rotation = Quat::from_rotation_arc(Vec3::Z, to_hitch_dir);

                            (target_pos, target_rotation)
                        } else {
                            // Fallback: straight line behind previous trailer
                            let target_pos = hitch_point - prev_forward * hitch_length;
                            let target_pos = Vec3::new(target_pos.x, 0.4, target_pos.z);
                            (target_pos, *prev_cart_target_rot)
                        }
                    } else {
                        // Fallback: use current transform if previous cart not found
                        (*cart_pos, *cart_rot)
                    }
                };

                // Store the target for next cart to use and for applying later
                cart_targets.insert((*player_id, *order), (target_world_pos, target_rot));
            }
        }
    }

    // Apply the calculated targets with physics-based smoothing (allows for swinging)
    for (cart, mut transform) in q_carts.iter_mut() {
        let key = (cart.player_id, cart.order);
        if let Some((target_pos, target_rot)) = cart_targets.get(&key) {
            // Use different smoothing factors for position and rotation
            // Position: faster response for more dynamic movement
            let pos_smooth = 1.0 - (-dt * 12.0).exp(); // ~12x per second
                                                       // Rotation: slightly slower for more natural swinging
            let rot_smooth = 1.0 - (-dt * 10.0).exp(); // ~10x per second

            transform.translation = transform.translation.lerp(*target_pos, pos_smooth);
            transform.rotation = transform.rotation.slerp(*target_rot, rot_smooth);
        }
    }
}

// Reconcile local state with server state (accounting for ping)
fn reconcile_server_state(
    time: Res<Time>,
    mut cache: ResMut<WorldCache>,
    client: Res<ClientInfo>,
    ping: Res<PingTracker>,
    mut local_sim: Option<ResMut<LocalSim>>,
) {
    let Some(world) = &cache.state else {
        return;
    };
    let Some(mut sim) = local_sim else {
        return;
    };
    let Some(my_id) = client.id else {
        return;
    };

    // Only reconcile when we get a new server tick
    if world.tick <= sim.last_server_tick {
        return;
    }

    // Save local player state before updating from server
    let my_local_player = sim.sim.state.players.get(&my_id).cloned();

    // Update all players and items from server
    sim.sim.state.players = world.players.clone();
    sim.sim.state.items = world.items.clone();

    // Reconcile local player: smoothly correct towards server position
    let server_player_opt = sim.sim.state.players.get(&my_id).cloned();
    if let Some(server_player) = server_player_opt {
        // If player was dead and is now alive, use server state directly (respawn)
        let was_dead = my_local_player.as_ref().map_or(false, |p| !p.alive);
        let is_now_alive = server_player.alive;

        if was_dead && is_now_alive {
            // Player respawned - use server state directly
            sim.sim.state.players.insert(my_id, server_player);
            sim.just_respawned = true; // Flag for instant transform update
        } else {
            sim.just_respawned = false;
            if let Some(mut local_player) = my_local_player {
                // Normal reconciliation - smoothly correct towards server position
                // Use frame-rate independent exponential smoothing
                let server_pos = server_player.position;
                let dt = time.delta_secs();
                let correction_rate = 15.0; // corrections per second
                let correction_factor = 1.0 - (-dt * correction_rate).exp();
                local_player.position.x +=
                    (server_pos.x - local_player.position.x) * correction_factor;
                local_player.position.z +=
                    (server_pos.z - local_player.position.z) * correction_factor;

                // Smoothly correct rotation (handle angle wrapping)
                let rot_diff = server_player.rotation_y - local_player.rotation_y;
                // Normalize to [-PI, PI]
                let rot_diff_normalized = ((rot_diff + std::f32::consts::PI)
                    % (2.0 * std::f32::consts::PI))
                    - std::f32::consts::PI;
                local_player.rotation_y += rot_diff_normalized * correction_factor;

                // Also update boost meter from server
                local_player.boost_meter = server_player.boost_meter;

                // Update trailer from server - always sync length and positions
                // The server is authoritative for trailer length and positions
                // The server already calculates correct cart positions, so we just use them directly
                local_player.trailer = server_player.trailer.clone();

                // Update alive status
                local_player.alive = server_player.alive;

                // Put reconciled local player back
                sim.sim.state.players.insert(my_id, local_player);
            }
        }
    }

    sim.last_server_tick = world.tick;
}

// Reconcile test player state with server state - debug builds only
#[cfg(debug_assertions)]
fn reconcile_test_player_state(
    time: Res<Time>,
    mut cache: ResMut<TestPlayerCache>,
    test_client: Res<TestPlayerInfo>,
    mut test_sim: Option<ResMut<TestPlayerSim>>,
) {
    let Some(world) = &cache.state else {
        return;
    };
    let Some(mut sim) = test_sim else {
        return;
    };
    let Some(test_id) = test_client.id else {
        return;
    };

    // Only reconcile when we get a new server tick
    if world.tick <= sim.last_server_tick {
        return;
    }

    // Save test player state before updating from server
    let my_test_player = sim.sim.state.players.get(&test_id).cloned();

    // Update all players and items from server
    sim.sim.state.players = world.players.clone();
    sim.sim.state.items = world.items.clone();

    // Reconcile test player: smoothly correct towards server position
    let server_player_opt = sim.sim.state.players.get(&test_id).cloned();
    if let Some(server_player) = server_player_opt {
        // If player was dead and is now alive, use server state directly (respawn)
        let was_dead = my_test_player.as_ref().map_or(false, |p| !p.alive);
        let is_now_alive = server_player.alive;

        if was_dead && is_now_alive {
            // Player respawned - use server state directly
            sim.sim.state.players.insert(test_id, server_player);
            sim.just_respawned = true; // Flag for instant transform update
        } else {
            sim.just_respawned = false;
            if let Some(mut local_player) = my_test_player {
                // Normal reconciliation - smoothly correct towards server position
                // Use frame-rate independent exponential smoothing
                let server_pos = server_player.position;
                let dt = time.delta_secs();
                let correction_rate = 15.0; // corrections per second
                let correction_factor = 1.0 - (-dt * correction_rate).exp();
                local_player.position.x +=
                    (server_pos.x - local_player.position.x) * correction_factor;
                local_player.position.z +=
                    (server_pos.z - local_player.position.z) * correction_factor;

                // Smoothly correct rotation
                let rot_diff = server_player.rotation_y - local_player.rotation_y;
                let rot_diff_normalized = ((rot_diff + std::f32::consts::PI)
                    % (2.0 * std::f32::consts::PI))
                    - std::f32::consts::PI;
                local_player.rotation_y += rot_diff_normalized * correction_factor;

                // Also update boost meter from server
                local_player.boost_meter = server_player.boost_meter;

                // Update trailer from server - always sync length and positions
                // The server is authoritative for trailer length and positions
                // The server already calculates correct cart positions, so we just use them directly
                local_player.trailer = server_player.trailer.clone();

                // Update alive status
                local_player.alive = server_player.alive;

                sim.sim.state.players.insert(test_id, local_player);
            }
        }
    }

    sim.last_server_tick = world.tick;
}

// Sync world state to visual entities (from local sim, not directly from server)
fn sync_world_state(
    mut commands: Commands,
    client: Res<ClientInfo>,
    test_client: Res<TestPlayerInfo>,
    mut local_sim: Option<ResMut<LocalSim>>,
    mut test_sim: Option<ResMut<TestPlayerSim>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    q_players: Query<(Entity, &ServerPlayer)>,
    q_server_players: Query<(&ServerPlayer, &Transform), Without<ServerTruckTrailer>>,
    q_local_player: Query<Entity, With<LocalPlayer>>,
    q_test_player: Query<Entity, With<TestPlayer>>,
    q_collectibles: Query<(Entity, &ServerCollectible)>,
    q_carts: Query<(Entity, &ServerTruckTrailer)>,
) {
    let Some(mut sim) = local_sim else {
        return;
    };
    let Some(my_id) = client.id else {
        return;
    };

    // Capture the flag and world state before the loop
    let just_respawned_local = sim.just_respawned;
    let world = sim.sim.state.clone(); // Clone to avoid borrow issues

    // Track which entities exist
    let mut existing_players: HashMap<PlayerId, Entity> = HashMap::new();
    for (e, sp) in q_players.iter() {
        existing_players.insert(sp.id, e);
    }

    // Check if local player entity exists
    let local_player_entity = q_local_player.iter().next();
    #[cfg(debug_assertions)]
    let test_player_entity = q_test_player.iter().next();
    #[cfg(not(debug_assertions))]
    let test_player_entity = None;
    #[cfg(debug_assertions)]
    let test_id = test_client.id;
    #[cfg(not(debug_assertions))]
    let test_id: Option<shared::PlayerId> = None;

    let mut existing_collectibles: HashMap<Uuid, Entity> = HashMap::new();
    for (e, sc) in q_collectibles.iter() {
        existing_collectibles.insert(sc.id, e);
    }

    let mut existing_carts: HashMap<(PlayerId, usize), Entity> = HashMap::new();
    for (e, stc) in q_carts.iter() {
        existing_carts.insert((stc.player_id, stc.order), e);
    }

    // Spawn/update players - rectangular hover truck shape (longer front-to-back)
    // Width: 0.8, Height: 0.8, Length: 1.8 (front-to-back)
    let player_mesh = meshes.add(Cuboid::new(0.8, 0.8, 1.8));
    for (player_id, player_state) in world.players.iter() {
        let is_me = *player_id == my_id;
        #[cfg(debug_assertions)]
        let is_test = test_id.is_some() && *player_id == test_id.unwrap();
        #[cfg(not(debug_assertions))]
        let is_test = false;

        // Despawn dead players
        if !player_state.alive {
            if is_me {
                if let Some(entity) = local_player_entity {
                    commands.entity(entity).despawn();
                }
            } else if is_test {
                if let Some(entity) = test_player_entity {
                    commands.entity(entity).despawn();
                }
            } else {
                if let Some(entity) = existing_players.remove(player_id) {
                    commands.entity(entity).despawn();
                }
            }
            continue;
        }

        // Determine base color
        let base_color = if is_me {
            Color::srgb(0.2, 0.8, 0.95) // Blue for main player
        } else if is_test {
            Color::srgb(0.4, 0.95, 0.3) // Green for test player
        } else {
            Color::srgb(0.95, 0.4, 0.3) // Red for other players
        };

        let player_mat = materials.add(base_color);

        let pos = shared_to_bevy_vec3(player_state.position);
        let rot = Quat::from_rotation_y(player_state.rotation_y);

        if *player_id == my_id {
            // Local player - spawn if doesn't exist, otherwise it's updated by local_player_move
            if local_player_entity.is_none() {
                commands.spawn((
                    Mesh3d(player_mesh.clone()),
                    MeshMaterial3d(player_mat),
                    Transform::from_translation(pos).with_rotation(rot),
                    GlobalTransform::default(),
                    Visibility::default(),
                    InheritedVisibility::default(),
                    LocalPlayer { id: *player_id },
                    SceneTag,
                ));
            } else if just_respawned_local {
                // Player just respawned - update transform instantly (no interpolation)
                if let Some(entity) = local_player_entity {
                    commands
                        .entity(entity)
                        .insert(Transform::from_translation(pos).with_rotation(rot));
                }
                // Reset flag after using it
                sim.just_respawned = false;
            }
        } else if is_test {
            // Test player - spawn if doesn't exist, otherwise it's updated by test_player_move (debug builds only)
            #[cfg(debug_assertions)]
            {
                if test_player_entity.is_none() {
                    commands.spawn((
                        Mesh3d(player_mesh.clone()),
                        MeshMaterial3d(player_mat),
                        Transform::from_translation(pos).with_rotation(rot),
                        GlobalTransform::default(),
                        Visibility::default(),
                        InheritedVisibility::default(),
                        TestPlayer { id: *player_id },
                        SceneTag,
                    ));
                } else if let Some(mut test_sim_ref) = test_sim.as_mut() {
                    if test_sim_ref.just_respawned {
                        // Test player just respawned - update transform instantly (no interpolation)
                        if let Some(entity) = test_player_entity {
                            commands
                                .entity(entity)
                                .insert(Transform::from_translation(pos).with_rotation(rot));
                        }
                        // Reset flag after using it
                        test_sim_ref.just_respawned = false;
                    }
                }
            }
        } else {
            // Other players - update from server state with interpolation
            if let Some(entity) = existing_players.remove(player_id) {
                // Update interpolation target (don't teleport, let interpolation system handle it)
                if let Ok((_, current_transform)) = q_server_players.get(entity) {
                    let current_pos = current_transform.translation;
                    let current_rot = current_transform.rotation;
                    commands.entity(entity).insert(ServerPlayerInterpolation {
                        prev_pos: current_pos,
                        prev_rot: current_rot,
                        target_pos: pos,
                        target_rot: rot,
                        time_since_update: 0.0,
                    });
                } else {
                    // Fallback: direct update if no transform found
                    commands
                        .entity(entity)
                        .insert(Transform::from_translation(pos).with_rotation(rot));
                }
            } else {
                // Spawn new player (respawned)
                commands.spawn((
                    Mesh3d(player_mesh.clone()),
                    MeshMaterial3d(player_mat),
                    Transform::from_translation(pos).with_rotation(rot),
                    GlobalTransform::default(),
                    Visibility::default(),
                    InheritedVisibility::default(),
                    ServerPlayer { id: *player_id },
                    ServerPlayerInterpolation {
                        prev_pos: pos,
                        prev_rot: rot,
                        target_pos: pos,
                        target_rot: rot,
                        time_since_update: 0.0,
                    },
                    SceneTag,
                ));
            }
        }
    }

    // Despawn players that no longer exist (but not local player or test player)
    for (player_id, entity) in existing_players {
        if player_id != my_id && !test_id.map_or(false, |tid| player_id == tid) {
            commands.entity(entity).despawn();
        }
    }

    // Spawn/update collectibles
    let collectible_mesh = meshes.add(Cuboid::new(0.6, 0.6, 0.6));
    let collectible_mat = materials.add(Color::srgb(0.95, 0.85, 0.2));
    for (item_id, item) in world.items.iter() {
        let pos = shared_to_bevy_vec3(item.pos);

        if let Some(entity) = existing_collectibles.remove(item_id) {
            // Update existing collectible (no interpolation for items, they just teleport)
            commands
                .entity(entity)
                .insert(Transform::from_translation(Vec3::new(pos.x, 0.3, pos.z)));
        } else {
            // Spawn new collectible
            commands.spawn((
                Mesh3d(collectible_mesh.clone()),
                MeshMaterial3d(collectible_mat.clone()),
                Transform::from_translation(Vec3::new(pos.x, 0.3, pos.z)),
                GlobalTransform::default(),
                Visibility::default(),
                InheritedVisibility::default(),
                ServerCollectible { id: *item_id },
                SceneTag,
            ));
        }
    }

    // Despawn collectibles that no longer exist
    for (_, entity) in existing_collectibles {
        commands.entity(entity).despawn();
    }

    // Spawn/update truck trailers (only spawn/despawn, positions updated by update_truck_trailers)
    // Cart shape: rectangular like player but slightly smaller (Width: 0.7, Height: 0.7, Length: 1.4)
    let cart_mesh = meshes.add(Cuboid::new(0.7, 0.7, 1.4));

    // Helper function to generate a random color from cart ID (deterministic)
    let cart_color = |player_id: &Uuid, order: usize| -> Color {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        player_id.hash(&mut hasher);
        order.hash(&mut hasher);
        let hash = hasher.finish();

        // Generate RGB values from hash (ensure bright, saturated colors)
        let r = ((hash & 0xFF) as f32 / 255.0) * 0.7 + 0.3; // 0.3-1.0
        let g = (((hash >> 8) & 0xFF) as f32 / 255.0) * 0.7 + 0.3; // 0.3-1.0
        let b = (((hash >> 16) & 0xFF) as f32 / 255.0) * 0.7 + 0.3; // 0.3-1.0
        Color::srgb(r, g, b)
    };

    // Build a set of expected cart keys from the world state
    let mut expected_cart_keys: std::collections::HashSet<(PlayerId, usize)> =
        std::collections::HashSet::new();
    for (player_id, player_state) in world.players.iter() {
        if !player_state.alive {
            continue;
        }
        for (order, _) in player_state.trailer.iter().enumerate().skip(1) {
            expected_cart_keys.insert((*player_id, order));
        }
    }

    // Spawn new carts and mark existing ones
    for (player_id, player_state) in world.players.iter() {
        if !player_state.alive {
            continue;
        }

        for (order, cart_pos) in player_state.trailer.iter().enumerate().skip(1) {
            let key = (*player_id, order);

            if existing_carts.remove(&key).is_none() {
                // Generate unique color for this cart
                let color = cart_color(player_id, order);
                let cart_mat = materials.add(color);

                // Spawn new trailer at the correct position from server state
                // This prevents the cart from jumping when first spawned
                let spawn_pos = shared_to_bevy_vec3(*cart_pos);
                commands.spawn((
                    Mesh3d(cart_mesh.clone()),
                    MeshMaterial3d(cart_mat),
                    Transform::from_translation(spawn_pos),
                    GlobalTransform::default(),
                    Visibility::default(),
                    InheritedVisibility::default(),
                    ServerTruckTrailer {
                        player_id: *player_id,
                        order,
                    },
                    SceneTag,
                ));
            }
        }
    }

    // Despawn carts that no longer exist in the expected set
    // This provides a safety check to ensure we only despawn carts that are truly not expected
    for (key, entity) in existing_carts {
        if !expected_cart_keys.contains(&key) {
            commands.entity(entity).despawn();
        }
    }
}

fn update_follow_cam(
    time: Res<Time>,
    client: Res<ClientInfo>,
    q_local_player: Query<&Transform, (With<LocalPlayer>, Without<Camera>)>,
    mut q_cam: Query<(&FollowCam, &mut Transform), With<Camera>>,
) {
    let Some(_my_id) = client.id else {
        return;
    };

    // Find local player
    let Ok(player_t) = q_local_player.single() else {
        return;
    };
    let Ok((follow, mut cam_t)) = q_cam.single_mut() else {
        return;
    };

    let dt = time.delta_secs();
    let target = player_t.translation;

    // Camera stays behind player relative to its facing
    let cam_offset_world = player_t.rotation * follow.offset;
    let desired_cam_pos = target + cam_offset_world;

    // Look ahead of the player in their facing direction (forward is +Z in Bevy)
    // Calculate forward direction from player rotation
    let forward = player_t.rotation * Vec3::Z;
    let look_ahead_distance = 8.0; // How far ahead to look
    let desired_look_at = target + forward * look_ahead_distance;

    // Smoothly lerp camera position towards desired position
    let smooth_factor = 1.0 - (-dt * 15.0).exp(); // Exponential smoothing, ~15x per second
    cam_t.translation = cam_t.translation.lerp(desired_cam_pos, smooth_factor);

    // Calculate desired rotation using look_at
    let desired_rot = Transform::from_translation(cam_t.translation)
        .looking_at(desired_look_at, Vec3::Y)
        .rotation;

    // Smoothly slerp rotation
    cam_t.rotation = cam_t.rotation.slerp(desired_rot, smooth_factor);
}

#[derive(Resource, Default)]
struct GridSpawned(bool);

fn spawn_grid_once(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    client: Res<ClientInfo>,
    mut spawned: Local<Option<GridSpawned>>,
) {
    if spawned.is_some() {
        return;
    }
    if client.world_size == 0.0 {
        return;
    } // Wait for server to send world_size

    spawn_wire_grid(
        &mut commands,
        &mut meshes,
        &mut materials,
        client.world_size,
    );
    *spawned = Some(GridSpawned(true));
}

fn spawn_wire_grid(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    world_size: f32,
) {
    let base_color = Color::srgb(0.12, 0.16, 0.2);
    let major_color = Color::srgb(0.22, 0.36, 0.5);
    let axis_x_color = Color::srgb(0.85, 0.3, 0.3);
    let axis_z_color = Color::srgb(0.3, 0.85, 0.3);
    let mat_thin = StandardMaterial {
        base_color: base_color,
        emissive: base_color.into(),
        perceptual_roughness: 0.4,
        metallic: 0.0,
        unlit: true,
        ..default()
    };
    let mat_major = StandardMaterial {
        base_color: major_color,
        emissive: major_color.into(),
        perceptual_roughness: 0.4,
        metallic: 0.0,
        unlit: true,
        ..default()
    };
    let mat_axis_x = StandardMaterial {
        base_color: axis_x_color,
        emissive: axis_x_color.into(),
        perceptual_roughness: 0.4,
        metallic: 0.0,
        unlit: true,
        ..default()
    };
    let mat_axis_z = StandardMaterial {
        base_color: axis_z_color,
        emissive: axis_z_color.into(),
        perceptual_roughness: 0.4,
        metallic: 0.0,
        unlit: true,
        ..default()
    };
    let mat_thin = materials.add(mat_thin);
    let mat_major = materials.add(mat_major);
    let mat_axis_x = materials.add(mat_axis_x);
    let mat_axis_z = materials.add(mat_axis_z);

    let half = world_size as i32;
    let thin = 0.02;
    let major = 0.05;
    let length = (half * 2 + 2) as f32;
    let line_x_thin = meshes.add(Cuboid::new(length, thin, thin));
    let line_z_thin = meshes.add(Cuboid::new(thin, thin, length));
    let line_x_major = meshes.add(Cuboid::new(length, major, major));
    let line_z_major = meshes.add(Cuboid::new(major, major, length));

    for i in -half..=half {
        let is_axis = i == 0;
        let is_major = i % 4 == 0;
        let z = i as f32;
        // lines parallel to X at Z = i
        let (mesh_x, mat_x) = if is_axis {
            (line_x_major.clone(), mat_axis_z.clone())
        } else if is_major {
            (line_x_major.clone(), mat_major.clone())
        } else {
            (line_x_thin.clone(), mat_thin.clone())
        };
        commands.spawn((
            Mesh3d(mesh_x),
            MeshMaterial3d(mat_x),
            Transform::from_xyz(0.0, 0.0, z),
            GlobalTransform::default(),
            Visibility::default(),
            InheritedVisibility::default(),
            SceneTag,
        ));
        let x = i as f32;
        // lines parallel to Z at X = i
        let (mesh_z, mat_z) = if is_axis {
            (line_z_major.clone(), mat_axis_x.clone())
        } else if is_major {
            (line_z_major.clone(), mat_major.clone())
        } else {
            (line_z_thin.clone(), mat_thin.clone())
        };
        commands.spawn((
            Mesh3d(mesh_z),
            MeshMaterial3d(mat_z),
            Transform::from_xyz(x, 0.0, 0.0),
            GlobalTransform::default(),
            Visibility::default(),
            InheritedVisibility::default(),
            SceneTag,
        ));
    }

    // Spawn walls at the boundaries
    let wall_height = 3.0;
    let wall_thickness = 0.5;
    let wall_color = Color::srgb(0.3, 0.3, 0.35);
    let wall_mat = materials.add(StandardMaterial {
        base_color: wall_color,
        emissive: wall_color.into(),
        perceptual_roughness: 0.6,
        metallic: 0.1,
        unlit: false,
        ..default()
    });

    // North wall (positive Z)
    let north_wall = meshes.add(Cuboid::new(world_size * 2.0, wall_height, wall_thickness));
    commands.spawn((
        Mesh3d(north_wall.clone()),
        MeshMaterial3d(wall_mat.clone()),
        Transform::from_xyz(0.0, wall_height / 2.0, world_size),
        GlobalTransform::default(),
        Visibility::default(),
        InheritedVisibility::default(),
        SceneTag,
    ));

    // South wall (negative Z)
    let south_wall = meshes.add(Cuboid::new(world_size * 2.0, wall_height, wall_thickness));
    commands.spawn((
        Mesh3d(south_wall.clone()),
        MeshMaterial3d(wall_mat.clone()),
        Transform::from_xyz(0.0, wall_height / 2.0, -world_size),
        GlobalTransform::default(),
        Visibility::default(),
        InheritedVisibility::default(),
        SceneTag,
    ));

    // East wall (positive X)
    let east_wall = meshes.add(Cuboid::new(wall_thickness, wall_height, world_size * 2.0));
    commands.spawn((
        Mesh3d(east_wall.clone()),
        MeshMaterial3d(wall_mat.clone()),
        Transform::from_xyz(world_size, wall_height / 2.0, 0.0),
        GlobalTransform::default(),
        Visibility::default(),
        InheritedVisibility::default(),
        SceneTag,
    ));

    // West wall (negative X)
    let west_wall = meshes.add(Cuboid::new(wall_thickness, wall_height, world_size * 2.0));
    commands.spawn((
        Mesh3d(west_wall.clone()),
        MeshMaterial3d(wall_mat.clone()),
        Transform::from_xyz(-world_size, wall_height / 2.0, 0.0),
        GlobalTransform::default(),
        Visibility::default(),
        InheritedVisibility::default(),
        SceneTag,
    ));
}

fn send_ping(
    time: Res<Time>,
    mut tracker: ResMut<PingTracker>,
    mut chans: ResMut<NetChannels>,
    mut timer: Local<Option<Timer>>,
) {
    if chans.to_server.is_none() {
        return;
    }
    if timer.is_none() {
        *timer = Some(Timer::from_seconds(1.0, TimerMode::Repeating));
    }
    let t = timer.as_mut().unwrap();
    t.tick(time.delta());
    if !t.just_finished() {
        return;
    }
    #[cfg(not(target_arch = "wasm32"))]
    let id = {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_else(|_| tracker.last_id.wrapping_add(1))
    };
    #[cfg(target_arch = "wasm32")]
    let id = { (Date::now() as u64).max(tracker.last_id.wrapping_add(1)) };
    tracker.in_flight.insert(id, time_now());
    tracker.last_id = id;
    if let Some(tx) = &chans.to_server {
        let _ = tx.unbounded_send(serde_json::to_string(&ClientToServer::Ping(id)).unwrap());
    }
}

fn update_hud(
    time: Res<Time>,
    mut fps: ResMut<FpsCounter>,
    tracker: Res<PingTracker>,
    mut q_window: Query<&mut Window, With<bevy::window::PrimaryWindow>>,
) {
    fps.accum_time += time.delta_secs();
    fps.accum_frames += 1;
    if fps.accum_time >= 0.5 {
        fps.fps = fps.accum_frames as f32 / fps.accum_time;
        fps.accum_time = 0.0;
        fps.accum_frames = 0;
    }
    if let Ok(mut window) = q_window.single_mut() {
        window.title = format!(
            "Hover Truck - FPS: {:>3.0}  Ping: {:>3} ms",
            fps.fps,
            if tracker.rtt_ms > 0.0 {
                tracker.rtt_ms.round() as i32
            } else {
                -1
            }
        );
    }
}

#[derive(Component)]
struct LoadingScreen;

#[derive(Component)]
struct LoadingText;

#[derive(Component)]
struct BoostBar;

#[derive(Component)]
struct BoostBarFill;

#[derive(Component)]
struct Minimap;

#[derive(Component)]
struct MinimapPlayerDot {
    player_id: PlayerId,
}

#[derive(Component)]
struct MinimapArrow;

#[derive(Component)]
struct MinimapArrowBody;

#[derive(Component)]
struct MinimapArrowHead;

fn setup_loading_screen(mut commands: Commands, mut loading: ResMut<LoadingState>) {
    // Create loading screen UI
    let loading_entity = commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                flex_direction: FlexDirection::Column,
                ..default()
            },
            BackgroundColor(Color::srgba(0.05, 0.06, 0.09, 1.0)),
            LoadingScreen,
        ))
        .with_children(|parent| {
            // Title
            parent.spawn((
                Text::new("Hover Truck"),
                Node {
                    margin: UiRect::all(Val::Px(20.0)),
                    ..default()
                },
            ));

            // Loading text
            parent.spawn((
                Text::new("Connecting to server..."),
                LoadingText,
                Node {
                    margin: UiRect::all(Val::Px(10.0)),
                    ..default()
                },
            ));
        })
        .id();

    loading.loading_screen_entity = Some(loading_entity);

    // Create boost UI bar (bottom left corner)
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(20.0),
                bottom: Val::Px(20.0),
                width: Val::Px(200.0),
                height: Val::Px(20.0),
                flex_direction: FlexDirection::Column,
                ..default()
            },
            BoostBar,
        ))
        .with_children(|parent| {
            // Background bar
            parent.spawn((
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.2, 0.2, 0.2, 0.8)),
            ));

            // Fill bar (will be updated)
            parent.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    bottom: Val::Px(0.0),
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
                BackgroundColor(Color::srgb(0.2, 0.8, 1.0)),
                BoostBarFill,
            ));
        });

    // Create minimap (top right corner)
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                right: Val::Px(20.0),
                top: Val::Px(20.0),
                width: Val::Px(200.0),
                height: Val::Px(200.0),
                ..default()
            },
            BackgroundColor(Color::srgba(0.1, 0.1, 0.15, 0.9)),
            Minimap,
        ))
        .with_children(|parent| {
            // Border
            parent.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    right: Val::Px(0.0),
                    top: Val::Px(0.0),
                    bottom: Val::Px(0.0),
                    border: UiRect::all(Val::Px(2.0)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.3, 0.3, 0.4, 1.0)),
            ));
        });
}

fn update_loading_screen(
    time: Res<Time>,
    mut commands: Commands,
    mut loading: ResMut<LoadingState>,
    mut q_loading: Query<Entity, With<LoadingScreen>>,
    mut q_text: Query<&mut Text, With<LoadingText>>,
) {
    // Tick the minimum display timer if it exists
    if let Some(timer) = &mut loading.min_display_timer {
        timer.tick(time.delta());
    }

    if loading.is_ready() {
        // Hide loading screen by despawn (despawn automatically handles children)
        if let Ok(entity) = q_loading.single() {
            commands.entity(entity).despawn();
        }
        return;
    }

    // Update loading text based on state
    if let Ok(mut text) = q_text.single_mut() {
        let status = if !loading.welcome_received {
            "Connecting to server..."
        } else if !loading.first_state_received {
            "Syncing with server..."
        } else {
            "Loading..."
        };
        *text = Text::new(status);
    }
}

// Update player visuals when boosting
fn update_player_boost_visuals(
    keys: Res<ButtonInput<KeyCode>>,
    client: Res<ClientInfo>,
    local_sim: Option<Res<LocalSim>>,
    mut q_local_player: Query<(&LocalPlayer, &MeshMaterial3d<StandardMaterial>)>,
    mut q_test_player: Query<(&TestPlayer, &MeshMaterial3d<StandardMaterial>)>,
    mut q_server_players: Query<(&ServerPlayer, &MeshMaterial3d<StandardMaterial>)>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    world: Res<WorldCache>,
) {
    let Some(sim) = local_sim else {
        return;
    };

    // Update local player
    if let Ok((local_player, material_handle)) = q_local_player.single() {
        if let Some(player_state) = sim.sim.state.players.get(&local_player.id) {
            if let Some(material) = materials.get_mut(&material_handle.0) {
                let boost_pressed = keys.pressed(KeyCode::KeyW);
                let boost_active = boost_pressed && player_state.boost_meter > 0.0;

                let base_color = Color::srgb(0.2, 0.8, 0.95); // Blue for main player
                let color = if boost_active {
                    // Bright yellow-orange when boosting
                    let srgba = base_color.to_srgba();
                    Color::srgb(
                        (srgba.red * 0.5 + 0.5).min(1.0),
                        (srgba.green * 0.5 + 0.5).min(1.0),
                        srgba.blue * 0.3,
                    )
                } else {
                    base_color
                };
                material.base_color = color;
            }
        }
    }

    // Update test player
    if let Ok((test_player, material_handle)) = q_test_player.single() {
        if let Some(player_state) = sim.sim.state.players.get(&test_player.id) {
            if let Some(material) = materials.get_mut(&material_handle.0) {
                let boost_pressed = keys.pressed(KeyCode::KeyW);
                let boost_active = boost_pressed && player_state.boost_meter > 0.0;

                let base_color = Color::srgb(0.4, 0.95, 0.3); // Green for test player
                let color = if boost_active {
                    // Bright yellow-orange when boosting
                    let srgba = base_color.to_srgba();
                    Color::srgb(
                        (srgba.red * 0.5 + 0.5).min(1.0),
                        (srgba.green * 0.5 + 0.5).min(1.0),
                        srgba.blue * 0.3,
                    )
                } else {
                    base_color
                };
                material.base_color = color;
            }
        }
    }

    // Update server players (use world state for boost info)
    if let Some(world_state) = &world.state {
        for (server_player, material_handle) in q_server_players.iter() {
            if world_state.players.get(&server_player.id).is_some() {
                if let Some(material) = materials.get_mut(&material_handle.0) {
                    // For server players, we can't check keys, so we estimate boost active
                    // by checking if meter is depleting (less than 1.0 and decreasing)
                    // This is approximate but should work reasonably well
                    let base_color = Color::srgb(0.95, 0.4, 0.3); // Red for other players
                                                                  // We'll update this more accurately if we track previous meter values
                                                                  // For now, just use base color for server players
                    material.base_color = base_color;
                }
            }
        }
    }
}

// Update boost UI bar
fn update_boost_ui(
    client: Res<ClientInfo>,
    local_sim: Option<Res<LocalSim>>,
    mut q_boost_fill: Query<&mut Node, With<BoostBarFill>>,
) {
    let Some(sim) = local_sim else {
        return;
    };
    let Some(my_id) = client.id else {
        return;
    };

    if let Some(player_state) = sim.sim.state.players.get(&my_id) {
        let boost_meter = player_state.boost_meter.clamp(0.0, 1.0);

        if let Ok(mut node) = q_boost_fill.single_mut() {
            node.width = Val::Percent(boost_meter * 100.0);
        }
    }
}

// Update minimap with player positions
fn update_minimap(
    mut commands: Commands,
    client: Res<ClientInfo>,
    q_minimap: Query<Entity, With<Minimap>>,
    q_local_player: Query<(&LocalPlayer, &Transform)>,
    q_test_player: Query<(&TestPlayer, &Transform)>,
    q_server_players: Query<(&ServerPlayer, &Transform), Without<ServerTruckTrailer>>,
    q_existing_dots: Query<(Entity, &MinimapPlayerDot)>,
    q_existing_arrow: Query<Entity, With<MinimapArrow>>,
    q_arrow_body: Query<Entity, With<MinimapArrowBody>>,
    q_arrow_head: Query<Entity, With<MinimapArrowHead>>,
) {
    let Some(minimap_entity) = q_minimap.iter().next() else {
        return;
    };
    let Some(my_id) = client.id else {
        return;
    };
    let world_size = client.world_size;
    if world_size == 0.0 {
        return;
    }

    let minimap_size = 200.0; // Size of minimap in pixels
    let dot_size = 6.0;
    let arrow_size = 8.0;

    // Collect all players with their positions and IDs
    let mut players: Vec<(PlayerId, Vec3, Quat, bool)> = Vec::new();

    // Add local player
    if let Ok((local_player, transform)) = q_local_player.single() {
        players.push((
            local_player.id,
            transform.translation,
            transform.rotation,
            local_player.id == my_id,
        ));
    }

    // Add test player
    if let Ok((test_player, transform)) = q_test_player.single() {
        players.push((
            test_player.id,
            transform.translation,
            transform.rotation,
            test_player.id == my_id,
        ));
    }

    // Add server players
    for (server_player, transform) in q_server_players.iter() {
        players.push((
            server_player.id,
            transform.translation,
            transform.rotation,
            server_player.id == my_id,
        ));
    }

    // Track which dots exist
    let mut existing_dots: HashMap<PlayerId, Entity> = HashMap::new();
    for (entity, dot) in q_existing_dots.iter() {
        existing_dots.insert(dot.player_id, entity);
    }

    // Update or create dots for each player
    for (player_id, pos, rot, is_me) in players.iter() {
        // Convert world position to minimap coordinates
        // World: -world_size to +world_size
        // Minimap: 0 to minimap_size
        let normalized_x = (pos.x + world_size) / (2.0 * world_size);
        let normalized_z = (pos.z + world_size) / (2.0 * world_size);
        let minimap_x = normalized_x * minimap_size;
        let minimap_y = (1.0 - normalized_z) * minimap_size; // Flip Z (world Z+ is forward, minimap Y+ is down)

        if *is_me {
            // Current player: show arrow
            let rotation_y = rot.to_euler(EulerRot::YXZ).0;
            let arrow_entity = q_existing_arrow.iter().next();

            // Calculate arrow direction in minimap space (rotation_y is around Y axis, we need Z axis rotation for 2D minimap)
            // In world space: rotation_y rotates around Y (up), so forward is in XZ plane
            // In minimap: we're looking down at XZ plane, so rotation_y directly maps to rotation in minimap
            let arrow_dir_x = rotation_y.sin();
            let arrow_dir_y = -rotation_y.cos(); // Negative because minimap Y increases downward

            if let Some(arrow_ent) = arrow_entity {
                // Update existing arrow container position
                commands.entity(arrow_ent).insert(Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(minimap_x - arrow_size / 2.0),
                    top: Val::Px(minimap_y - arrow_size / 2.0),
                    width: Val::Px(arrow_size),
                    height: Val::Px(arrow_size),
                    ..default()
                });

                // Update arrow body (small rectangle pointing in direction)
                // The body is a rectangle centered at the arrow position, extending backward from the tip
                if let Some(body_ent) = q_arrow_body.iter().next() {
                    let body_length = arrow_size * 0.5;
                    let body_width = arrow_size * 0.25;
                    // Position body backward from center along the direction
                    let body_center_x = arrow_size / 2.0 - arrow_dir_x * body_length * 0.3;
                    let body_center_y = arrow_size / 2.0 - arrow_dir_y * body_length * 0.3;

                    commands.entity(body_ent).insert(Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(body_center_x - body_width / 2.0),
                        top: Val::Px(body_center_y - body_length / 2.0),
                        width: Val::Px(body_width),
                        height: Val::Px(body_length),
                        ..default()
                    });
                }

                // Update arrow head (square at the tip pointing in direction)
                if let Some(head_ent) = q_arrow_head.iter().next() {
                    let head_size = arrow_size * 0.35;
                    // Position head at the tip, forward along the direction
                    let head_center_x = arrow_size / 2.0 + arrow_dir_x * arrow_size * 0.25;
                    let head_center_y = arrow_size / 2.0 + arrow_dir_y * arrow_size * 0.25;

                    commands.entity(head_ent).insert(Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(head_center_x - head_size / 2.0),
                        top: Val::Px(head_center_y - head_size / 2.0),
                        width: Val::Px(head_size),
                        height: Val::Px(head_size),
                        ..default()
                    });
                }
            } else {
                // Create new arrow
                let arrow_container = commands
                    .spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: Val::Px(minimap_x - arrow_size / 2.0),
                            top: Val::Px(minimap_y - arrow_size / 2.0),
                            width: Val::Px(arrow_size),
                            height: Val::Px(arrow_size),
                            ..default()
                        },
                        MinimapArrow,
                    ))
                    .id();

                // Arrow body (small rectangle pointing in direction)
                // The body is a rectangle centered at the arrow position, extending backward from the tip
                let body_length = arrow_size * 0.5;
                let body_width = arrow_size * 0.25;
                // Position body backward from center along the direction
                let body_center_x = arrow_size / 2.0 - arrow_dir_x * body_length * 0.3;
                let body_center_y = arrow_size / 2.0 - arrow_dir_y * body_length * 0.3;

                commands.entity(arrow_container).with_children(|parent| {
                    parent.spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: Val::Px(body_center_x - body_width / 2.0),
                            top: Val::Px(body_center_y - body_length / 2.0),
                            width: Val::Px(body_width),
                            height: Val::Px(body_length),
                            ..default()
                        },
                        BackgroundColor(Color::srgb(0.2, 0.8, 0.95)), // Blue for current player
                        MinimapArrowBody,
                    ));

                    // Arrow head (square at the tip pointing in direction)
                    let head_size = arrow_size * 0.35;
                    // Position head at the tip, forward along the direction
                    let head_center_x = arrow_size / 2.0 + arrow_dir_x * arrow_size * 0.25;
                    let head_center_y = arrow_size / 2.0 + arrow_dir_y * arrow_size * 0.25;

                    parent.spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: Val::Px(head_center_x - head_size / 2.0),
                            top: Val::Px(head_center_y - head_size / 2.0),
                            width: Val::Px(head_size),
                            height: Val::Px(head_size),
                            ..default()
                        },
                        BackgroundColor(Color::srgb(0.1, 0.6, 0.9)), // Darker blue for arrow head
                        MinimapArrowHead,
                    ));
                });

                // Make arrow a child of minimap
                commands.entity(minimap_entity).add_child(arrow_container);
            }
        } else {
            // Other players: show dot
            if let Some(dot_entity) = existing_dots.remove(player_id) {
                // Update existing dot
                commands.entity(dot_entity).insert(Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(minimap_x - dot_size / 2.0),
                    top: Val::Px(minimap_y - dot_size / 2.0),
                    width: Val::Px(dot_size),
                    height: Val::Px(dot_size),
                    ..default()
                });
            } else {
                // Create new dot
                commands.entity(minimap_entity).with_children(|parent| {
                    parent.spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: Val::Px(minimap_x - dot_size / 2.0),
                            top: Val::Px(minimap_y - dot_size / 2.0),
                            width: Val::Px(dot_size),
                            height: Val::Px(dot_size),
                            ..default()
                        },
                        BackgroundColor(Color::srgb(0.95, 0.4, 0.3)), // Red for other players
                        MinimapPlayerDot {
                            player_id: *player_id,
                        },
                    ));
                });
            }
        }
    }

    // Remove dots for players that no longer exist
    for (_player_id, entity) in existing_dots.iter() {
        commands.entity(*entity).despawn();
    }
}

// Interpolate server players smoothly between server updates
fn interpolate_server_players(
    time: Res<Time>,
    mut q_server_players: Query<(
        &ServerPlayer,
        &mut Transform,
        &mut ServerPlayerInterpolation,
    )>,
) {
    let dt = time.delta_secs();
    let server_tick_interval = 1.0 / 30.0; // 30 TPS = ~0.033 seconds

    for (_, mut transform, mut interp) in q_server_players.iter_mut() {
        interp.time_since_update += dt;

        // Calculate interpolation factor (0.0 = prev_pos, 1.0 = target_pos)
        // We interpolate over one server tick interval
        let t = (interp.time_since_update / server_tick_interval).min(1.0);

        // Smooth interpolation using exponential smoothing for better feel
        let smooth_t = 1.0 - (-t * 8.0).exp(); // Smooth curve

        // Interpolate position
        transform.translation = interp.prev_pos.lerp(interp.target_pos, smooth_t);

        // Interpolate rotation
        transform.rotation = interp.prev_rot.slerp(interp.target_rot, smooth_t);

        // If we've fully interpolated, update prev to target for next cycle
        if t >= 1.0 {
            interp.prev_pos = interp.target_pos;
            interp.prev_rot = interp.target_rot;
            interp.time_since_update = 0.0;
        }
    }
}

// Update trailer lines connecting the truck to trailers
fn update_trailer_lines(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    local_sim: Option<Res<LocalSim>>,
    q_local_player: Query<(&LocalPlayer, &Transform), Without<ServerTruckTrailer>>,
    q_test_player: Query<(&TestPlayer, &Transform), Without<ServerTruckTrailer>>,
    q_server_players: Query<(&ServerPlayer, &Transform), Without<ServerTruckTrailer>>,
    q_carts: Query<(&ServerTruckTrailer, &Transform)>,
    q_lines: Query<(Entity, &TrailerLine)>,
) {
    let Some(sim) = local_sim else {
        return;
    };

    // Create line mesh and material if not already created
    let line_mesh = meshes.add(Cylinder::new(0.02, 1.0)); // Thin cylinder for line
    let line_mat = materials.add(StandardMaterial {
        base_color: Color::srgb(0.5, 0.5, 0.5),
        unlit: true,
        ..default()
    });

    // Track existing lines
    let mut existing_lines: HashMap<(PlayerId, usize), Entity> = HashMap::new();
    for (entity, line) in q_lines.iter() {
        existing_lines.insert((line.player_id, line.from_order), entity);
    }

    // Build map of player transforms
    let mut player_transforms: HashMap<PlayerId, Transform> = HashMap::new();
    if let Ok((local_player, transform)) = q_local_player.single() {
        player_transforms.insert(local_player.id, *transform);
    }
    if let Ok((test_player, transform)) = q_test_player.single() {
        player_transforms.insert(test_player.id, *transform);
    }
    for (server_player, transform) in q_server_players.iter() {
        player_transforms.insert(server_player.id, *transform);
    }

    // Group carts by player
    let mut carts_by_player: HashMap<PlayerId, Vec<(usize, Transform)>> = HashMap::new();
    for (cart, transform) in q_carts.iter() {
        carts_by_player
            .entry(cart.player_id)
            .or_insert_with(Vec::new)
            .push((cart.order, *transform));
    }

    // Process each player's trailer chain
    for (player_id, player_state) in sim.sim.state.players.iter() {
        if !player_state.alive {
            continue;
        }

        let Some(player_transform) = player_transforms.get(player_id) else {
            continue;
        };

        // Get player's carts sorted by order
        let Some(cart_list) = carts_by_player.get(player_id) else {
            continue;
        };
        let mut sorted_carts: Vec<_> = cart_list.iter().collect();
        sorted_carts.sort_by_key(|(order, _)| *order);

        // Calculate hitch point on player (back of player)
        let player_forward = player_transform.rotation * Vec3::Z;
        let player_back_offset = 0.9;
        let player_hitch_point = player_transform.translation - player_forward * player_back_offset;

        // Line from player to first trailer
        if let Some((1, first_cart_transform)) = sorted_carts.first() {
            let cart_front_offset = 0.7;
            let cart_forward = first_cart_transform.rotation * Vec3::Z;
            let cart_hitch_point =
                first_cart_transform.translation + cart_forward * cart_front_offset;

            let line_key = (*player_id, 0);
            if let Some(line_entity) = existing_lines.remove(&line_key) {
                // Update existing line
                update_line_entity(
                    &mut commands,
                    line_entity,
                    player_hitch_point,
                    cart_hitch_point,
                );
            } else {
                // Spawn new line
                let line_entity = spawn_line_entity(
                    &mut commands,
                    player_hitch_point,
                    cart_hitch_point,
                    &line_mesh,
                    &line_mat,
                );
                commands.entity(line_entity).insert(TrailerLine {
                    player_id: *player_id,
                    from_order: 0,
                });
            }
        }

        // Lines between trailers
        for i in 0..sorted_carts.len().saturating_sub(1) {
            let (order1, transform1) = sorted_carts[i];
            let (_order2, transform2) = sorted_carts[i + 1];

            let cart_back_offset = 0.7;
            let cart_front_offset = 0.7;

            let forward1 = transform1.rotation * Vec3::Z;
            let hitch1 = transform1.translation - forward1 * cart_back_offset;

            let forward2 = transform2.rotation * Vec3::Z;
            let hitch2 = transform2.translation + forward2 * cart_front_offset;

            let line_key = (*player_id, *order1);
            if let Some(line_entity) = existing_lines.remove(&line_key) {
                // Update existing line
                update_line_entity(&mut commands, line_entity, hitch1, hitch2);
            } else {
                // Spawn new line
                let line_entity =
                    spawn_line_entity(&mut commands, hitch1, hitch2, &line_mesh, &line_mat);
                commands.entity(line_entity).insert(TrailerLine {
                    player_id: *player_id,
                    from_order: *order1,
                });
            }
        }
    }

    // Despawn lines that no longer exist
    for (_, entity) in existing_lines {
        commands.entity(entity).despawn();
    }
}

fn spawn_line_entity(
    commands: &mut Commands,
    start: Vec3,
    end: Vec3,
    line_mesh: &Handle<Mesh>,
    line_mat: &Handle<StandardMaterial>,
) -> Entity {
    let midpoint = (start + end) * 0.5;
    let direction = end - start;
    let length = direction.length();

    // Create transform for the line
    let rotation = if length > 0.001 {
        Quat::from_rotation_arc(Vec3::Y, direction.normalize())
    } else {
        Quat::IDENTITY
    };

    commands
        .spawn((
            Mesh3d(line_mesh.clone()),
            MeshMaterial3d(line_mat.clone()),
            Transform::from_translation(midpoint)
                .with_rotation(rotation)
                .with_scale(Vec3::new(1.0, length, 1.0)),
            GlobalTransform::default(),
            Visibility::default(),
            InheritedVisibility::default(),
            SceneTag,
        ))
        .id()
}

fn update_line_entity(commands: &mut Commands, entity: Entity, start: Vec3, end: Vec3) {
    let midpoint = (start + end) * 0.5;
    let direction = end - start;
    let length = direction.length();

    // Update transform for the line
    let rotation = if length > 0.001 {
        Quat::from_rotation_arc(Vec3::Y, direction.normalize())
    } else {
        Quat::IDENTITY
    };

    commands.entity(entity).insert(
        Transform::from_translation(midpoint)
            .with_rotation(rotation)
            .with_scale(Vec3::new(1.0, length, 1.0)),
    );
}
