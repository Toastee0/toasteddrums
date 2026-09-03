// tb4 — classic ESP32 touch bench. Pads on P2, P4, P33, P32.
//
// Verified channel map (soc/touch_sensor_channel.h, core 3.3.11):
//   T0=GPIO4  T1=GPIO0  T2=GPIO2  T3=GPIO15 T4=GPIO13
//   T5=GPIO12 T6=GPIO14 T7=GPIO27 T8=GPIO33 T9=GPIO32
// So P2=T2, P4=T0, P33=T8, P32=T9.
//
// POLARITY (operator-observed on real plates, and matches esp32-hal-touch.h):
//   the value FALLS when touched -- roughly 100 counts below the resting average.
//   "for ESP32 values close to 0 mean touch detected".
//
// duration_ms: default 5.0 gave far too little range on these heavily-loaded plates.
// 20.0 is what produced the usable ~100-count swing. Costs update rate (~10ms), which is
// right at the drum budget, so this is the knob to trade if latency needs improving.
//
// NOTE P2 is a strapping pin and on most ESP32 devkits it also drives the onboard LED,
// which loads the channel. If P2 reads oddly compared to P4/P32/P33, that is why --
// P13, P14 and P27 are clean alternatives with no strapping or LED duty.

static const float   DURATION_MS = 20.0f;
static const uint8_t PINS[]  = { 2, 4, 33, 32 };
static const char   *NAMES[] = { "P2", "P4", "P33", "P32" };
static const uint8_t N = 4;

void setup() {
  Serial.begin(115200);
  delay(1000);
  touchSetConfig(DURATION_MS, TOUCH_VOLT_LIM_L_0V5, TOUCH_VOLT_LIM_H_1V7);
  Serial.println();
  Serial.printf("! tb4  P2=T2 P4=T0 P33=T8 P32=T9  duration_ms=%.1f\n", DURATION_MS);
  Serial.println("! value FALLS when touched (~100 counts). per window: min/mean/max");
}

void loop() {
  uint32_t lo[N], hi[N]; uint64_t acc[N];
  for (uint8_t i = 0; i < N; i++) { lo[i] = 0xFFFFFFFF; hi[i] = 0; acc[i] = 0; }

  uint32_t n = 0, t0 = millis();
  while (millis() - t0 < 100) {
    for (uint8_t i = 0; i < N; i++) {
      uint32_t v = touchRead(PINS[i]);
      if (v < lo[i]) lo[i] = v;
      if (v > hi[i]) hi[i] = v;
      acc[i] += v;
    }
    n++;
  }

  Serial.printf("w n=%lu", (unsigned long)n);
  for (uint8_t i = 0; i < N; i++) {
    Serial.printf("  %s=%lu/%lu/%lu", NAMES[i],
                  (unsigned long)lo[i], (unsigned long)(acc[i] / (n ? n : 1)), (unsigned long)hi[i]);
  }
  Serial.println();
}
