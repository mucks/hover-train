use bevy::prelude::*;
use bevy_transform_interpolation::prelude::*;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::collections::HashMap;
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
struct LocalSim {
	sim: GameSim,
	last_server_tick: u64,
}

fn main() {
	#[cfg(target_arch = "wasm32")]
	{
		console_error_panic_hook::set_once();
		console_log::init_with_level(log::Level::Info).ok();
	}

	App::new()
		.insert_resource(ClientInfo { id: None, world_size: 0.0 })
		.insert_resource(WorldCache::default())
		.insert_resource(NetChannels::default())
		.insert_resource(PingTracker::default())
		.insert_resource(FpsCounter::default())
		.insert_resource(ClearColor(Color::srgb(0.05, 0.06, 0.09)))
		.add_plugins(DefaultPlugins)
		.add_systems(Startup, (setup_scene_3d, net_connect))
		.add_systems(Update, spawn_grid_once)
		.add_systems(Update, (
			net_pump,
			send_player_input,
			local_player_move,
			update_train_carts,
			reconcile_server_state,
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

#[derive(Resource, Default)]
struct PingTracker {
	last_id: u64,
	in_flight: HashMap<u64, Instant>,
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
			offset: Vec3::new(0.0, 8.0, -12.0),
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

	let url = std::env::var("SERVER_WS_URL").unwrap_or_else(|_| "ws://127.0.0.1:4001/ws".to_string());
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
			let ws = WebSocket::new(&url).unwrap();
			ws.set_binary_type(web_sys::BinaryType::Arraybuffer);
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
				let onerror = Closure::<dyn FnMut(_)>::new(move |e: ErrorEvent| {
					log::error!("ws error: {:?}", e.message());
				});
				ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
				onerror.forget();
			}
			// write
			let ws_clone = ws.clone();
			spawn_local(async move {
				while let Some(out) = rx_out.next().await {
					let _ = ws_clone.send_with_str(&out);
				}
			});
		});
	}
}

fn net_pump(mut commands: Commands, mut chans: ResMut<NetChannels>, mut cache: ResMut<WorldCache>, mut client: ResMut<ClientInfo>, mut ping: ResMut<PingTracker>) {
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
						cache.state = Some(world);
					}
					ServerToClient::Pong(id) => {
						if let Some(start) = ping.in_flight.remove(&id) {
							let rtt = start.elapsed();
							ping.rtt_ms = (rtt.as_secs_f64() * 1000.0) as f32;
						}
					}
					ServerToClient::YouDied => {}
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
	
	// Determine turn input from keys
	let turn = if keys.pressed(KeyCode::KeyA) || keys.pressed(KeyCode::ArrowLeft) {
		TurnInput::Left
	} else if keys.pressed(KeyCode::KeyD) || keys.pressed(KeyCode::ArrowRight) {
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
	if keys.pressed(KeyCode::KeyA) || keys.pressed(KeyCode::ArrowLeft) {
		player.rotation_y += turn_speed * dt;
	} else if keys.pressed(KeyCode::KeyD) || keys.pressed(KeyCode::ArrowRight) {
		player.rotation_y -= turn_speed * dt;
	}
	
	// Apply movement (same logic as server)
	let forward_x = player.rotation_y.sin();
	let forward_z = player.rotation_y.cos();
	player.position.x += forward_x * player_speed * dt;
	player.position.z += forward_z * player_speed * dt;
	
	// Wrap position
	let wrap = |v: f32| {
		let mut r = v;
		if r < -world_size { r = world_size; }
		if r > world_size { r = -world_size; }
		r
	};
	player.position.x = wrap(player.position.x);
	player.position.z = wrap(player.position.z);
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

// Update train cart positions every frame from local simulation (smooth movement)
fn update_train_carts(
	time: Res<Time>,
	client: Res<ClientInfo>,
	local_sim: Option<Res<LocalSim>>,
	mut q_carts: Query<(&ServerTrainCart, &mut Transform)>,
) {
	let Some(sim) = local_sim else { return; };
	let Some(_my_id) = client.id else { return; };
	
	let dt = time.delta_secs();
	let smooth_factor = 1.0 - (-dt * 20.0).exp(); // Smooth interpolation factor
	
	// Update all carts from local simulation state
	for (cart, mut transform) in q_carts.iter_mut() {
		if let Some(player_state) = sim.sim.state.players.get(&cart.player_id) {
			if !player_state.alive { continue; }
			
			// Get cart position from train
			if let Some(&cart_pos) = player_state.train.get(cart.order) {
				let target_pos = shared_to_bevy_vec3(cart_pos);
				let target_world_pos = Vec3::new(target_pos.x, 0.4, target_pos.z);
				
				// Calculate rotation from previous position
				let prev_pos = if cart.order > 1 {
					player_state.train.get(cart.order - 1).copied()
				} else {
					Some(player_state.position)
				};
				
				let target_rot = if let Some(prev) = prev_pos {
					let prev_bevy = shared_to_bevy_vec3(prev);
					let dir = (target_pos - prev_bevy).normalize();
					if dir.length_squared() > 0.001 {
						Quat::from_rotation_arc(Vec3::Z, dir)
					} else {
						transform.rotation
					}
				} else {
					transform.rotation
				};
				
				// Smoothly lerp position and rotation towards target
				transform.translation = transform.translation.lerp(target_world_pos, smooth_factor);
				transform.rotation = transform.rotation.slerp(target_rot, smooth_factor);
			}
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
		if let Some(mut local_player) = my_local_player {
			// Smoothly correct position towards server position
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
			
			// Put reconciled local player back
			sim.sim.state.players.insert(my_id, local_player);
		}
	}
	
	sim.last_server_tick = world.tick;
}

// Sync world state to visual entities (from local sim, not directly from server)
fn sync_world_state(
	mut commands: Commands,
	client: Res<ClientInfo>,
	local_sim: Option<Res<LocalSim>>,
	mut meshes: ResMut<Assets<Mesh>>,
	mut materials: ResMut<Assets<StandardMaterial>>,
	q_players: Query<(Entity, &ServerPlayer)>,
	q_local_player: Query<Entity, With<LocalPlayer>>,
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
	
	let mut existing_collectibles: HashMap<Uuid, Entity> = HashMap::new();
	for (e, sc) in q_collectibles.iter() {
		existing_collectibles.insert(sc.id, e);
	}
	
	let mut existing_carts: HashMap<(PlayerId, usize), Entity> = HashMap::new();
	for (e, stc) in q_carts.iter() {
		existing_carts.insert((stc.player_id, stc.order), e);
	}
	
	// Spawn/update players
	let player_mesh = meshes.add(Cuboid::new(1.0, 1.0, 1.0));
	for (player_id, player_state) in &world.players {
		if !player_state.alive { continue; }
		
		let is_me = *player_id == my_id;
		let color = if is_me {
			Color::srgb(0.2, 0.8, 0.95)
		} else {
			Color::srgb(0.95, 0.4, 0.3)
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
		} else {
			// Other players - update from server state
			if let Some(entity) = existing_players.remove(player_id) {
				// Update existing player transform
				commands.entity(entity).insert(Transform::from_translation(pos).with_rotation(rot));
			} else {
				// Spawn new player
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
	
	// Despawn players that no longer exist (but not local player)
	for (player_id, entity) in existing_players {
		if player_id != my_id {
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
	let cart_mesh = meshes.add(Cuboid::new(0.6, 0.6, 0.6));
	let cart_mat = materials.add(Color::srgb(0.9, 0.6, 0.2));
	for (player_id, player_state) in &world.players {
		if !player_state.alive { continue; }
		
		for (order, _) in player_state.train.iter().enumerate().skip(1) {
			let key = (*player_id, order);
			
			if existing_carts.remove(&key).is_none() {
				// Spawn new cart (position will be updated by update_train_carts)
				commands.spawn((
					Mesh3d(cart_mesh.clone()),
					MeshMaterial3d(cart_mat.clone()),
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
	let desired_look_at = target;
	
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
	let id = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or_else(|_| tracker.last_id.wrapping_add(1));
	tracker.in_flight.insert(id, Instant::now());
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
