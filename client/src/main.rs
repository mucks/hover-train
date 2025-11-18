use bevy::prelude::*;
use bevy_transform_interpolation::prelude::*;
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::time::{Instant, SystemTime, UNIX_EPOCH};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;
#[cfg(target_arch = "wasm32")]
use js_sys::Date;
use bevy::pbr::prelude::{StandardMaterial, MeshMaterial3d};
use bevy::prelude::Mesh3d;
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::{SinkExt, StreamExt};
use shared::{ClientToServer, ServerToClient, TurnInput, WorldState, Vec3 as SharedVec3, PlayerId, GameSim, GameConfig};
use uuid::Uuid;

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
	
	app
		.insert_resource(ClientInfo { id: None, world_size: 0.0 })
		.insert_resource(WorldCache::default())
		.insert_resource(NetChannels::default())
		.insert_resource(PingTracker::default())
		.insert_resource(FpsCounter::default())
		.insert_resource(LoadingState::default())
		// Test player resources
		.insert_resource(TestPlayerInfo { id: None, world_size: 0.0 })
		.insert_resource(TestPlayerCache::default())
		.insert_resource(TestPlayerChannels::default())
		.insert_resource(ClearColor(Color::srgb(0.05, 0.06, 0.09)))
		.add_systems(Startup, (setup_scene_3d, net_connect, net_connect_test_player, setup_loading_screen))
		.add_systems(Update, (spawn_grid_once, update_loading_screen))
		.add_systems(Update, (
			net_pump,
			net_pump_test_player,
			send_player_input,
			send_test_player_input,
			local_player_move,
			test_player_move,
			update_train_carts,
			reconcile_server_state,
			reconcile_test_player_state,
			update_follow_cam,
			send_ping,
			update_hud,
		))
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
struct ServerTrainCart {
	player_id: PlayerId,
	order: usize,
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
	if chans.to_server.is_some() { return; }
	let (tx_out, mut rx_out) = unbounded::<String>();
	let (tx_in, rx_in) = unbounded::<String>();
	chans.to_server = Some(tx_out.clone());
	chans.from_server = Some(rx_in);

	#[cfg(not(target_arch = "wasm32"))]
	let url = std::env::var("SERVER_WS_URL").unwrap_or_else(|_| "ws://127.0.0.1:4001/ws".to_string());
	#[cfg(target_arch = "wasm32")]
	let url = {
		// In browser, connect directly to the server on port 4001
		// (Trunk proxy doesn't handle WebSocket upgrades well, so connect directly)
		let window = web_sys::window().expect("no global `window` exists");
		let location = window.location();
		// Use ws:// for localhost (server is on port 4001)
		if location.hostname().unwrap_or_default() == "127.0.0.1" || location.hostname().unwrap_or_default() == "localhost" {
			"ws://127.0.0.1:4001/ws".to_string()
		} else {
			// For production, you'd use the actual host
			let protocol = if location.protocol().unwrap_or_default() == "https:" { "wss:" } else { "ws:" };
			format!("{}//{}:4001/ws", protocol, location.hostname().unwrap_or_default())
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

// Test player connection (separate WebSocket)
fn net_connect_test_player(mut chans: ResMut<TestPlayerChannels>) {
	if chans.to_server.is_some() { return; }
	let (tx_out, mut rx_out) = unbounded::<String>();
	let (tx_in, rx_in) = unbounded::<String>();
	chans.to_server = Some(tx_out.clone());
	chans.from_server = Some(rx_in);

	#[cfg(not(target_arch = "wasm32"))]
	let url = std::env::var("SERVER_WS_URL").unwrap_or_else(|_| "ws://127.0.0.1:4001/ws".to_string());
	#[cfg(target_arch = "wasm32")]
	let url = {
		let window = web_sys::window().expect("no global `window` exists");
		let location = window.location();
		if location.hostname().unwrap_or_default() == "127.0.0.1" || location.hostname().unwrap_or_default() == "localhost" {
			"ws://127.0.0.1:4001/ws".to_string()
		} else {
			let protocol = if location.protocol().unwrap_or_default() == "https:" { "wss:" } else { "ws:" };
			format!("{}//{}:4001/ws", protocol, location.hostname().unwrap_or_default())
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
		while let Ok(Some(m)) = rx.try_next() { msgs.push(m); }
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
						};
						// Add local player to sim
						local_sim.sim.state.players.insert(id, shared::PlayerState {
							id,
							position: SharedVec3 { x: 0.0, y: 0.5, z: 0.0 },
							rotation_y: 0.0,
							train: std::collections::VecDeque::new(),
							alive: true,
						});
						commands.insert_resource(local_sim);
					}
					ServerToClient::State(world) => {
						if !loading.first_state_received {
							loading.first_state_received = true;
							loading.state_count = 1;
							// Start timer for minimum display time
							loading.min_display_timer = Some(Timer::from_seconds(1.5, TimerMode::Once));
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

// Test player net pump
fn net_pump_test_player(
	mut commands: Commands,
	mut chans: ResMut<TestPlayerChannels>,
	mut cache: ResMut<TestPlayerCache>,
	mut test_client: ResMut<TestPlayerInfo>,
) {
	if let Some(rx) = chans.from_server.as_mut() {
		let mut msgs = Vec::new();
		while let Ok(Some(m)) = rx.try_next() { msgs.push(m); }
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
						};
						// Add test player to sim
						test_sim.sim.state.players.insert(id, shared::PlayerState {
							id,
							position: SharedVec3 { x: 0.0, y: 0.5, z: 0.0 },
							rotation_y: 0.0,
							train: std::collections::VecDeque::new(),
							alive: true,
						});
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
	if client.id.is_none() { return; }
	if chans.to_server.is_none() { return; }
	let Some(mut sim) = local_sim else { return; };
	
	// Send input at a fixed rate (every 50ms = 20 times per second)
	if timer.is_none() {
		*timer = Some(Timer::from_seconds(0.05, TimerMode::Repeating));
	}
	let t = timer.as_mut().unwrap();
	t.tick(time.delta());
	if !t.just_finished() { return; }
	
	// Determine turn input from keys (A/D only, arrow keys are for test player)
	let turn = if keys.pressed(KeyCode::KeyA) {
		TurnInput::Left
	} else if keys.pressed(KeyCode::KeyD) {
		TurnInput::Right
	} else {
		TurnInput::Straight
	};
	
	// Apply input locally immediately (client-side prediction)
	if let Some(my_id) = client.id {
		sim.sim.submit_input(my_id, turn);
	}
	
	// Send input to server
	if let Some(tx) = &chans.to_server {
		let msg = ClientToServer::Input { turn };
		if let Ok(json) = serde_json::to_string(&msg) {
			let _ = tx.unbounded_send(json);
		}
	}
}

// Send test player input (arrow keys only)
fn send_test_player_input(
	time: Res<Time>,
	keys: Res<ButtonInput<KeyCode>>,
	test_client: Res<TestPlayerInfo>,
	chans: ResMut<TestPlayerChannels>,
	mut test_sim: Option<ResMut<TestPlayerSim>>,
	mut timer: Local<Option<Timer>>,
) {
	if test_client.id.is_none() { return; }
	if chans.to_server.is_none() { return; }
	let Some(mut sim) = test_sim else { return; };
	
	// Send input at a fixed rate (every 50ms = 20 times per second)
	if timer.is_none() {
		*timer = Some(Timer::from_seconds(0.05, TimerMode::Repeating));
	}
	let t = timer.as_mut().unwrap();
	t.tick(time.delta());
	if !t.just_finished() { return; }
	
	// Determine turn input from arrow keys only
	let turn = if keys.pressed(KeyCode::ArrowLeft) {
		TurnInput::Left
	} else if keys.pressed(KeyCode::ArrowRight) {
		TurnInput::Right
	} else {
		TurnInput::Straight
	};
	
	// Apply input locally immediately (client-side prediction)
	if let Some(test_id) = test_client.id {
		sim.sim.submit_input(test_id, turn);
	}
	
	// Send input to server
	if let Some(tx) = &chans.to_server {
		let msg = ClientToServer::Input { turn };
		if let Ok(json) = serde_json::to_string(&msg) {
			let _ = tx.unbounded_send(json);
		}
	}
}

// Move local player every frame (client-side prediction)
fn local_player_move(
	time: Res<Time>,
	keys: Res<ButtonInput<KeyCode>>,
	client: Res<ClientInfo>,
	mut local_sim: Option<ResMut<LocalSim>>,
	mut q_local_player: Query<&mut Transform, (With<LocalPlayer>, Without<Camera>)>,
) {
	let Some(mut sim) = local_sim else { return; };
	let Some(my_id) = client.id else { return; };
	
	let dt = time.delta_secs();
	let world_size = sim.sim.cfg.world_size;
	let turn_speed = sim.sim.cfg.turn_speed;
	let player_speed = sim.sim.cfg.player_speed;
	
	// Get local player from sim
	let Some(player) = sim.sim.state.players.get_mut(&my_id) else { return; };
	if !player.alive { return; }
	
	// Apply turn input every frame based on current key state (smooth turning)
	// A/D only, arrow keys are for test player
	if keys.pressed(KeyCode::KeyA) {
		player.rotation_y += turn_speed * dt;
	} else if keys.pressed(KeyCode::KeyD) {
		player.rotation_y -= turn_speed * dt;
	}
	
	// Apply movement (same logic as server)
	let forward_x = player.rotation_y.sin();
	let forward_z = player.rotation_y.cos();
	player.position.x += forward_x * player_speed * dt;
	player.position.z += forward_z * player_speed * dt;
	
	// Clamp position to world bounds (walls will kill on server, but prevent visual glitches)
	let player_radius = 0.5;
	player.position.x = player.position.x.clamp(-world_size + player_radius, world_size - player_radius);
	player.position.z = player.position.z.clamp(-world_size + player_radius, world_size - player_radius);
	player.position.y = 0.5;
	
	// Update train
	player.train.push_front(player.position);
	if player.train.len() > 1 {
		player.train.pop_back();
	}
	
	// Update visual transform immediately
	if let Ok(mut transform) = q_local_player.single_mut() {
		let pos = shared_to_bevy_vec3(player.position);
		let rot = Quat::from_rotation_y(player.rotation_y);
		transform.translation = pos;
		transform.rotation = rot;
	}
}

// Move test player every frame (client-side prediction)
fn test_player_move(
	time: Res<Time>,
	keys: Res<ButtonInput<KeyCode>>,
	test_client: Res<TestPlayerInfo>,
	mut test_sim: Option<ResMut<TestPlayerSim>>,
	mut q_test_player: Query<&mut Transform, (With<TestPlayer>, Without<Camera>, Without<LocalPlayer>)>,
) {
	let Some(mut sim) = test_sim else { return; };
	let Some(test_id) = test_client.id else { return; };
	
	let dt = time.delta_secs();
	let world_size = sim.sim.cfg.world_size;
	let turn_speed = sim.sim.cfg.turn_speed;
	let player_speed = sim.sim.cfg.player_speed;
	
	// Get test player from sim
	let Some(player) = sim.sim.state.players.get_mut(&test_id) else { return; };
	if !player.alive { return; }
	
	// Apply turn input every frame based on arrow keys (smooth turning)
	if keys.pressed(KeyCode::ArrowLeft) {
		player.rotation_y += turn_speed * dt;
	} else if keys.pressed(KeyCode::ArrowRight) {
		player.rotation_y -= turn_speed * dt;
	}
	
	// Apply movement (same logic as server)
	let forward_x = player.rotation_y.sin();
	let forward_z = player.rotation_y.cos();
	player.position.x += forward_x * player_speed * dt;
	player.position.z += forward_z * player_speed * dt;
	
	// Clamp position to world bounds (walls will kill on server, but prevent visual glitches)
	let player_radius = 0.5;
	player.position.x = player.position.x.clamp(-world_size + player_radius, world_size - player_radius);
	player.position.z = player.position.z.clamp(-world_size + player_radius, world_size - player_radius);
	player.position.y = 0.5;
	
	// Update train
	player.train.push_front(player.position);
	if player.train.len() > 1 {
		player.train.pop_back();
	}
	
	// Update visual transform immediately
	if let Ok(mut transform) = q_test_player.single_mut() {
		let pos = shared_to_bevy_vec3(player.position);
		let rot = Quat::from_rotation_y(player.rotation_y);
		transform.translation = pos;
		transform.rotation = rot;
	}
}

// Update train cart positions every frame - truck trailer physics with dynamic swinging
fn update_train_carts(
	time: Res<Time>,
	client: Res<ClientInfo>,
	test_client: Res<TestPlayerInfo>,
	local_sim: Option<Res<LocalSim>>,
	test_sim: Option<Res<TestPlayerSim>>,
	mut q_carts: Query<(&ServerTrainCart, &mut Transform)>,
	q_local_player: Query<(&LocalPlayer, &Transform), Without<ServerTrainCart>>,
	q_test_player: Query<(&TestPlayer, &Transform), Without<ServerTrainCart>>,
	q_server_players: Query<(&ServerPlayer, &Transform), Without<ServerTrainCart>>,
) {
	// Use main sim for most players, but also check test sim for test player
	let Some(sim) = local_sim else { return; };
	let Some(_my_id) = client.id else { return; };
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
	
	// Get test player transform
	if let Ok((test_player, transform)) = q_test_player.single() {
		player_transforms.insert(test_player.id, *transform);
	}
	
	// Get server player transforms
	for (server_player, transform) in q_server_players.iter() {
		player_transforms.insert(server_player.id, *transform);
	}
	
	// Group carts by player and sort by order
	let mut carts_by_player: std::collections::HashMap<_, Vec<_>> = std::collections::HashMap::new();
	for (cart, transform) in q_carts.iter() {
		carts_by_player.entry(cart.player_id).or_insert_with(Vec::new).push((cart.order, transform.translation, transform.rotation));
	}
	
	// Calculate target positions for all carts (process in order to build chain)
	let mut cart_targets: std::collections::HashMap<(PlayerId, usize), (Vec3, Quat)> = std::collections::HashMap::new();
	
	for (player_id, cart_list) in carts_by_player.iter() {
		// Get player transform (rendered position)
		let Some(player_transform) = player_transforms.get(player_id) else { continue; };
		
		// Check if this player belongs to test player, if so use test sim
		let player_state = if test_id.is_some() && *player_id == test_id.unwrap() {
			test_sim.as_ref().and_then(|ts| ts.sim.state.players.get(player_id))
		} else {
			sim.sim.state.players.get(player_id)
		};
		
		if let Some(player_state) = player_state {
			if !player_state.alive { continue; }
			
			// Sort by order to process sequentially
			let mut sorted_carts: Vec<_> = cart_list.iter().collect();
			sorted_carts.sort_by_key(|(order, _, _)| *order);
			
			// Process carts in order, building the chain with truck trailer physics
			for (order, cart_pos, cart_rot) in sorted_carts {
				let (target_world_pos, target_rot) = if *order == 1 {
					// First trailer: attached to truck (player)
					// Calculate hitch point on the truck (back of player)
					let player_forward = player_transform.rotation * Vec3::Z;
					let hitch_point = player_transform.translation - player_forward * player_back_offset;
					
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
					
					if let Some((prev_cart_target_pos, prev_cart_target_rot)) = cart_targets.get(&prev_cart_key) {
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
	mut cache: ResMut<WorldCache>,
	client: Res<ClientInfo>,
	ping: Res<PingTracker>,
	mut local_sim: Option<ResMut<LocalSim>>,
) {
	let Some(world) = &cache.state else { return; };
	let Some(mut sim) = local_sim else { return; };
	let Some(my_id) = client.id else { return; };
	
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
	if let Some(server_player) = sim.sim.state.players.get(&my_id) {
		// If player was dead and is now alive, use server state directly (respawn)
		let was_dead = my_local_player.as_ref().map_or(false, |p| !p.alive);
		let is_now_alive = server_player.alive;
		
		if was_dead && is_now_alive {
			// Player respawned - use server state directly
			let respawned_player = server_player.clone();
			sim.sim.state.players.insert(my_id, respawned_player);
		} else if let Some(mut local_player) = my_local_player {
			// Normal reconciliation - smoothly correct towards server position
			let server_pos = server_player.position;
			let correction_factor = 0.2; // How much to correct per frame (reduced for smoother correction)
			local_player.position.x += (server_pos.x - local_player.position.x) * correction_factor;
			local_player.position.z += (server_pos.z - local_player.position.z) * correction_factor;
			
			// Smoothly correct rotation (handle angle wrapping)
			let rot_diff = server_player.rotation_y - local_player.rotation_y;
			// Normalize to [-PI, PI]
			let rot_diff_normalized = ((rot_diff + std::f32::consts::PI) % (2.0 * std::f32::consts::PI)) - std::f32::consts::PI;
			local_player.rotation_y += rot_diff_normalized * correction_factor;
			
			// Smoothly update train from server (don't just replace, interpolate)
			// Only update train length and gradually correct positions
			if server_player.train.len() != local_player.train.len() {
				// Length changed, use server train
				local_player.train = server_player.train.clone();
			} else {
				// Same length, smoothly interpolate each position
				for (local_cart, &server_cart) in local_player.train.iter_mut().zip(server_player.train.iter()) {
					local_cart.x += (server_cart.x - local_cart.x) * 0.2;
					local_cart.z += (server_cart.z - local_cart.z) * 0.2;
				}
			}
			
			// Update alive status
			local_player.alive = server_player.alive;
			
			// Put reconciled local player back
			sim.sim.state.players.insert(my_id, local_player);
		}
	}
	
	sim.last_server_tick = world.tick;
}

// Reconcile test player state with server state
fn reconcile_test_player_state(
	mut cache: ResMut<TestPlayerCache>,
	test_client: Res<TestPlayerInfo>,
	mut test_sim: Option<ResMut<TestPlayerSim>>,
) {
	let Some(world) = &cache.state else { return; };
	let Some(mut sim) = test_sim else { return; };
	let Some(test_id) = test_client.id else { return; };
	
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
	if let Some(server_player) = sim.sim.state.players.get(&test_id) {
		// If player was dead and is now alive, use server state directly (respawn)
		let was_dead = my_test_player.as_ref().map_or(false, |p| !p.alive);
		let is_now_alive = server_player.alive;
		
		if was_dead && is_now_alive {
			// Player respawned - use server state directly
			let respawned_player = server_player.clone();
			sim.sim.state.players.insert(test_id, respawned_player);
		} else if let Some(mut local_player) = my_test_player {
			// Normal reconciliation - smoothly correct towards server position
			let server_pos = server_player.position;
			let correction_factor = 0.2;
			local_player.position.x += (server_pos.x - local_player.position.x) * correction_factor;
			local_player.position.z += (server_pos.z - local_player.position.z) * correction_factor;
			
			// Smoothly correct rotation
			let rot_diff = server_player.rotation_y - local_player.rotation_y;
			let rot_diff_normalized = ((rot_diff + std::f32::consts::PI) % (2.0 * std::f32::consts::PI)) - std::f32::consts::PI;
			local_player.rotation_y += rot_diff_normalized * correction_factor;
			
			// Update train
			if server_player.train.len() != local_player.train.len() {
				local_player.train = server_player.train.clone();
			} else {
				for (local_cart, &server_cart) in local_player.train.iter_mut().zip(server_player.train.iter()) {
					local_cart.x += (server_cart.x - local_cart.x) * 0.2;
					local_cart.z += (server_cart.z - local_cart.z) * 0.2;
				}
			}
			
			// Update alive status
			local_player.alive = server_player.alive;
			
			sim.sim.state.players.insert(test_id, local_player);
		}
	}
	
	sim.last_server_tick = world.tick;
}

// Sync world state to visual entities (from local sim, not directly from server)
fn sync_world_state(
	mut commands: Commands,
	client: Res<ClientInfo>,
	test_client: Res<TestPlayerInfo>,
	local_sim: Option<Res<LocalSim>>,
	test_sim: Option<Res<TestPlayerSim>>,
	mut meshes: ResMut<Assets<Mesh>>,
	mut materials: ResMut<Assets<StandardMaterial>>,
	q_players: Query<(Entity, &ServerPlayer)>,
	q_local_player: Query<Entity, With<LocalPlayer>>,
	q_test_player: Query<Entity, With<TestPlayer>>,
	q_collectibles: Query<(Entity, &ServerCollectible)>,
	q_carts: Query<(Entity, &ServerTrainCart)>,
) {
	let Some(sim) = local_sim else { return; };
	let Some(my_id) = client.id else { return; };
	
	let world = &sim.sim.state;
	
	// Track which entities exist
	let mut existing_players: HashMap<PlayerId, Entity> = HashMap::new();
	for (e, sp) in q_players.iter() {
		existing_players.insert(sp.id, e);
	}
	
	// Check if local player entity exists
	let local_player_entity = q_local_player.iter().next();
	let test_player_entity = q_test_player.iter().next();
	let test_id = test_client.id;
	
	let mut existing_collectibles: HashMap<Uuid, Entity> = HashMap::new();
	for (e, sc) in q_collectibles.iter() {
		existing_collectibles.insert(sc.id, e);
	}
	
	let mut existing_carts: HashMap<(PlayerId, usize), Entity> = HashMap::new();
	for (e, stc) in q_carts.iter() {
		existing_carts.insert((stc.player_id, stc.order), e);
	}
	
	// Spawn/update players - rectangular hover train shape (longer front-to-back)
	// Width: 0.8, Height: 0.8, Length: 1.8 (front-to-back)
	let player_mesh = meshes.add(Cuboid::new(0.8, 0.8, 1.8));
	for (player_id, player_state) in &world.players {
		let is_me = *player_id == my_id;
		let is_test = test_id.is_some() && *player_id == test_id.unwrap();
		
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
		
		let color = if is_me {
			Color::srgb(0.2, 0.8, 0.95) // Blue for main player
		} else if is_test {
			Color::srgb(0.4, 0.95, 0.3) // Green for test player
		} else {
			Color::srgb(0.95, 0.4, 0.3) // Red for other players
		};
		let player_mat = materials.add(color);
		
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
			}
		} else if is_test {
			// Test player - spawn if doesn't exist, otherwise it's updated by test_player_move
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
			}
		} else {
			// Other players - update from server state
			if let Some(entity) = existing_players.remove(player_id) {
				// Update existing player transform
				commands.entity(entity).insert(Transform::from_translation(pos).with_rotation(rot));
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
	for (item_id, item) in &world.items {
		let pos = shared_to_bevy_vec3(item.pos);
		
		if let Some(entity) = existing_collectibles.remove(item_id) {
			// Update existing collectible (no interpolation for items, they just teleport)
			commands.entity(entity).insert(Transform::from_translation(Vec3::new(pos.x, 0.3, pos.z)));
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
	
	// Spawn/update train carts (only spawn/despawn, positions updated by update_train_carts)
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
	
	for (player_id, player_state) in &world.players {
		if !player_state.alive { continue; }
		
		for (order, _) in player_state.train.iter().enumerate().skip(1) {
			let key = (*player_id, order);
			
			if existing_carts.remove(&key).is_none() {
				// Generate unique color for this cart
				let color = cart_color(player_id, order);
				let cart_mat = materials.add(color);
				
				// Spawn new cart (position will be updated by update_train_carts)
				commands.spawn((
					Mesh3d(cart_mesh.clone()),
					MeshMaterial3d(cart_mat),
					Transform::from_translation(Vec3::ZERO),
					GlobalTransform::default(),
					Visibility::default(),
					InheritedVisibility::default(),
					ServerTrainCart { player_id: *player_id, order },
					SceneTag,
				));
			}
		}
	}
	
	// Despawn carts that no longer exist
	for (_, entity) in existing_carts {
		commands.entity(entity).despawn();
	}
}

fn update_follow_cam(
	time: Res<Time>,
	client: Res<ClientInfo>,
	q_local_player: Query<&Transform, (With<LocalPlayer>, Without<Camera>)>,
	mut q_cam: Query<(&FollowCam, &mut Transform), With<Camera>>,
) {
	let Some(_my_id) = client.id else { return; };
	
	// Find local player
	let Ok(player_t) = q_local_player.single() else { return; };
	let Ok((follow, mut cam_t)) = q_cam.single_mut() else { return; };
	
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
	if spawned.is_some() { return; }
	if client.world_size == 0.0 { return; } // Wait for server to send world_size
	
	spawn_wire_grid(&mut commands, &mut meshes, &mut materials, client.world_size);
	*spawned = Some(GridSpawned(true));
}

fn spawn_wire_grid(commands: &mut Commands, meshes: &mut Assets<Mesh>, materials: &mut Assets<StandardMaterial>, world_size: f32) {
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
	if chans.to_server.is_none() { return; }
	if timer.is_none() {
		*timer = Some(Timer::from_seconds(1.0, TimerMode::Repeating));
	}
	let t = timer.as_mut().unwrap();
	t.tick(time.delta());
	if !t.just_finished() { return; }
	#[cfg(not(target_arch = "wasm32"))]
	let id = {
		SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or_else(|_| tracker.last_id.wrapping_add(1))
	};
	#[cfg(target_arch = "wasm32")]
	let id = {
		(Date::now() as u64).max(tracker.last_id.wrapping_add(1))
	};
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
			"Hover Train - FPS: {:>3.0}  Ping: {:>3} ms",
			fps.fps,
			if tracker.rtt_ms > 0.0 { tracker.rtt_ms.round() as i32 } else { -1 }
		);
	}
}

#[derive(Component)]
struct LoadingScreen;

#[derive(Component)]
struct LoadingText;

fn setup_loading_screen(
	mut commands: Commands,
	mut loading: ResMut<LoadingState>,
) {
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
				Text::new("Hover Train"),
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
