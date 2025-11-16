use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use uuid::Uuid;

pub type PlayerId = Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Vec3 {
	pub x: f32,
	pub y: f32,
	pub z: f32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TurnInput {
	Left,
	Right,
	Straight,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerState {
	pub id: PlayerId,
	pub position: Vec3,
	pub rotation_y: f32, // yaw angle in radians
	pub train: VecDeque<Vec3>,
	pub alive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
	pub pos: Vec3,
	pub id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldState {
	pub world_size: f32, // half-size of the world
	pub players: HashMap<PlayerId, PlayerState>,
	pub items: HashMap<Uuid, Item>,
	pub tick: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientToServer {
	Hello { name: String },
	Input { turn: TurnInput },
	Ping(u64),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerToClient {
	Welcome { id: PlayerId, world_size: f32 },
	State(WorldState),
	Pong(u64),
	YouDied,
}

#[derive(Debug, Clone)]
pub struct GameConfig {
	pub world_size: f32,
	pub player_speed: f32,
	pub turn_speed: f32,
	pub initial_length: usize,
	pub item_spawn_every_ticks: u64,
}

impl Default for GameConfig {
	fn default() -> Self {
		Self {
			world_size: 64.0,
			player_speed: 6.0,
			turn_speed: 2.5,
			initial_length: 3,
			item_spawn_every_ticks: 20,
		}
	}
}

pub struct GameSim {
	pub cfg: GameConfig,
	pub state: WorldState,
	pub pending_inputs: HashMap<PlayerId, TurnInput>,
}

impl GameSim {
	pub fn new(cfg: GameConfig) -> Self {
		Self {
			state: WorldState {
				world_size: cfg.world_size,
				players: HashMap::new(),
				items: HashMap::new(),
				tick: 0,
			},
			pending_inputs: HashMap::new(),
			cfg,
		}
	}

	pub fn add_player(&mut self) -> PlayerId {
		let id = Uuid::new_v4();
		let mut rng = rand::thread_rng();
		let ws = self.cfg.world_size;
		let position = Vec3 {
			x: rng.gen_range(-ws..ws),
			y: 0.5,
			z: rng.gen_range(-ws..ws),
		};
		let rotation_y = rng.gen_range(0.0..std::f32::consts::TAU);
		let mut train = VecDeque::new();
		train.push_back(position);
		self.state.players.insert(id, PlayerState { id, position, rotation_y, train, alive: true });
		id
	}

	pub fn remove_player(&mut self, id: &PlayerId) {
		self.state.players.remove(id);
		self.pending_inputs.remove(id);
	}

	pub fn submit_input(&mut self, id: PlayerId, input: TurnInput) {
		self.pending_inputs.insert(id, input);
	}

	fn wrap(&self, pos: Vec3) -> Vec3 {
		let ws = self.cfg.world_size;
		let wrap = |v: f32| {
			let mut r = v;
			if r < -ws { r = ws; }
			if r > ws { r = -ws; }
			r
		};
		Vec3 { x: wrap(pos.x), y: pos.y, z: wrap(pos.z) }
	}

	fn spawn_item(&mut self) {
		let mut rng = rand::thread_rng();
		let ws = self.cfg.world_size;
		let pos = Vec3 {
			x: rng.gen_range(-ws..ws),
			y: 0.3,
			z: rng.gen_range(-ws..ws),
		};
		let id = Uuid::new_v4();
		self.state.items.insert(id, Item { pos, id });
	}

	pub fn step(&mut self) {
		self.state.tick += 1;
		let dt = 0.1; // 100ms tick = 0.1 seconds
		let world_size = self.cfg.world_size;
		
		// Apply inputs and move players
		for player in self.state.players.values_mut() {
			if !player.alive { continue; }
			
			// Apply turn input
			if let Some(input) = self.pending_inputs.remove(&player.id) {
				use TurnInput::*;
				match input {
					Left => player.rotation_y += self.cfg.turn_speed * dt,
					Right => player.rotation_y -= self.cfg.turn_speed * dt,
					Straight => {}
				}
			}
			
			// Auto-forward movement
			let forward_x = player.rotation_y.sin();
			let forward_z = player.rotation_y.cos();
			player.position.x += forward_x * self.cfg.player_speed * dt;
			player.position.z += forward_z * self.cfg.player_speed * dt;
			
			// Wrap position
			let wrap = |v: f32| {
				let mut r = v;
				if r < -world_size { r = world_size; }
				if r > world_size { r = -world_size; }
				r
			};
			player.position.x = wrap(player.position.x);
			player.position.z = wrap(player.position.z);
			player.position.y = 0.5; // Maintain hover height
		}
		
		// Check items and update trains
		let mut items_to_remove = Vec::new();
		let mut player_grew: HashMap<PlayerId, bool> = HashMap::new();
		
		for player in self.state.players.values_mut() {
			if !player.alive { continue; }
			
			// Check items
			let mut consumed = false;
			for (iid, item) in &self.state.items {
				let dx = player.position.x - item.pos.x;
				let dz = player.position.z - item.pos.z;
				let dist_sq = dx * dx + dz * dz;
				if dist_sq <= 0.7 * 0.7 {
					items_to_remove.push(*iid);
					consumed = true;
					break;
				}
			}
			
			// Update train
			player.train.push_front(player.position);
			player_grew.insert(player.id, consumed);
		}
		
		// Remove consumed items
		for iid in items_to_remove {
			self.state.items.remove(&iid);
		}
		
		// Pop train tails for players that didn't grow
		for player in self.state.players.values_mut() {
			if !player.alive { continue; }
			if !player_grew.get(&player.id).copied().unwrap_or(false) && player.train.len() > 1 {
				player.train.pop_back();
			}
		}
		
		// Check collisions between players (need to clone train data to avoid borrow conflicts)
		let player_data: Vec<(PlayerId, Vec3, VecDeque<Vec3>)> = self.state.players.iter()
			.filter(|(_, p)| p.alive)
			.map(|(id, p)| (*id, p.position, p.train.clone()))
			.collect();
		
		let mut players_to_kill = Vec::new();
		for (player_id, player_pos, _) in &player_data {
			for (other_id, _, other_train) in &player_data {
				if *player_id == *other_id { continue; }
				for &train_pos in other_train {
					let dx = player_pos.x - train_pos.x;
					let dz = player_pos.z - train_pos.z;
					let dist_sq = dx * dx + dz * dz;
					if dist_sq <= 0.5 * 0.5 {
						players_to_kill.push(*player_id);
						break;
					}
				}
				if players_to_kill.contains(player_id) { break; }
			}
		}
		
		// Kill players that collided
		for player_id in players_to_kill {
			if let Some(player) = self.state.players.get_mut(&player_id) {
				player.alive = false;
			}
		}
		
		// Periodic spawn
		if self.state.tick % self.cfg.item_spawn_every_ticks == 0 {
			self.spawn_item();
		}
	}
}

