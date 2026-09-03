// t1 — labeled analog bench for the four pad pins, A0..A3.
// Uses the board's own A0..A3 macros and prints what GPIO each one resolves to, so the
// A->GPIO mapping is confirmed by the core rather than taken from a table.
// Build with the BARE fqbn: esp32:esp32:XIAO_ESP32S3   (CDCOnBoot=cdc means CDC OFF)
// SILENT: no audio, no I2C, no XMOS.

static const uint8_t PINS[]  = { A0, A1, A2, A3 };
static const char   *NAMES[] = { "A0", "A1", "A2", "A3" };
static const uint8_t N = 4;

void setup() {
  Serial.begin(115200);
  delay(3000);  // outlast the USB CDC re-enumeration the DTR reset causes
  analogReadResolution(12);

  Serial.print("! pin map:");
  for (uint8_t i = 0; i < N; i++) {
    Serial.printf("  %s=GPIO%u", NAMES[i], PINS[i]);
  }
  Serial.println();
  Serial.println("! 12-bit analogRead, 0-4095");
}

void loop() {
  for (uint8_t i = 0; i < N; i++) {
    Serial.printf("%s=%4u  ", NAMES[i], analogRead(PINS[i]));
  }
  Serial.println();
  delay(250);
}
