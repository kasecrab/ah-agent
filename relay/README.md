# ah relay

Where an `ah` session and a phone meet. It runs on your own Cloudflare account,
on the free plan, and it cannot read anything that passes through it: every
payload is sealed with keys derived from a pairing code the relay is never
given. What it can see is how many frames there are, how big they are, and
when — the price of having somewhere to meet at all.

## Deploying it

You need a Cloudflare account (free, no card) and Node for `wrangler`.

```
npx wrangler login
npx wrangler deploy
```

That prints a URL like `https://ah-relay.<you>.workers.dev`. Give it to `ah`
once:

```
ah remote pair --url https://ah-relay.<you>.workers.dev
```

which prints a QR code to scan with the phone, and the same pairing code as
text in case the camera will not cooperate.

There is deliberately no one-click deploy button: the Workers Builds image
ships Node, Python, PHP, Ruby, Go and Bun, but no Rust toolchain, so a button
would have to install one on every deploy and wait several minutes for it.

## Running it locally

```
npx wrangler dev
```

No account needed — `wrangler dev` simulates the Durable Object and its SQLite
storage locally. `python3 relay/mock.py` is the same protocol again in memory,
for when even a wasm toolchain is more than you want; `python3 relay/smoke.py`
drives either of them and checks it turns away everyone it should. Press `e` at the dev server to open the Local Explorer and
read the `log` table while a session is streaming.

## What it costs

Well inside the free plan. A two-minute turn is around 600 frames from the
desktop; incoming WebSocket messages bill at 20:1, so 30 requests. Eight turns
a day is roughly 240 requests against 100,000, about 4,800 log rows against
100,000 rows written, and a fraction of a GB-s against 13,000.

The one line that matters for this is `accept_websocket_with_tags`. An
ordinary `accept` keeps the object resident, and a resident object holding an
idle socket burns about 10,800 GB-s a day — a single idle user would spend the
whole daily allowance on nothing at all.

## Layout

| File | What it does |
|---|---|
| `src/lib.rs` | routing, and nothing else; holds no state |
| `src/hub.rs` | the Durable Object: one per pairing, two sockets, one log |
| `src/auth.rs` | the single HMAC check that decides who may open a socket |
| `src/schema.rs` | the tables, and the limits they are kept inside |

The wire format lives in `../crates/ah-remote-proto`, compiled both natively by
the harness and to wasm32 here, so the two halves cannot disagree about a frame.
