// pads32 — ToastedDrums pad controller (classic ESP32 devkit, CP2102).
//
// IMPLEMENTS PROTOCOL.md v1. That document is the contract; this file and src/pads.rs both
// conform to it. Change the contract first, then both halves.
//
// An INPUT DEVICE ONLY: it senses hits and reports them. The host makes all sound.
//
// ---------------------------------------------------------------------------------------
// HARDWARE FACTS (measured on this board -- do not re-derive)
// ---------------------------------------------------------------------------------------
// Verified touch map (soc/touch_sensor_channel.h, esp32 core 3.3.11):
//   T0=GPIO4  T1=GPIO0  T2=GPIO2  T3=GPIO15 T4=GPIO13
//   T5=GPIO12 T6=GPIO14 T7=GPIO27 T8=GPIO33 T9=GPIO32
// GPIO16/17 have NO touch hardware; touchRead() returns 0 for them forever.
// GPIO0 is the BOOT strap: its pull-up clamps it to a flat 0, and the CP2102 DTR line
// drives it -- a host asserting DTR traps the chip in "waiting for download".
//
// POLARITY: the value FALLS when touched. Matches esp32-hal-touch.h:
//   "for ESP32 values close to 0 mean touch detected".
//
// Core 3.3.11 is IDF >= 5.5, so touchSetCycles() does NOT exist -- the NG driver is used
// even on the classic ESP32. Tuning is touchSetConfig(duration_ms, volt_lo, volt_hi),
// LATCHED before the first touchRead(). That is why `dur` alone needs a reboot.
//
// MEASURED, resting -> struck (operator, from the raw bench):
//   P2  150 -> 100   P4  160 -> 112   P33 500 -> 200   P32 500 -> 200
// Thresholds are ABSOLUTE COUNTS, per pad. Wall plates swing ~300 counts, floor plates ~50,
// so one shared number cannot serve both. Percentages were tried and abandoned: they hide
// what is happening and the numbers are read off the raw stream in counts anyway.
// ---------------------------------------------------------------------------------------

#include <Preferences.h>
#include <stdarg.h>

// Defined before anything else: the Arduino builder injects function prototypes right after
// the include block, so a type used in a signature must already exist here.
struct Ev {
  uint8_t  idx;
  uint8_t  vel;
  uint32_t t;
  float    depth;
  float    slope;
  float    base;
  uint32_t vmin;
};

static const uint8_t PINS[]  = { 2, 4, 33, 32 };
static const char   *NAMES[] = { "P2", "P4", "P33", "P32" };
static const uint8_t N = 4;

static const uint8_t  RING          = 64;
static const uint32_t TRACE_POST_MS = 32;
static const uint16_t CAL_SAMPLES   = 256;
static const uint16_t CAL_MS        = 1000;
static const uint32_t BEACON_MS     = 1000;
static const uint32_t AUTO_LIVE_MS  = 8000;
static const uint8_t  EVQ           = 64;
static const uint16_t LINEBUF      = 512;   // PROTOCOL.md 1

Preferences prefs;

struct Cfg {
  float    dur_ms;
  float    thresh_cnt[N];
  float    slope_cps[N];
  float    gain_cnt[N];
  uint16_t lockout_ms;
  uint16_t hold_ms;
  uint16_t window_ms;
  uint32_t baud;
} cfg;

// ABSOLUTE COUNTS, not percentages of baseline.
//
// Percentages were a mistake. They were meant to let a pad move between mountings without
// retuning, but the operator reads thresholds straight off the raw stream in counts, and
// converting back and forth just hid what was going on. Counts are what is measured, so
// counts are what is configured.
//
// Operator measurement from the raw bench, resting -> struck:
//
//   pad   nominal   hit    drop   trigger at
//   P2      150     100     50        35
//   P4      160     112     48        35
//   P33     500     200    300       150
//   P32     500     200    300       150
//
// gain is the drop that maps to velocity 127, set at the observed full-strike drop so a
// hard hit lands at the top of the scale without everything above half clipping there.
//
// slope is RETAINED ONLY AS A REPORTED DIAGNOSTIC. It used to gate the trigger as well, on
// the theory that it separated a strike from a hand hovering — but the floor plates rise at
// a fraction of the wall plates' rate, so any single slope gate silenced them. The trigger
// is now one thing: did the signal bounce past thresh counts below the baseline.
struct PadDefault { float thresh, slope, gain; };
// gain is deliberately ~1.6x the measured typical drop, NOT equal to it. Setting gain to
// the typical drop means every ordinary hit computes vel = depth/gain*127 = 127, so the
// whole kit plays at maximum and there is no dynamic range at all -- observed as "all floor
// hits come in at vel 127". With headroom a normal strike lands near 80 and only a hard one
// reaches the top.
static const PadDefault PAD_DEFAULTS[N] = {
  {  35.0f,  8.0f,  80.0f },   // P2  floor, typical drop  50
  {  35.0f,  8.0f,  78.0f },   // P4  floor, typical drop  48
  { 150.0f, 35.0f, 480.0f },   // P33 wall,  typical drop 300
  { 150.0f, 35.0f, 480.0f },   // P32 wall,  typical drop 300
};

static void cfgDefaults() {
  cfg.dur_ms = 10.0f;
  for (uint8_t i = 0; i < N; i++) {
    cfg.thresh_cnt[i]  = PAD_DEFAULTS[i].thresh;
    cfg.slope_cps[i] = PAD_DEFAULTS[i].slope;
    cfg.gain_cnt[i]    = PAD_DEFAULTS[i].gain;
  }
  cfg.lockout_ms = 40; cfg.hold_ms = 12; cfg.window_ms = 100;
  cfg.baud = 115200;
}

static void cfgLoad() {
  cfgDefaults();
  char k[16];
  prefs.begin("pads32", true);
  cfg.dur_ms     = prefs.getFloat ("dur",     cfg.dur_ms);
  cfg.lockout_ms = prefs.getUShort("lockout", cfg.lockout_ms);
  cfg.hold_ms    = prefs.getUShort("hold",    cfg.hold_ms);
  cfg.window_ms  = prefs.getUShort("window",  cfg.window_ms);
  cfg.baud       = prefs.getULong ("baud",    cfg.baud);
  for (uint8_t i = 0; i < N; i++) {
    snprintf(k, sizeof(k), "thresh%u", i); cfg.thresh_cnt[i]  = prefs.getFloat(k, cfg.thresh_cnt[i]);
    snprintf(k, sizeof(k), "slope%u",  i); cfg.slope_cps[i] = prefs.getFloat(k, cfg.slope_cps[i]);
    snprintf(k, sizeof(k), "gain%u",   i); cfg.gain_cnt[i]    = prefs.getFloat(k, cfg.gain_cnt[i]);
  }
  prefs.end();
}

static void cfgSave() {
  char k[16];
  prefs.begin("pads32", false);
  prefs.putFloat ("dur",     cfg.dur_ms);
  prefs.putUShort("lockout", cfg.lockout_ms);
  prefs.putUShort("hold",    cfg.hold_ms);
  prefs.putUShort("window",  cfg.window_ms);
  prefs.putULong ("baud",    cfg.baud);
  for (uint8_t i = 0; i < N; i++) {
    snprintf(k, sizeof(k), "thresh%u", i); prefs.putFloat(k, cfg.thresh_cnt[i]);
    snprintf(k, sizeof(k), "slope%u",  i); prefs.putFloat(k, cfg.slope_cps[i]);
    snprintf(k, sizeof(k), "gain%u",   i); prefs.putFloat(k, cfg.gain_cnt[i]);
  }
  prefs.end();
}

enum PadState : uint8_t { IDLE, IN_HIT, LOCKED };

struct Pad {
  float    base;
  uint32_t prev_v;
  uint32_t prev_t;
  PadState st;
  uint32_t t_onset;
  uint32_t v_min;
  float    slope_at;
  float    base_at;
  uint32_t cur;
  uint16_t ring[RING];
  uint8_t  ri;
  uint32_t last_ring_ms;
  bool     trace_pending;
  uint32_t trace_at;
} pad[N];

static Ev       evq[EVQ];
static uint8_t  ev_head = 0, ev_count = 0;
static uint16_t ev_dropped = 0;

static const uint32_t SLOPE_DT_MS = 3;

enum Link : uint8_t { L_OFFER, L_LIVE };
static Link     lnk = L_OFFER;
static char     peer[24] = "";
static bool     host_seen = false;
static uint32_t offer_since = 0, last_beacon = 0;

static bool streaming = true;
static bool tracing   = true;
enum Mode : uint8_t { M_HITS, M_RAW, M_BOTH, M_POLL };
static Mode mode = M_BOTH;

// PROTOCOL.md 3: the optional #<seq> tag of the command being handled. 0 = untagged.
static uint16_t cur_seq = 0;

static inline bool sessionUp()   { return lnk == L_LIVE && streaming; }
static inline bool pushHits()    { return sessionUp() && mode != M_RAW && mode != M_POLL; }
static inline bool pushWindows() { return sessionUp() && (mode == M_RAW || mode == M_BOTH); }

// ---- replies (PROTOCOL.md 4) ----------------------------------------------------------
// Exactly one of these terminates every command, and it always comes last.
static void ok(const char *fmt, ...) {
  char body[240];
  va_list ap; va_start(ap, fmt);
  vsnprintf(body, sizeof(body), fmt, ap);
  va_end(ap);
  if (cur_seq) Serial.printf("ok #%u %s\n", cur_seq, body);
  else         Serial.printf("ok %s\n", body);
}

static void fail(const char *code, const char *fmt, ...) {
  char body[200];
  va_list ap; va_start(ap, fmt);
  vsnprintf(body, sizeof(body), fmt, ap);
  va_end(ap);
  if (cur_seq) Serial.printf("err #%u %s %s\n", cur_seq, code, body);
  else         Serial.printf("err %s %s\n", code, body);
}

// ---- helpers --------------------------------------------------------------------------
static int resolvePad(const char *s) {
  if (!s) return -1;
  if (s[0] >= '0' && s[0] <= '9' && s[1] == 0) {
    int i = s[0] - '0';
    return (i >= 0 && i < N) ? i : -1;
  }
  for (uint8_t i = 0; i < N; i++) {
    const char *n = NAMES[i];
    uint8_t k = 0; bool same = true;
    for (; n[k]; k++) {
      char a = n[k]; if (a >= 'A' && a <= 'Z') a += 32;
      if (s[k] != a) { same = false; break; }
    }
    if (same && s[k] == 0) return i;
  }
  return -1;
}

// Range-checks per PROTOCOL.md 6. Returns false and replies `err range` if out of bounds.
static bool inRange(const char *key, float v, float lo, float hi) {
  if (v >= lo && v <= hi) return true;
  fail("range", "%s must be %.4g..%.4g", key, lo, hi);
  return false;
}

static uint16_t calbuf[N][CAL_SAMPLES];

static uint16_t medianOf(uint16_t *a, uint16_t n) {
  for (uint16_t i = 1; i < n; i++) {
    uint16_t k = a[i]; int j = (int)i - 1;
    while (j >= 0 && a[j] > k) { a[j + 1] = a[j]; j--; }
    a[j + 1] = k;
  }
  return a[n / 2];
}

// Median, not mean: it ignores outliers in both directions, so a mains spike or an
// accidental brush during the second cannot drag the resting level.
static void calibrate() {
  Serial.println("! calibrating 1s - DO NOT TOUCH THE PADS");

  // Prime the channels before measuring. The NG driver initialises a touch channel on its
  // FIRST touchRead(), and those early reads come back as 0. Sampling straight away
  // averaged those zeros into the baseline, producing pads with base=0 that could never
  // reach a threshold and so never fired -- observed as P4 base=0 while its neighbours
  // were fine. Throw the first reads away.
  for (uint8_t warm = 0; warm < 64; warm++) {
    for (uint8_t i = 0; i < N; i++) (void)touchRead(PINS[i]);
    delay(2);
  }

  const uint32_t step_us = (uint32_t)CAL_MS * 1000UL / CAL_SAMPLES;
  for (uint16_t s = 0; s < CAL_SAMPLES; s++) {
    uint32_t t = micros();
    for (uint8_t i = 0; i < N; i++) calbuf[i][s] = (uint16_t)touchRead(PINS[i]);
    while (micros() - t < step_us) { }
  }
  for (uint8_t i = 0; i < N; i++) {
    uint16_t lo = 0xFFFF, hi = 0;
    for (uint16_t s = 0; s < CAL_SAMPLES; s++) {
      if (calbuf[i][s] < lo) lo = calbuf[i][s];
      if (calbuf[i][s] > hi) hi = calbuf[i][s];
    }
    uint16_t med = medianOf(calbuf[i], CAL_SAMPLES);
    Pad &p = pad[i];
    p.base = (float)med; p.cur = med;
    p.prev_v = med; p.prev_t = millis();
    p.st = IDLE; p.trace_pending = false;
    for (uint8_t k = 0; k < RING; k++) p.ring[k] = med;
    p.ri = 0; p.last_ring_ms = millis();
    Serial.printf("! cal %s base=%u min=%u max=%u spread=%u\n",
                  NAMES[i], med, lo, hi, (unsigned)(hi - lo));

    // A live electrode always wanders by tens of counts as the room moves around it. The
    // plate itself cannot fail -- it is sheet metal and a wire -- so a tiny, MOTIONLESS
    // reading is never the pad. It is the measurement: a channel that has not initialised,
    // or a connection that is open or shorted. Say that, because the symptom otherwise
    // presents as a silent pad and gets blamed on thresholds.
    if (med < 20 || (hi - lo) < 2)
      Serial.printf("! WARN %s not measuring (base=%u spread=%u) - channel or connector, not the plate\n",
                    NAMES[i], med, (unsigned)(hi - lo));
  }
}

// ---- UART: never stall the detector ----------------------------------------------------
// HardwareSerial blocks once its TX ring fills. A 512-byte trace is ~44ms of transmit time
// at 115200 -- long enough to miss strikes while the loop sits in Serial.println(). So
// every unsolicited write checks for room first and is dropped if there is none. Hits are
// also queued for `poll`, so a dropped push is a lost diagnostic, not a lost beat.
// (Arduino exposes no UART DMA path; this ring is the practical equivalent. Reaching real
// DMA would mean driving the IDF uart driver directly.)
static uint16_t tx_dropped = 0;

static bool txRoom(size_t n) {
  return (size_t)Serial.availableForWrite() >= n;
}

// ---- events (PROTOCOL.md 5) -----------------------------------------------------------
static void emitHit(const Ev &e) {
  Serial.printf("h %u %s %u %lu %.1f %.2f %.0f %lu\n",
                e.idx, NAMES[e.idx], e.vel, (unsigned long)e.t,
                e.depth, e.slope, e.base, (unsigned long)e.vmin);
}

static void emitTrace(uint8_t i) {
  // The single biggest write in the system. Skip it rather than stall the detector.
  if (!txRoom(400)) { if (tx_dropped < 0xFFFF) tx_dropped++; return; }
  Pad &p = pad[i];
  char line[LINEBUF];
  int o = snprintf(line, sizeof(line), "x %u %s %.0f 1 ", i, NAMES[i], p.base_at);
  uint8_t start = p.ri;                    // oldest sample sits at the write cursor
  for (uint8_t k = 0; k < RING; k++) {
    uint8_t idx = (uint8_t)((start + k) % RING);
    o += snprintf(line + o, sizeof(line) - o, "%s%u", k ? "," : "", p.ring[idx]);
    if (o >= (int)sizeof(line) - 8) break;
  }
  Serial.println(line);
}

static void emitCfg() {
  char line[LINEBUF];
  int o = snprintf(line, sizeof(line), "cfg dur=%.1f lockout=%u hold=%u window=%u baud=%lu",
                   cfg.dur_ms, cfg.lockout_ms, cfg.hold_ms, cfg.window_ms,
                   (unsigned long)cfg.baud);
  for (uint8_t i = 0; i < N; i++)
    o += snprintf(line + o, sizeof(line) - o, " p%u=%.1f,%.2f,%.1f",
                  i, cfg.thresh_cnt[i], cfg.slope_cps[i], cfg.gain_cnt[i]);
  Serial.println(line);
}

static void emitOffer() {
  char line[LINEBUF];
  int o = snprintf(line, sizeof(line), "! offer pads32 proto=1 pads=%u", N);
  for (uint8_t i = 0; i < N; i++)
    o += snprintf(line + o, sizeof(line) - o, " %s=%.0f", NAMES[i], pad[i].base);
  Serial.println(line);
}

static void pushEv(const Ev &e) {
  if (ev_count == EVQ) {                   // drop oldest; a stalled host must not block us
    ev_head = (uint8_t)((ev_head + 1) % EVQ);
    ev_count--;
    if (ev_dropped < 0xFFFF) ev_dropped++;
  }
  evq[(ev_head + ev_count) % EVQ] = e;
  ev_count++;
}

// ---- commands (PROTOCOL.md 6) ---------------------------------------------------------
static void cmdPoll() {
  uint8_t n = ev_count;
  uint16_t dropped = ev_dropped;
  while (ev_count) {
    emitHit(evq[ev_head]);
    ev_head = (uint8_t)((ev_head + 1) % EVQ);
    ev_count--;
  }
  ev_dropped = 0;

  char line[LINEBUF];
  int o = snprintf(line, sizeof(line), "v");
  for (uint8_t i = 0; i < N; i++)
    o += snprintf(line + o, sizeof(line) - o, " %s=%.0f,%lu",
                  NAMES[i], pad[i].base, (unsigned long)pad[i].cur);
  Serial.println(line);

  ok("poll n=%u ms=%lu dropped=%u", n, (unsigned long)millis(), dropped);
}

static void cmdGet() {
  emitCfg();
  for (uint8_t i = 0; i < N; i++)
    Serial.printf("p %u %s %.1f %.2f %.1f %.0f\n", i, NAMES[i],
                  cfg.thresh_cnt[i], cfg.slope_cps[i], cfg.gain_cnt[i], pad[i].base);
  ok("get pads=%u mode=%s stream=%s trace=%s link=%s peer=%s queued=%u txdrop=%u", N,
     mode == M_HITS ? "hits" : mode == M_RAW ? "raw" : mode == M_POLL ? "poll" : "both",
     streaming ? "on" : "off", tracing ? "on" : "off",
     lnk == L_LIVE ? "live" : "offer", peer[0] ? peer : "-", ev_count, tx_dropped);
}

// Unknown keys are counted and reported, never silently dropped: a typo in a stored preset
// must be visible rather than mysteriously ineffective.
static void cmdCfgBlob(char *rest) {
  int good = 0, bad = 0;
  for (char *tok = strtok(rest, " "); tok; tok = strtok(NULL, " ")) {
    char *eq = strchr(tok, '=');
    if (!eq) { bad++; continue; }
    *eq = 0;
    char *key = tok, *val = eq + 1;
    if      (!strcmp(key, "dur"))     { cfg.dur_ms     = atof(val);           good++; }
    else if (!strcmp(key, "lockout")) { cfg.lockout_ms = (uint16_t)atoi(val); good++; }
    else if (!strcmp(key, "hold"))    { cfg.hold_ms    = (uint16_t)atoi(val); good++; }
    else if (!strcmp(key, "baud"))    { cfg.baud       = (uint32_t)atol(val); good++; }
    else if (!strcmp(key, "window"))  { int w = atoi(val); cfg.window_ms = (uint16_t)(w < 5 ? 5 : w); good++; }
    else if (key[0] == 'p' && key[1] >= '0' && key[1] <= '9' && key[2] == 0) {
      int i = key[1] - '0';
      if (i < 0 || i >= N) { bad++; continue; }
      cfg.thresh_cnt[i] = atof(val);
      char *c1 = strchr(val, ',');
      if (c1) cfg.slope_cps[i] = atof(c1 + 1);
      char *c2 = c1 ? strchr(c1 + 1, ',') : NULL;
      if (c2) cfg.gain_cnt[i] = atof(c2 + 1);
      good++;
    }
    else bad++;
  }
  ok("cfg applied=%d rejected=%d", good, bad);
}

static void cmdSet(char *a1, char *a2, char *a3) {
  if (!a1 || !a2) { fail("badarg", "set needs <key> [<pad>] <value>"); return; }

  if (!strcmp(a1, "lockout")) { float v = atof(a2); if (!inRange("lockout", v, 0, 1000)) return;
                                cfg.lockout_ms = (uint16_t)v; ok("set lockout %u", cfg.lockout_ms); return; }
  if (!strcmp(a1, "hold"))    { float v = atof(a2); if (!inRange("hold", v, 1, 200)) return;
                                cfg.hold_ms = (uint16_t)v; ok("set hold %u", cfg.hold_ms); return; }
  if (!strcmp(a1, "window"))  { float v = atof(a2); if (!inRange("window", v, 5, 5000)) return;
                                cfg.window_ms = (uint16_t)v; ok("set window %u", cfg.window_ms); return; }
  if (!strcmp(a1, "dur"))     { float v = atof(a2); if (!inRange("dur", v, 1, 100)) return;
                                cfg.dur_ms = v; ok("set dur %.1f needs save+reboot", cfg.dur_ms); return; }

  float *tgt = NULL; float lo = 0, hi = 0;
  if      (!strcmp(a1, "thresh")) { tgt = cfg.thresh_cnt;  lo = 1;    hi = 4000; }
  else if (!strcmp(a1, "slope"))  { tgt = cfg.slope_cps;   lo = 0.1;  hi = 2000; }
  else if (!strcmp(a1, "gain"))   { tgt = cfg.gain_cnt;    lo = 1;    hi = 4000; }
  else { fail("badcmd", "unknown key %s", a1); return; }

  if (a3) {
    int i = resolvePad(a2);
    if (i < 0) { fail("badpad", "no pad %s", a2); return; }
    float v = atof(a3);
    if (!inRange(a1, v, lo, hi)) return;
    tgt[i] = v;
    ok("set %s %s %.3f", a1, NAMES[i], v);
  } else {
    float v = atof(a2);
    if (!inRange(a1, v, lo, hi)) return;
    for (uint8_t i = 0; i < N; i++) tgt[i] = v;
    ok("set %s all %.3f", a1, v);
  }
}

static void handle(char *s) {
  while (*s == ' ') s++;
  for (char *p = s; *p; p++) if (*p >= 'A' && *p <= 'Z') *p += 32;
  if (!*s) return;

  // PROTOCOL.md 3: strip an optional leading #<seq> tag; the reply echoes it.
  cur_seq = 0;
  if (*s == '#') {
    char *e = NULL;
    long v = strtol(s + 1, &e, 10);
    if (e && e > s + 1 && v > 0 && v <= 65535) { cur_seq = (uint16_t)v; s = e; while (*s == ' ') s++; }
  }
  if (!*s) { fail("badcmd", "empty"); return; }

  // cfg carries the whole remainder, so catch it before strtok chews the line up.
  if (!strncmp(s, "cfg ", 4)) { cmdCfgBlob(s + 4); return; }

  char *verb = strtok(s, " ");
  char *a1   = strtok(NULL, " ");
  char *a2   = strtok(NULL, " ");
  char *a3   = strtok(NULL, " ");

  if      (!strcmp(verb, "ping")) ok("pong");
  else if (!strcmp(verb, "id"))   ok("id pads32 proto=1 pads=%u build=%s %s", N, __DATE__, __TIME__);
  else if (!strcmp(verb, "get"))  cmdGet();
  else if (!strcmp(verb, "dump")) { emitCfg(); ok("dump"); }
  else if (!strcmp(verb, "poll")) cmdPoll();
  else if (!strcmp(verb, "hello")) {
    snprintf(peer, sizeof(peer), "%s", a1 ? a1 : "anon");
    host_seen = true;
    emitCfg();                       // host populates its sliders from this
    ok("hello %s from pads32 proto=1 pads=%u", peer, N);
  }
  else if (!strcmp(verb, "go"))   { lnk = L_LIVE;  host_seen = true; ok("go live"); }
  else if (!strcmp(verb, "stop")) { lnk = L_OFFER; host_seen = true; offer_since = millis(); ok("stop offer"); }
  else if (!strcmp(verb, "cal") || !strcmp(verb, "zero")) { calibrate(); ok("cal"); }
  else if (!strcmp(verb, "help")) ok("cmds: hello go stop poll ping id get dump cfg cal mode stream trace baud set save help");
  else if (!strcmp(verb, "stream")) {
    if (!a1) { fail("badarg", "stream needs on|off"); return; }
    streaming = !strcmp(a1, "on");
    ok("stream %s", streaming ? "on" : "off");
  }
  else if (!strcmp(verb, "trace")) {
    if (!a1) { fail("badarg", "trace needs on|off"); return; }
    tracing = !strcmp(a1, "on");
    ok("trace %s", tracing ? "on" : "off");
  }
  else if (!strcmp(verb, "mode")) {
    if      (a1 && !strcmp(a1, "hits")) mode = M_HITS;
    else if (a1 && !strcmp(a1, "raw"))  mode = M_RAW;
    else if (a1 && !strcmp(a1, "both")) mode = M_BOTH;
    else if (a1 && !strcmp(a1, "poll")) mode = M_POLL;
    else { fail("badarg", "mode needs hits|raw|both|poll"); return; }
    ok("mode %s", a1);
  }
  else if (!strcmp(verb, "baud")) {
    if (!a1) { fail("badarg", "baud needs a rate"); return; }
    long b = atol(a1);
    if (!inRange("baud", (float)b, 9600, 2000000)) return;
    cfg.baud = (uint32_t)b;
    ok("baud %ld", b);               // acked at the OLD rate, then switch
    Serial.flush();
    Serial.updateBaudRate((uint32_t)b);
  }
  else if (!strcmp(verb, "save")) {
    cfgSave(); ok("save"); Serial.flush(); delay(60); ESP.restart();
  }
  else if (!strcmp(verb, "set")) cmdSet(a1, a2, a3);
  else fail("badcmd", "unknown %s", verb);
}

// PROTOCOL.md 1: no XON/XOFF. 0x11/0x13 carry no meaning and are treated as ordinary bytes.
static void pollSerial() {
  static char buf[LINEBUF];
  static uint16_t len = 0;
  static bool overrun = false;
  while (Serial.available()) {
    int c = Serial.read();
    if (c == '\r') continue;
    if (c == '\n') {
      buf[len] = 0;
      if (overrun) { cur_seq = 0; fail("toolong", "line over %u bytes", LINEBUF); }
      else if (len) handle(buf);
      len = 0; overrun = false;
      continue;
    }
    if (len < LINEBUF - 1) buf[len++] = (char)c; else overrun = true;
  }
}

void setup() {
  cfgLoad();
  // A real TX ring, requested BEFORE begin(). Without this the ESP32 has only its 128-byte
  // hardware FIFO, so availableForWrite() can never report more than 128 -- which silently
  // made every txRoom() check for a window (160) or a trace (400) fail forever, and killed
  // all streaming output while command replies still worked. The ring also means writes are
  // buffered rather than spinning on the FIFO.
  Serial.setTxBufferSize(2048);
  Serial.begin(cfg.baud);
  delay(800);
  touchSetConfig(cfg.dur_ms, TOUCH_VOLT_LIM_L_0V5, TOUCH_VOLT_LIM_H_1V7);

  Serial.println();
  Serial.printf("! id pads32 proto=1 pads=%u baud=%lu build=%s %s\n",
                N, (unsigned long)cfg.baud, __DATE__, __TIME__);
  calibrate();
  emitCfg();
  emitOffer();
  Serial.println("! offering - send: hello <name> then go");

  lnk = L_OFFER;
  offer_since = millis();
  last_beacon = millis();
}

void loop() {
  static uint32_t wt0 = 0, wn = 0;
  static uint32_t lo[N], hi[N];
  static uint64_t acc[N];

  if (wt0 == 0) {
    wt0 = millis(); wn = 0;
    for (uint8_t i = 0; i < N; i++) { lo[i] = 0xFFFFFFFF; hi[i] = 0; acc[i] = 0; }
  }

  uint32_t now = millis();

  if (lnk == L_OFFER) {
    if (now - last_beacon >= BEACON_MS) { emitOffer(); last_beacon = now; }
    // Only auto-start if no host ever spoke. An explicit `stop` is a decision, not a lull.
    if (!host_seen && now - offer_since >= AUTO_LIVE_MS) {
      Serial.println("! no host - going live on saved config");
      lnk = L_LIVE;
    }
  }

  for (uint8_t i = 0; i < N; i++) {
    uint32_t v = touchRead(PINS[i]);
    Pad &p = pad[i];
    p.cur = v;

    if (v < lo[i]) lo[i] = v;
    if (v > hi[i]) hi[i] = v;
    acc[i] += v;

    if (now - p.last_ring_ms >= 1) {
      p.ring[p.ri] = (uint16_t)v;
      p.ri = (uint8_t)((p.ri + 1) % RING);
      p.last_ring_ms = now;
    }

    if (p.trace_pending && now >= p.trace_at) {
      if (pushHits() && tracing) emitTrace(i);
      p.trace_pending = false;
    }

    float thresh = cfg.thresh_cnt[i];            // absolute counts
    float defl   = p.base - (float)v;

    // The baseline is STATIC: measured once by the boot calibration, then never moved.
    // With the plates fixed in place the resting numbers are consistent, so there is no
    // drift worth chasing -- and both mechanisms that chased it caused bugs. An IIR tracker
    // that a resting hand could drag downward, and a "wedge recovery" that rebased onto a
    // touched value and stranded the pad until reboot ("crash worked twice then stopped").
    // Re-measure deliberately with `cal`, never silently while playing.

    switch (p.st) {
      case IDLE: {
        // Just a threshold crossing: did the signal bounce past thresh counts below the
        // baseline. Nothing else. There used to be a slope gate here as well, on the theory
        // that these electrodes sense at a distance so depth alone would fire on approach --
        // but it rejected real strikes on the floor plates, whose swing is ~50 counts
        // against the wall plates' ~300, and it made the pads look dead. One gate, in the
        // units the signal is actually read in.
        if (defl > thresh) {
          float dt = (float)(now - p.prev_t);
          if (dt < 1.0f) dt = 1.0f;
          p.st = IN_HIT; p.t_onset = now; p.v_min = v; p.base_at = p.base;
          // Still reported, purely as a diagnostic — it no longer decides anything.
          p.slope_at = ((float)p.prev_v - (float)v) / dt;
        }
        if (now - p.prev_t >= SLOPE_DT_MS) { p.prev_v = v; p.prev_t = now; }
        break;
      }
      case IN_HIT: {
        if (v < p.v_min) p.v_min = v;
        if (now - p.t_onset >= cfg.hold_ms) {
          float depth_cnt = p.base_at - (float)p.v_min;    // counts below baseline
          int vel = (int)(depth_cnt / cfg.gain_cnt[i] * 127.0f + 0.5f);
          if (vel < 1)   vel = 1;
          if (vel > 127) vel = 127;

          Ev e; e.idx = i; e.vel = (uint8_t)vel; e.t = p.t_onset; e.depth = depth_cnt;
          e.slope = p.slope_at; e.base = p.base_at; e.vmin = p.v_min;

          // Queue always, so `poll` works regardless of mode; push only if asked to.
          if (lnk == L_LIVE) pushEv(e);
          if (pushHits()) {
            if (txRoom(80)) {
              emitHit(e);
            } else if (tx_dropped < 0xFFFF) tx_dropped++;
            if (tracing) { p.trace_pending = true; p.trace_at = p.t_onset + TRACE_POST_MS; }
          }
          p.st = LOCKED; p.t_onset = now;
        }
        break;
      }
      case LOCKED: {
        // Re-arm only once the signal has come back UP past the release level, not merely
        // when the lockout timer expires. The baseline is static, so a foot resting on the
        // pad holds the reading below thresh indefinitely -- without this the detector
        // refires the instant lockout ends, every hold+lockout ms. Heard as a machine-gun
        // stutter while a pad is held down.
        //
        // Releasing at half the trigger threshold is ordinary hysteresis: far enough below
        // the trigger that a real strike's decay clears it, far enough above zero that
        // resting noise cannot rattle the pad back and forth across the boundary.
        if (now - p.t_onset >= cfg.lockout_ms && defl < thresh * 0.5f) {
          p.st = IDLE; p.prev_v = v; p.prev_t = now;
        }
        break;
      }
    }
  }
  wn++;

  if (now - wt0 >= cfg.window_ms) {
    if (pushWindows()) {
      if (!txRoom(160)) { if (tx_dropped < 0xFFFF) tx_dropped++; wt0 = 0; return; }
      char line[LINEBUF];
      int o = snprintf(line, sizeof(line), "w %lu", (unsigned long)wn);
      for (uint8_t i = 0; i < N; i++)
        o += snprintf(line + o, sizeof(line) - o, " %s=%lu/%lu/%lu",
                      NAMES[i], (unsigned long)lo[i],
                      (unsigned long)(acc[i] / (wn ? wn : 1)), (unsigned long)hi[i]);
      Serial.println(line);
    }
    wt0 = 0;
  }

  pollSerial();
}
