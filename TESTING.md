# Testing uclip_daemon — step-by-step guide

Three layers: automated (headless) → live socket (needs compositor) → robustness.
Work through them in order; each layer assumes the previous is green.

## 0. Prerequisites

- Wayland session on Hyprland (or any compositor exposing `wl_seat` +
  `ext_data_control_manager_v1`), `WAYLAND_DISPLAY` set.
- Tools: `cargo`, `socat`, `python3` (stdlib only), `jq` (optional, pretty-prints).
- Socket path: `$XDG_RUNTIME_DIR/uclip.sock`
  (`echo $XDG_RUNTIME_DIR` → usually `/run/user/1000`).

## 1. Automated tests (no compositor needed)

```sh
cargo test                          # all 50: lib 46 + bin 4
cargo test daemon::server           # 10 IPC harness tests
cargo test daemon::types            # 8 protocol serde tests
cargo test daemon::snapshot         # 5 paging/clamp/eviction tests
cargo test -- --nocapture           # same, with println/tracing output shown
cargo test lagged -- --nocapture    # single-test debug by name substring
```

What they prove: protocol shapes, snapshot paging, per-client dispatch
(List/Ping/Restore-forward/Subscribe), 2-client fan-out, lagged resync,
malformed/oversized liveness, bind perms + rebind, echo-suppress predicate,
`ClipState::remove`. Harness tests use 5 s timeouts — a regression fails
loudly instead of hanging CI.

What they *cannot* prove (needs Layer 2): real `set_selection` against the
compositor, and the live echo-suppress round-trip.

## 2. Lints (must be clean before any manual claim)

```sh
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## 3. Boot the daemon

```sh
RUST_LOG=debug cargo run &
```

Expect, in order:

1. `Listening on "/run/user/1000/uclip.sock"`
2. Registry globals (`wl_seat`, `ext_data_control_manager_v1`, …)
3. `clipboard monitor ready`

Failure modes: `no wl_seat found` (no compositor / wrong `WAYLAND_DISPLAY`),
`no ext_data_control_manager_v1` (compositor didn't grant the privileged
protocol), `Address in use` (stale socket — kill the old daemon; `bind`
unlinks `kill -9` leftovers automatically).

Confirm the socket:

```sh
ls -l $XDG_RUNTIME_DIR/uclip.sock   # expect: srwx------ (0700, owner-only)
```

## 4. Request/response round-trips (one-shot, via socat)

Each request is one JSON object + `\n`; each reply is one line with your `id`
echoed. Keep a terminal with daemon logs visible — every failure also logs
there with context.

```sh
# Ping → Pong (proves framing + JSON + id echo)
echo '{"v":1,"id":"p1","req":"Ping"}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/uclip.sock
# expect: {"v":1,"id":"p1","resp":"Pong"}

# List (empty at first boot, total grows as you copy)
echo '{"v":1,"id":"r1","req":{"List":{"offset":0,"limit":50}}}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/uclip.sock | jq .
# expect: {"v":1,"id":"r1","resp":{"Entries":{"total":N,"entries":[...]}}}

# Bad id → typed Error, daemon stays up (regression test for the old `?` crash)
echo '{"v":1,"id":"bad","req":{"Restore":{"entry_id":999999}}}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/uclip.sock
# expect: {"v":1,"id":"bad","resp":{"Error":{"message":"clipboard entry 999999 not found"}}}

# Garbage → Error, connection stays open for the next line
printf 'not json\n{"v":1,"id":"p2","req":"Ping"}\n' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/uclip.sock
# expect: Error line, then Pong on the same connection
```

Entry rows look like:
`{"id":7,"timestamp_millis":…,"kind":"text","primary_mime":"text/plain;charset=utf-8","preview":"first line…","has_text":true}`.

## 5. Subscribe + live pushes (interactive)

Terminal 1 — open a persistent connection and subscribe (leave it open;
pushes only flow to a live subscribed socket):

```sh
socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/uclip.sock
```

Type/paste and hit Enter (unit variants are bare strings):

```json
{"v":1,"id":"s1","req":"Subscribe"}
```

Expect the ack:

```json
{"v":1,"id":"s1","resp":"Subscribed"}
```

Now copy text in any app. Each copy pushes one line unsolicited:

```json
{"v":1,"event":{"EntryAdded":{"entry":{...}}}}
```

Checks: one push per copy (aliases grouped, not one per MIME); copying the
*same* text twice in a row pushes nothing the second time (consecutive dedup);
`List` afterwards shows the new rows.

If the ack arrives but no pushes: confirm the daemon log shows `clipboard
changed` + `Stored` for that copy. If the Subscribe line gets no reply at
all: missing trailing newline or smart quotes from copy-paste — retype it.

## 6. Restore round-trip (the P3 path — do in order)

```sh
# 1. Pick a text entry id from List
echo '{"v":1,"id":"r1","req":{"List":{"offset":0,"limit":5}}}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/uclip.sock | jq .resp.Entries.entries
```

```sh
# 2. Restore it (replace 7 with a real id)
echo '{"v":1,"id":"rr","req":{"Restore":{"entry_id":7}}}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/uclip.sock
# expect within ~100 ms: {"v":1,"id":"rr","resp":{"Restored":{"entry_id":7}}}
```

```sh
# 3. Paste (Ctrl+V) into any text field → expect byte-exact content of entry 7
```

```sh
# 4. List again → history length UNCHANGED (echo suppressed, no phantom row,
#    no phantom push on subscribed clients)
echo '{"v":1,"id":"r2","req":{"List":{"offset":0,"limit":5}}}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/uclip.sock | jq .resp.Entries.total
```

```sh
# 5. Copy fresh text → history grows by exactly 1 (suppression didn't leak)
```

```sh
# 6. Rapid-fire: send 3 restores back-to-back → all three Restored replies,
#    correct ids, daemon log shows three "restored clipboard entry" lines
for i in 7 6 5; do echo "{\"v\":1,\"id\":\"q$i\",\"req\":{\"Restore\":{\"entry_id\":$i}}}"; done \
  | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/uclip.sock
```

If step 4 shows `total` grew by 1 with a duplicate preview, echo-suppression
missed: check daemon logs for `suppressed own restore echo` (absent = flag
never matched — compare the phantom row's preview to the restored entry's)
and confirm the restore succeeded first (`restored clipboard entry 7`).

## 7. Robustness

```sh
kill -9 <daemon-pid>; RUST_LOG=debug cargo run &   # stale-sock rebind works
ls -l $XDG_RUNTIME_DIR/uclip.sock                  # still srwx------
python3 -c "print('x'*(70*1024))" | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/uclip.sock
# expect: {"resp":{"Error":{"message":"line too long..."}}}, connection alive
```

Idle check: with no clipboard activity and no clients, daemon CPU in
`htop`/`top` should sit at ~0 (the 100 ms `poll` wait). Sustained spinning
means the wait regressed — file a bug against the poll loop before anything
else.

## 8. Python alternative (no socat)

```python
import socket, json
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect('/run/user/1000/uclip.sock')  # or os.environ['XDG_RUNTIME_DIR'] + '/uclip.sock'
f = s.makefile('rw')
def req(id, r):
    f.write(json.dumps({'v': 1, 'id': id, 'req': r}) + '\n'); f.flush()
    return json.loads(f.readline())
print(req('p1', 'Ping'))
print(req('r1', {'List': {'offset': 0, 'limit': 50}}))
print(req('rr', {'Restore': {'entry_id': 7}}))
# subscribe loop: send Subscribe once, then readline() forever for pushes
```

## Pass criteria (P2 + P3 sign-off)

- [ ] Layer 1: 50 passed; Layer 2: zero warnings.
- [ ] Boot logs `Listening` → `monitor ready`; socket `srwx------`.
- [ ] `Ping→Pong`, `List` totals track real copies, bad id → typed `Error`.
- [ ] Subscribe ack + one push per copy, dedup silences immediate repeats.
- [ ] Restore → `Restored` ≤ ~100 ms idle, paste is byte-exact, `total`
      unchanged (no phantom), fresh copy grows by exactly 1, rapid-fire
      serializes with correct ids.
- [ ] `kill -9` rebind, oversized-line `Error`, idle CPU ~0.
