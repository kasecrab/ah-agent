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
npx wrangler secret put AH_PROVISION_TOKEN
npx wrangler deploy
```

The secret is a password you invent — anything long and random will do — and
it is asked for once, when a pairing is made. Without it the relay pairs with
nobody and says so: `/hub/<id>/provision` is reachable from the URL alone, and
a relay that took any request would let a script own every hub name it could
think of, each one a Durable Object with tables of its own, on your allowance.

Deploying prints a URL like `https://ah-relay.<you>.workers.dev`. Give both to
`ah`, once:

```
ah remote pair --url https://ah-relay.<you>.workers.dev --token <the secret>
```

which prints a QR code to scan with the phone, and the same pairing code as
text in case the camera will not cooperate. `AH_PROVISION_TOKEN` in the
environment does instead of `--token`.

The secret is only for making pairings. It is not part of the key ladder, it
never reaches the phone, and knowing it does not help anybody read a frame.

## Ending a pairing

```
ah remote forget
```

tells the relay to revoke the hub — it drops the log and the device list,
closes both sockets, and marks the name revoked, so the old code opens nothing
afterwards — and then clears the local credential. If the relay cannot be
reached it stops rather than clearing anything, because a pairing forgotten
only on this side is a pairing that still answers whoever has the code.
`--local` forgets it here anyway, and says what that leaves behind.

The mark lives in the hub's own storage and lasts as long as the hub does. A
hub nobody has connected to for thirty days deletes itself whole — log, device
list and revoked mark together — and the name is free again after that, for
whoever holds both the old pairing code, which is the only thing that derives
that name, and the provisioning secret. `ah remote pair` makes a fresh code
and so a different hub, and revokes the one it replaces before it does.

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
