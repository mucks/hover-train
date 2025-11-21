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
	pub trailer: VecDeque<Vec3>,
	pub alive: bool,
	pub boost_meter: f32, // Boost meter from 0.0 to 1.0
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
	Input { turn: TurnInput, boost: bool },
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
			world_size: 128.0, // Doubled from 64.0 (full world is now 256x256)
			player_speed: 12.0, // Doubled from 6.0
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
	pub pending_boosts: HashMap<PlayerId, bool>,
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
			pending_boosts: HashMap::new(),
			cfg,
		}
	}

	pub fn add_player(&mut self) -> PlayerId {
		let id = Uuid::new_v4();
		let mut rng = rand::thread_rng();
		let ws = self.cfg.world_size;
		// Keep players away from edges (15 unit buffer to account for trailer length)
		let margin = 15.0;
		let spawn_range = (ws - margin).max(5.0); // Ensure at least 5 units of spawn range
		let position = Vec3 {
			x: rng.gen_range(-spawn_range..spawn_range),
			y: 0.5,
			z: rng.gen_range(-spawn_range..spawn_range),
		};
		let rotation_y = rng.gen_range(0.0..std::f32::consts::TAU);
		
		// Initialize trailer with 2 carts (3 positions total: player + 2 carts)
		// Calculate positions behind the player for the carts
		// Cart spacing: cart length (1.4) + gap (0.8) = 2.2 units between cart centers
		// Player back to first cart: player_back_offset (0.9) + gap (0.8) + cart_front_offset (0.7) = 2.4 units
		let mut trailer = VecDeque::new();
		trailer.push_back(position); // Current position (player)
		
		// Calculate backward direction from rotation
		let backward_x = -rotation_y.sin();
		let backward_z = -rotation_y.cos();
		
		// First cart position: behind player by 2.4 units (player back + gap + cart front)
		let cart1_pos = Vec3 {
			x: position.x + backward_x * 2.4,
			y: 0.5,
			z: position.z + backward_z * 2.4,
		};
		trailer.push_back(cart1_pos);
		
		// Second cart position: behind first cart by 2.2 units (cart back + gap + cart front)
		let cart2_pos = Vec3 {
			x: cart1_pos.x + backward_x * 2.2,
			y: 0.5,
			z: cart1_pos.z + backward_z * 2.2,
		};
		trailer.push_back(cart2_pos);
		
		self.state.players.insert(id, PlayerState { 
			id, 
			position, 
			rotation_y, 
			trailer, 
			alive: true,
			boost_meter: 1.0, // Start with full boost
		});
		id
	}

	pub fn remove_player(&mut self, id: &PlayerId) {
		self.state.players.remove(id);
		self.pending_inputs.remove(id);
	}

	pub fn respawn_player(&mut self, id: &PlayerId) {
		if let Some(player) = self.state.players.get_mut(id) {
			let mut rng = rand::thread_rng();
			let ws = self.cfg.world_size;
			// Keep players away from edges (15 unit buffer to account for trailer length)
			let margin = 15.0;
			let spawn_range = (ws - margin).max(5.0); // Ensure at least 5 units of spawn range
			// Respawn at random position
			player.position = Vec3 {
				x: rng.gen_range(-spawn_range..spawn_range),
				y: 0.5,
				z: rng.gen_range(-spawn_range..spawn_range),
			};
			player.rotation_y = rng.gen_range(0.0..std::f32::consts::TAU);
			// Reset trailer to just the player position (no cubes)
			player.trailer.clear();
			player.trailer.push_back(player.position);
			player.alive = true;
			// Reset boost state
			player.boost_meter = 1.0; // Reset to full boost
			// Clear any pending inputs
			self.pending_inputs.remove(id);
			self.pending_boosts.remove(id);
		}
	}

	pub fn submit_input(&mut self, id: PlayerId, input: TurnInput) {
		self.pending_inputs.insert(id, input);
	}

	pub fn submit_boost(&mut self, id: PlayerId, boost: bool) {
		self.pending_boosts.insert(id, boost);
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
		let dt = 1.0 / 30.0; // 33ms tick ≈ 0.033 seconds (30 TPS)
		let world_size = self.cfg.world_size;
		
		// Apply inputs and move players
		for player in self.state.players.values_mut() {
			if !player.alive { continue; }
			
			// Handle boost input and update boost meter
			let boost_pressed = self.pending_boosts.remove(&player.id).unwrap_or(false);
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
			
			// Apply turn input
			if let Some(input) = self.pending_inputs.remove(&player.id) {
				use TurnInput::*;
				match input {
					Left => player.rotation_y += self.cfg.turn_speed * dt,
					Right => player.rotation_y -= self.cfg.turn_speed * dt,
					Straight => {}
				}
			}
			
			// Auto-forward movement with boost multiplier
			let boost_active = boost_pressed && player.boost_meter > 0.0;
			let speed_multiplier = if boost_active { 2.0 } else { 1.0 };
			let forward_x = player.rotation_y.sin();
			let forward_z = player.rotation_y.cos();
			player.position.x += forward_x * self.cfg.player_speed * speed_multiplier * dt;
			player.position.z += forward_z * self.cfg.player_speed * speed_multiplier * dt;
			
			// Check wall collisions - kill player if they hit the boundary
			// Player radius is approximately 0.5 (half of 1.0 cube size, but we use 0.9 for truck shape)
			let player_radius = 0.5;
			if player.position.x <= -world_size + player_radius || 
			   player.position.x >= world_size - player_radius ||
			   player.position.z <= -world_size + player_radius ||
			   player.position.z >= world_size - player_radius {
				player.alive = false;
			} else {
				// Clamp position to keep player within bounds (prevent going slightly past wall)
				player.position.x = player.position.x.clamp(-world_size + player_radius, world_size - player_radius);
				player.position.z = player.position.z.clamp(-world_size + player_radius, world_size - player_radius);
			}
			player.position.y = 0.5; // Maintain hover height
		}
		
		// Check items and update trailers
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
			
			// Determine target trailer length
			// If player grew, add one cart. Otherwise, keep the current length (trailers don't shrink automatically)
			// Trailers only shrink when the player dies/respawns
			let current_length = player.trailer.len();
			let min_length = self.cfg.initial_length; // Minimum trailer length (player + initial carts)
			let target_length = if consumed {
				current_length + 1 // Add a cart when growing
			} else {
				// Keep current length - trailers don't shrink automatically
				// They only shrink when player dies and respawns
				current_length.max(min_length) // Ensure at least minimum length
			};
			
			// Update trailer - store actual cart positions, not just historical player positions
			// Calculate current cart positions based on physics
			let gap = 0.8;
			let player_back_offset = 0.9;
			let cart_front_offset = 0.7;
			let cart_back_offset = 0.7;
			let hitch_length = gap + cart_front_offset;
			
			let player_forward = Vec3 {
				x: player.rotation_y.sin(),
				y: 0.0,
				z: player.rotation_y.cos(),
			};
			
			// Calculate new cart positions based on current player state
			let mut new_trailer = VecDeque::new();
			new_trailer.push_back(player.position); // First element is always player position
			
			// Calculate positions for existing carts (target_length - 1 because we already added player position)
			let num_carts = target_length - 1;
			if num_carts > 0 {
				// Get previous cart positions for direction calculation
				let mut prev_cart_pos: Option<Vec3> = None;
				let mut prev_cart_forward: Option<Vec3> = None;
				
				// Use previous trailer positions to get direction, but recalculate actual positions
				let mut old_trailer_iter = player.trailer.iter().skip(1);
				
				for order in 0..num_carts {
					let old_pos = old_trailer_iter.next();
					
					let (cart_pos, cart_forward) = if order == 0 {
						// First cart: attached to player
						let hitch_point = Vec3 {
							x: player.position.x - player_forward.x * player_back_offset,
							y: 0.5,
							z: player.position.z - player_forward.z * player_back_offset,
						};
						
						// Use old position to determine direction if available
						if let Some(&old_cart_pos) = old_pos {
							let to_hitch = Vec3 {
								x: hitch_point.x - old_cart_pos.x,
								y: 0.0,
								z: hitch_point.z - old_cart_pos.z,
							};
							let to_hitch_dist = (to_hitch.x * to_hitch.x + to_hitch.z * to_hitch.z).sqrt();
							
							if to_hitch_dist > 0.001 {
								let to_hitch_dir = Vec3 {
									x: to_hitch.x / to_hitch_dist,
									y: 0.0,
									z: to_hitch.z / to_hitch_dist,
								};
								let cart_pos = Vec3 {
									x: hitch_point.x - to_hitch_dir.x * hitch_length,
									y: 0.5,
									z: hitch_point.z - to_hitch_dir.z * hitch_length,
								};
								(cart_pos, to_hitch_dir)
							} else {
								let backward = Vec3 { x: -player_forward.x, y: 0.0, z: -player_forward.z };
								let cart_pos = Vec3 {
									x: hitch_point.x + backward.x * hitch_length,
									y: 0.5,
									z: hitch_point.z + backward.z * hitch_length,
								};
								(cart_pos, player_forward)
							}
						} else {
							let backward = Vec3 { x: -player_forward.x, y: 0.0, z: -player_forward.z };
							let cart_pos = Vec3 {
								x: hitch_point.x + backward.x * hitch_length,
								y: 0.5,
								z: hitch_point.z + backward.z * hitch_length,
							};
							(cart_pos, player_forward)
						}
					} else {
						// Subsequent carts
						if let (Some(prev_pos), Some(prev_fwd)) = (prev_cart_pos, prev_cart_forward) {
							let hitch_point = Vec3 {
								x: prev_pos.x - prev_fwd.x * cart_back_offset,
								y: 0.5,
								z: prev_pos.z - prev_fwd.z * cart_back_offset,
							};
							
							if let Some(&old_cart_pos) = old_pos {
								let to_hitch = Vec3 {
									x: hitch_point.x - old_cart_pos.x,
									y: 0.0,
									z: hitch_point.z - old_cart_pos.z,
								};
								let to_hitch_dist = (to_hitch.x * to_hitch.x + to_hitch.z * to_hitch.z).sqrt();
								
								if to_hitch_dist > 0.001 {
									let to_hitch_dir = Vec3 {
										x: to_hitch.x / to_hitch_dist,
										y: 0.0,
										z: to_hitch.z / to_hitch_dist,
									};
									let cart_pos = Vec3 {
										x: hitch_point.x - to_hitch_dir.x * hitch_length,
										y: 0.5,
										z: hitch_point.z - to_hitch_dir.z * hitch_length,
									};
									(cart_pos, to_hitch_dir)
								} else {
									let backward = Vec3 { x: -prev_fwd.x, y: 0.0, z: -prev_fwd.z };
									let cart_pos = Vec3 {
										x: hitch_point.x + backward.x * hitch_length,
										y: 0.5,
										z: hitch_point.z + backward.z * hitch_length,
									};
									(cart_pos, prev_fwd)
								}
							} else {
								let backward = Vec3 { x: -prev_fwd.x, y: 0.0, z: -prev_fwd.z };
								let cart_pos = Vec3 {
									x: hitch_point.x + backward.x * hitch_length,
									y: 0.5,
									z: hitch_point.z + backward.z * hitch_length,
								};
								(cart_pos, prev_fwd)
							}
						} else {
							// Fallback
							let backward = Vec3 { x: -player_forward.x, y: 0.0, z: -player_forward.z };
							let cart_pos = Vec3 {
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
			
			player_grew.insert(player.id, consumed);
		}
		
		// Remove consumed items
		for iid in items_to_remove {
			self.state.items.remove(&iid);
		}
		
		// Check collisions between players and trailers
		// The trailer VecDeque now stores actual cart positions (calculated above)
		// Players die if they collide with another player OR another player's trailer segments
		let player_data: Vec<(PlayerId, Vec3, VecDeque<Vec3>)> = self.state.players.iter()
			.filter(|(_, p)| p.alive)
			.map(|(id, p)| (*id, p.position, p.trailer.clone()))
			.collect();
		
		let mut players_to_kill = Vec::new();
		for (player_id, player_pos, _) in &player_data {
			for (other_id, other_pos, other_trailer) in &player_data {
				if *player_id == *other_id { continue; }
				
				// Check collision with other player directly (player-to-player collision)
				let dx = player_pos.x - other_pos.x;
				let dz = player_pos.z - other_pos.z;
				let dist_sq = dx * dx + dz * dz;
				let player_collision_dist = 0.5 + 0.5; // Both players have radius 0.5
				if dist_sq <= player_collision_dist * player_collision_dist {
					players_to_kill.push(*player_id);
					continue;
				}
				
				// Check collision with other player's trailer cart positions
				// Skip the first element (index 0) as that's the player's own position
				for (order, &cart_pos) in other_trailer.iter().enumerate() {
					if order == 0 { continue; } // Skip player's own position
					
					let dx = player_pos.x - cart_pos.x;
					let dz = player_pos.z - cart_pos.z;
					let dist_sq = dx * dx + dz * dz;
					// Player radius (0.5) + trailer cart radius (0.35, cart is 0.7 wide)
					let trailer_collision_dist = 0.5 + 0.35;
					if dist_sq <= trailer_collision_dist * trailer_collision_dist {
						players_to_kill.push(*player_id);
						break; // Only need to detect one collision per other player
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
		
		// Respawn dead players (with a small delay to prevent instant respawn)
		// Respawn after 10 ticks (1 second) of being dead
		let dead_player_ids: Vec<PlayerId> = self.state.players.iter()
			.filter(|(_, p)| !p.alive)
			.map(|(id, _)| *id)
			.collect();
		for player_id in dead_player_ids {
			// Check if player has been dead long enough (simple: respawn immediately for now)
			// In a real game you might want a respawn timer
			self.respawn_player(&player_id);
		}
		
		// Periodic spawn
		if self.state.tick % self.cfg.item_spawn_every_ticks == 0 {
			self.spawn_item();
		}
	}
}

