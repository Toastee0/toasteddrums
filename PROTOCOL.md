# ToastedDrums pads protocol — v1

The contract between `fw/pads32/pads32.ino` (device) and `src/pads.rs` (host driver).
**Both sides implement this document. Change it here first, then change both halves.**

The device is an **input device only**. It senses hits and reports them. The host makes all
sound. The device never needs the host to function; the host never needs the device to build.

---

## 1. Transport

| | |
|---|---|
| Physical | UART over USB (CP2102 on the classic ESP32 devkit) |
| Framing | 8N1, ASCII, lines terminated by `\n` (`\r` accepted and ignored) |
| Default rate | 115200. Changeable with `baud`, persisted with `save` |
| Max line | 512 bytes including terminator. Longer lines are truncated and reported as `err` |
| Case | Commands are case-insensitive. Pad names are echoed in canonical case |

### 1.1 DTR and RTS — mandatory

On this devkit **RTS drives EN (reset) and DTR drives GPIO0 (boot mode)** through the
auto-reset transistors. A host that asserts either one will reset the board; DTR alone
holds GPIO0 low and traps it in the ROM bootloader printing `waiting for download`.

> A conforming host MUST open the port with DTR and RTS deasserted, and MUST NOT use
> hardware (RTS/CTS or DSR/DTR) flow control.

Setting `DTR_CONTROL_DISABLE` / `RTS_CONTROL_DISABLE` in the DCB is **not sufficient on its
own**. Those describe the steady state, but the OS still drives these lines around open and
close. Observed in the field: opening a connection and then closing it strands the device
in the ROM bootloader, because the close transition leaves GPIO0 low.

> A conforming host MUST also drive DTR and RTS low explicitly — on Windows,
> `EscapeCommFunction(CLRDTR)` and `EscapeCommFunction(CLRRTS)` — immediately after opening
> and again immediately before closing, so the transition belongs to the host rather than
> to the driver.

Symptom when this is got wrong: the device prints `waiting for download` and stops
responding. Recovery is a power cycle, or holding BOOT while tapping EN.

### 1.2 No XON/XOFF

Software flow control is **not** part of this protocol. `0x11` and `0x13` carry no meaning
and MUST NOT be sent. Rate is controlled by `stream off` or `mode poll`, which are explicit,
visible, and reversible — unlike an XOFF byte, which silently mutes a device with no
indication of why.

---

## 2. Session state machine

```
      power on
         │
         ▼
      ┌──────┐  1s median calibration, pads must not be touched
      │ CAL  │
      └──┬───┘
         ▼
      ┌───────┐  beacons `! offer` once a second, answers commands,
      │ OFFER │  emits NO hits and NO windows
      └──┬────┘
    go   │   ▲  stop
         ▼   │
      ┌──────┴┐  reports hits, by push or poll
      │ LIVE  │
      └───────┘
```

The device **does not stream until a session exists**. A host that opens the port and waits
for hits without sending `go` will wait forever, and that is correct behaviour.

**Standalone fallback:** if no host has sent *any* command within 8000 ms of entering OFFER,
the device enters LIVE on its saved config, so a plain terminal works and the kit is usable
with no host. If a host *has* spoken, the timeout is cancelled permanently — an explicit
`stop` is a decision and is never overridden by a timer.

---

## 3. Command format (host → device)

```
[#<seq> ]<verb>[ <arg>...]\n
```

`#<seq>` is an **optional** decimal tag, 1–65535. If present, the reply carries the same tag.
This exists because unsolicited events interleave with replies: without a tag a host cannot
tell which `ok` belongs to which command when more than one is in flight.

> A host that sends tagged commands MUST NOT assume the next line it reads is the reply.
> Hits, notices and windows may arrive in between.

**Every command produces exactly one `ok` or `err` line, and it comes last.** Commands that
return data emit that data first, then terminate with `ok`. So a host reads lines until it
sees `ok`/`err` — that line closes the response. Silence is always a fault.

---

## 4. Replies (device → host)

```
ok [#<seq> ]<text>
err [#<seq> ]<code> <text>
```

| code | meaning |
|---|---|
| `badcmd` | unknown verb |
| `badarg` | missing or unparseable argument |
| `badpad` | pad selector out of range or unknown name |
| `range` | value outside the accepted range |
| `state` | not valid in the current session state |
| `toolong` | line exceeded the 512-byte limit |

---

## 5. Events (device → host)

Every unsolicited line begins with a type character and a space. A host MUST ignore
message types it does not recognise, so the device can add types without breaking hosts.

### 5.1 `h` — hit

```
h <pad> <name> <vel> <t_ms> <depth_pct> <slope_pctms> <base> <min>
```

| field | type | meaning |
|---|---|---|
| `pad` | 0–7 | index; the stable machine identifier |
| `name` | string | pad label, e.g. `P4`. For humans; do not key on it |
| `vel` | 1–127 | velocity. Already scaled by that pad's `gain` |
| `t_ms` | u32 | device milliseconds at strike onset. Monotonic since boot, **not** wall clock, and wraps after ~49 days |
| `depth` | float | how far below baseline the strike reached, in **counts** |
| `slope` | float | fall rate at the trigger, **counts per ms**. Reported only — it does not gate anything |
| `base` | u32 | the pad's baseline at onset |
| `min` | u32 | deepest raw value during the strike |

All fields are positional and always present. `depth` greater than that pad's `gain` means
`vel` clipped at 127.

Emitted at `hold` ms after onset — that delay is the peak-search window, and is the
device-side latency floor for a hit.

### 5.2 `x` — waveform trace

```
x <pad> <name> <base> <ms_per_sample> <v0>,<v1>,...
```

The samples straddling the onset, oldest first, for calibration. Sent only when `trace on`
and only in push mode. Diagnostic: a host may ignore these entirely.

### 5.3 `w` — raw window

```
w <scans> <name>=<min>/<mean>/<max> ...
```

One group per pad. Sent every `window` ms in `mode raw` or `mode both`.

### 5.4 `v` — levels

```
v <name>=<base>,<current> ...
```

Sent as the last line of a `poll` response. Enough to drive meters without a second request.

### 5.5 `p` — pad status

```
p <pad> <name> <thresh_pct> <slope_pctms> <gain_pct> <base>
```

One per pad, emitted by `get` before its terminating `ok`.

### 5.6 `!` — notice

```
! <free text>
```

Human-readable. A host MUST NOT parse these for control flow — anything a host needs to act
on has its own message type. Notably `! offer ...` and `! cal ...` are informational; the
authoritative machine-readable state comes from `get`.

### 5.7 `cfg` — config blob

```
cfg <key>=<value> ...
```

Round-trips verbatim: what `dump` emits, `cfg` accepts. Keys are the same as `set`, plus
per-pad `p<N>=<thresh>,<slope>,<gain>`. A blob may be partial; omitted keys keep their value.

---

## 6. Commands

### Session
| command | reply |
|---|---|
| `hello <name>` | `ok hello <name> from pads32 proto=1 pads=<n>`, then a `cfg` line |
| `go` | `ok go live` |
| `stop` | `ok stop offer` |
| `ping` | `ok pong` |
| `id` | `ok id pads32 proto=1 pads=<n> build=<date>` |

### Query
| command | reply |
|---|---|
| `get` | a `cfg` line, one `p` line per pad, then `ok get` |
| `dump` | a `cfg` line, then `ok dump` |
| `poll` | `n` `h` lines, one `v` line, then `ok poll n=<n> ms=<t> dropped=<d>` |
| `help` | `ok cmds: ...` |

### Control
| command | reply |
|---|---|
| `cal` | `ok cal` (takes ~1 s; pads must not be touched) |
| `mode hits\|raw\|both\|poll` | `ok mode <m>` |
| `stream on\|off` | `ok stream <s>` |
| `trace on\|off` | `ok trace <t>` |
| `baud <n>` | `ok baud <n>` **sent at the old rate**, then the device switches |
| `save` | `ok save` then reboot — the link drops and the device re-calibrates |

### Tuning
```
set <key> <value>          all pads
set <key> <pad> <value>    one pad; <pad> is an index 0-7 or a name like p4
```

| key | scope | range | reboot? | meaning |
|---|---|---|---|---|
| `thresh` | per-pad | 1–4000 counts | no | how far below baseline a hit must go. **The only trigger condition.** |
| `slope` | per-pad | 0.1–2000 counts/ms | no | reported with each hit; **does not gate** |
| `gain` | per-pad | 1–4000 counts | no | drop that maps to velocity 127 |
| `lockout` | global | 0–1000 ms | no | retrigger lockout |
| `hold` | global | 1–200 ms | no | peak-search window; the latency floor |
| `window` | global | 5–5000 ms | no | `w` message period |
| `dur` | global | 1–100 ms | **yes** | touch measurement window |

Everything except `dur` takes effect on the next sample, so a host can offer **live
sensitivity sliders**. `dur` is latched by `touchSetConfig()` before the first `touchRead()`,
so it needs `save` and a reboot.

### 6.1 Absolute counts, a static baseline, and one gate

The trigger is a single condition: **did the signal drop more than `thresh` counts below the
baseline.** Nothing else.

Measured, resting → struck: `P2 150→100`, `P4 160→112`, `P33 500→200`, `P32 500→200`. Wall
plates swing ~300 counts, floor plates ~50, so one shared threshold cannot serve both —
hence per-pad. But per-pad **counts**, not percentages.

Three earlier designs were tried and removed, each after causing a fault:

- **Percentage-of-baseline thresholds.** Meant to let a pad move between mountings without
  retuning. In practice the numbers are read off the raw stream in counts, and converting
  hid what was happening.
- **A slope gate**, on the theory that these electrodes sense at a distance so depth alone
  would fire on approach. The floor plates rise far more slowly than the wall plates, so any
  single slope value silenced them — they presented as dead pads.
- **A drifting baseline** (slow IIR) plus a **wedge recovery** that rebased when deflection
  persisted. The tracker could be dragged by a resting hand, and the recovery could rebase
  onto a touched value and strand the pad until reboot.

The baseline is therefore **static**: measured once by the boot calibration and never moved.
With the plates fixed in place the resting numbers are consistent. Re-measure deliberately
with `cal`; nothing re-measures silently while you play.

---

## 7. Host obligations

1. Open with DTR and RTS **deasserted**. Do not enable any hardware flow control.
2. Send `hello <name>` then `go`. Expect nothing before that.
3. Treat any unrecognised line type as ignorable, not as an error.
4. Do not assume the next line after a command is its reply. Tag with `#<seq>` if it matters.
5. Treat `t_ms` as monotonic device time, not wall clock. Do not compare it across a reboot.
6. If using `poll`, poll often enough — the queue holds 64 hits and drops **oldest first**,
   reporting the count in `dropped=`. A dropped beat is reported, never silent.
