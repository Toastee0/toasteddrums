// pads — ToastedDrums touch-pad characterization bench (SILENT: no audio, numbers only).
//
// Board: Seeed ReSpeaker Lite carrier, factory-soldered XIAO ESP32-S3.
// The carrier owns GPIO5/6 (I2C: XMOS 0x42, TLV320AIC3204 0x18), GPIO7/8 (I2S WS/BCLK)
// and GPIO43/44 (I2S TX/RX). Free and touch-capable: GPIO1-4 (A0-A3) and GPIO9, plus GPIO5
// if we give up I2C.  (Wiring from ~/saltee/respeaker_sonar, memory respeaker-lite-sonar.)
//
// On ESP32-S3 a touch reading RISES with capacitance — opposite of the classic ESP32.
// esp32 core 3.3.11 uses the NG touch driver: the FSM free-runs, touchRead() returns the
// SMOOTH (IIR-filtered) latest value and never blocks. Timing is fixed at init, so the
// host changes it by writing RTC memory and rebooting (`c` / `g` below).
//
// Serial (115200, native USB CDC) out:
//   d <us> <v...>                    raw scan (one value per PADS[] entry, in order),
//                                    us = micros() at scan start
//   r <scans_per_s> <us_per_scan>    once a second
//   ! <text>                         notices (boot banner, config, baselines)
// Commands in:
//   s                    stream on/off
//   z                    re-zero baselines, print them
//   c <measure_us> <sleep_us>   set FSM timing, then reboot to apply
//   g <chg_times>               set charge times (sensitivity/duration), then reboot

// Every free touch-capable pin on this carrier, so the wiring identifies itself: touch a
// plate, see which channel moves. GPIO5 is also the XMOS I2C SDA — fine while the bench is
// silent (no I2C in use), revisit when audio needs the codec.
static const uint8_t PADS[] = { 1, 2, 3, 4, 5, 9 };   // GPIO == TOUCH channel number
static const uint8_t NPAD = sizeof(PADS);

// Survives a software reset, so the host can sweep timing without a reflash.
#define CFG_MAGIC 0x70616473  // "pads"
RTC_NOINIT_ATTR uint32_t cfg_magic, cfg_measure_us, cfg_sleep_us, cfg_chg;

static uint32_t base[NPAD];
static bool     streaming = true;
static uint32_t scans, t_report;

// Baseline: a hit only ever RAISES the value, so track downward instantly and upward
// slowly (1/256 per scan ~ 0.3 s at 900 Hz). Drift follows, hits stand out.
static inline void track(uint8_t i, uint32_t v) {
  if (v < base[i]) base[i] = v;
  else             base[i] += (v - base[i]) >> 8;
}

static void rezero() {
  for (uint8_t i = 0; i < NPAD; i++) {
    uint64_t acc = 0;
    for (uint8_t n = 0; n < 32; n++) { acc += touchRead(PADS[i]); delay(2); }
    base[i] = acc / 32;
  }
  Serial.print("! base");
  for (uint8_t i = 0; i < NPAD; i++) Serial.printf(" g%u=%u", PADS[i], base[i]);
  Serial.println();
}

static void reboot_with(uint32_t m, uint32_t s, uint32_t g) {
  cfg_magic = CFG_MAGIC; cfg_measure_us = m; cfg_sleep_us = s; cfg_chg = g;
  Serial.printf("! reboot measure=%u sleep=%u chg=%u\n", m, s, g);
  Serial.flush();
  ESP.restart();
}

void setup() {
  Serial.begin(115200);
  uint32_t t0 = millis();
  while (!Serial && millis() - t0 < 3000) delay(10);

  if (cfg_magic != CFG_MAGIC) { cfg_measure_us = 32; cfg_sleep_us = 256; cfg_chg = 500; cfg_magic = CFG_MAGIC; }
  // Both must precede the first touchRead() — the NG driver latches them at init.
  touchSetTiming((float)cfg_measure_us, cfg_sleep_us);
  touchSetConfig(cfg_chg, TOUCH_VOLT_LIM_L_0V5, TOUCH_VOLT_LIM_H_2V2);

  Serial.println("! pads v0 — ToastedDrums touch bench, GPIO 1/2/3/4 = TOUCH1-4, SILENT");
  Serial.printf("! cfg measure=%uus sleep=%uus chg=%u\n", cfg_measure_us, cfg_sleep_us, cfg_chg);
  rezero();
  t_report = millis();
}

void loop() {
  uint32_t us = micros();
  uint32_t v[NPAD];
  for (uint8_t i = 0; i < NPAD; i++) { v[i] = touchRead(PADS[i]); track(i, v[i]); }
  scans++;

  if (streaming) {
    Serial.printf("d %u", us);
    for (uint8_t i = 0; i < NPAD; i++) Serial.printf(" %u", v[i]);
    Serial.println();
  }

  uint32_t now = millis();
  if (now - t_report >= 1000) {
    uint32_t dt = now - t_report, n = scans ? scans : 1;
    Serial.printf("r %u %u\n", (scans * 1000) / dt, (dt * 1000) / n);
    scans = 0; t_report = now;
  }

  while (Serial.available()) {
    int c = Serial.read();
    if      (c == 's') { streaming = !streaming; Serial.printf("! stream %d\n", streaming); }
    else if (c == 'z') rezero();
    else if (c == 'c') { uint32_t m = Serial.parseInt(), s = Serial.parseInt(); if (m) reboot_with(m, s, cfg_chg); }
    else if (c == 'g') { uint32_t g = Serial.parseInt(); if (g) reboot_with(cfg_measure_us, cfg_sleep_us, g); }
  }
}
