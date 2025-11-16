## Hover Train - Bevy + Rust WebSocket Multiplayer

Run the server:

```bash
cargo run -p server
```

Run the client (native):

```bash
cargo run -p client
```

Run the client (web via Trunk):

```bash
cargo install trunk
cd client
trunk serve --open
```

Notes:
- Default server URL is `ws://127.0.0.1:8080/ws`. On web, you can pass `?server=ws://host:port/ws`.
- Controls: A/Left = turn left, D/Right = turn right, W/Up = straight.
- Goal: collect items to grow your hover train; cut other players off (collision = death).

