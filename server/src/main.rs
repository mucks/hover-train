use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
	extract::{
		ws::{Message, WebSocket},
		State, WebSocketUpgrade,
	},
	response::IntoResponse,
	routing::get,
	Router,
};
use futures::{sink::SinkExt, stream::StreamExt};
use shared::{ClientToServer, GameConfig, GameSim, ServerToClient};
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{error, info};

#[derive(Clone)]
struct AppState {
	sim: Arc<Mutex<GameSim>>,
	tx_state: broadcast::Sender<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	tracing_subscriber::fmt().with_env_filter("info").init();

	let sim = GameSim::new(GameConfig::default());
	let (tx_state, _rx_state) = broadcast::channel::<String>(64);
	let state = AppState { sim: Arc::new(Mutex::new(sim)), tx_state };

	let app = Router::new()
		.route("/ws", get(ws_handler))
		.with_state(state.clone());

	// Tick loop
	let state_for_tick = state.clone();
	tokio::spawn(async move {
		let mut ticker = tokio::time::interval(Duration::from_millis(100));
		loop {
			ticker.tick().await;
			let mut sim = state_for_tick.sim.lock().await;
			sim.step();
			let world = serde_json::to_string(&ServerToClient::State(sim.state.clone()));
			if let Ok(json) = world {
				let _ = state_for_tick.tx_state.send(json);
			}
		}
	});

	let port = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(4001u16);
	let addr = SocketAddr::from(([0, 0, 0, 0], port));
	info!("server listening on {addr}");
	let listener = tokio::net::TcpListener::bind(addr).await?;
	axum::serve(listener, app).await?;
	Ok(())
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
	ws.on_upgrade(move |socket| client_connection(socket, state))
}

async fn client_connection(mut socket: WebSocket, state: AppState) {
	let (mut sink, mut stream) = socket.split();
	let mut rx_broadcast = state.tx_state.subscribe();
	let (tx_direct, mut rx_direct) = mpsc::channel::<Message>(16);

	// On connect: add player and send welcome
	let player_id = {
		let mut sim = state.sim.lock().await;
		let id = sim.add_player();
		let welcome = ServerToClient::Welcome { id, world_size: sim.cfg.world_size };
		let _ = sink.send(Message::Text(serde_json::to_string(&welcome).unwrap())).await;
		id
	};

	// Writer task: forwards broadcast state and direct messages to client
	let writer_handle = tokio::spawn(async move {
		loop {
			tokio::select! {
				msg = rx_broadcast.recv() => {
					match msg {
						Ok(json) => {
							if sink.send(Message::Text(json)).await.is_err() {
								break;
							}
						}
						Err(_) => break,
					}
				}
				opt = rx_direct.recv() => {
					match opt {
						Some(message) => {
							if sink.send(message).await.is_err() {
								break;
							}
						}
						None => break,
					}
				}
			}
		}
	});

	// Reader: process client messages
	while let Some(Ok(msg)) = stream.next().await {
		match msg {
			Message::Text(txt) => {
				match serde_json::from_str::<ClientToServer>(&txt) {
					Ok(ClientToServer::Input { turn }) => {
						let mut sim = state.sim.lock().await;
						sim.submit_input(player_id, turn);
					}
					Ok(ClientToServer::Ping(n)) => {
						let _ = tx_direct.send(Message::Text(serde_json::to_string(&ServerToClient::Pong(n)).unwrap())).await;
					}
					Ok(ClientToServer::Hello { .. }) => {}
					Err(e) => {
						error!("bad client msg: {e}");
					}
				}
			}
			Message::Close(_) => break,
			Message::Binary(_) => {}
			_ => {}
		}
	}

	// Cleanup
	// Drop direct tx to stop writer, then wait for it to end
	drop(tx_direct);
	let _ = writer_handle.await;
	let mut sim = state.sim.lock().await;
	sim.remove_player(&player_id);
}

