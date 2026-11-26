#include <Arduino.h>
#include <Wire.h>
#include "vt16x2.h"

// Default GPIO wiring per ESP32 family (override via build_flags if using a custom board).
#if defined(CONFIG_IDF_TARGET_ESP32C3)
  #ifndef BBSCORD_I2C_SDA
    #define BBSCORD_I2C_SDA 8
  #endif
  #ifndef BBSCORD_I2C_SCL
    #define BBSCORD_I2C_SCL 9
  #endif
  static constexpr uint8_t KEY_PINS[6] = {2, 3, 4, 5, 6, 7};
#elif defined(CONFIG_IDF_TARGET_ESP32C6)
  #ifndef BBSCORD_I2C_SDA
    #define BBSCORD_I2C_SDA 6
  #endif
  #ifndef BBSCORD_I2C_SCL
    #define BBSCORD_I2C_SCL 7
  #endif
  static constexpr uint8_t KEY_PINS[6] = {0, 1, 2, 3, 4, 5};
#elif defined(CONFIG_IDF_TARGET_ESP32S3)
  #ifndef BBSCORD_I2C_SDA
    #define BBSCORD_I2C_SDA 8
  #endif
  #ifndef BBSCORD_I2C_SCL
    #define BBSCORD_I2C_SCL 9
  #endif
  static constexpr uint8_t KEY_PINS[6] = {4, 5, 6, 7, 15, 16};
#else
  // Classic ESP32 (Xtensa LX6)
  #ifndef BBSCORD_I2C_SDA
    #define BBSCORD_I2C_SDA 21
  #endif
  #ifndef BBSCORD_I2C_SCL
    #define BBSCORD_I2C_SCL 22
  #endif
  static constexpr uint8_t KEY_PINS[6] = {13, 14, 25, 26, 27, 33};
#endif

namespace {

// Self-contained PCF8574 I2C 4-bit HD44780 16x2 LCD driver (zero external library deps).
class Pcf8574Lcd {
public:
    bool begin() {
        Wire.begin(BBSCORD_I2C_SDA, BBSCORD_I2C_SCL);
        Wire.setClock(100000);
        // Auto-probe the two standard PCF8574 / PCF8574A I2C backpack addresses.
        if (probe(0x27)) {
            addr_ = 0x27;
        } else if (probe(0x3F)) {
            addr_ = 0x3F;
        } else {
            return false;
        }
        present_ = true;
        delay(50);
        // HD44780 4-bit initialization sequence.
        write_nibble(0x30);
        delay(5);
        write_nibble(0x30);
        delayMicroseconds(150);
        write_nibble(0x30);
        write_nibble(0x20); // 4-bit mode

        command(0x28); // 2 lines, 5x8 font
        command(0x0C); // Display ON, cursor OFF, blink OFF
        command(0x06); // Entry mode: increment cursor, no shift
        command(0x01); // Clear display
        delay(2);
        return true;
    }

    void write_row(uint8_t row, const char* text16) {
        if (!present_ || row >= 2) return;
        command(row == 0 ? 0x80 : 0xC0);
        for (uint8_t i = 0; i < 16; ++i) {
            char c = text16[i];
            write_byte(static_cast<uint8_t>(c ? c : ' '), RS_BIT);
        }
    }

private:
    static constexpr uint8_t RS_BIT = 0x01;
    static constexpr uint8_t EN_BIT = 0x04;
    static constexpr uint8_t BL_BIT = 0x08;

    bool probe(uint8_t addr) {
        Wire.beginTransmission(addr);
        return Wire.endTransmission() == 0;
    }

    void expander_write(uint8_t data) {
        Wire.beginTransmission(addr_);
        Wire.write(data | BL_BIT);
        Wire.endTransmission();
    }

    void pulse_enable(uint8_t data) {
        expander_write(data | EN_BIT);
        delayMicroseconds(1);
        expander_write(data & ~EN_BIT);
        delayMicroseconds(45);
    }

    void write_nibble(uint8_t high_nibble_and_flags) {
        expander_write(high_nibble_and_flags);
        pulse_enable(high_nibble_and_flags);
    }

    void write_byte(uint8_t val, uint8_t mode) {
        write_nibble((val & 0xF0) | mode);
        write_nibble(((val << 4) & 0xF0) | mode);
    }

    void command(uint8_t cmd) { write_byte(cmd, 0); }

    uint8_t addr_ = 0x27;
    bool present_ = false;
};

Pcf8574Lcd g_lcd;
bbscord_esp32::Vt16x2Screen g_screen;
bbscord_esp32::KeypadEncoder g_keypad;

void flush_lcd() {
    for (uint8_t r = 0; r < bbscord_esp32::LCD_ROWS; ++r) {
        if (g_screen.is_dirty(r)) {
            g_lcd.write_row(r, g_screen.row_text(r));
            g_screen.clear_dirty(r);
        }
    }
}

} // namespace

void setup() {
    Serial.begin(115200);
    for (uint8_t i = 0; i < bbscord_esp32::KeypadEncoder::NUM_KEYS; ++i) {
        pinMode(KEY_PINS[i], INPUT_PULLUP);
    }
    g_lcd.begin();
    g_screen.write_banner("BBSCORD 1.0 16x2", "Waiting USB host");
    flush_lcd();
}

void loop() {
    uint8_t rx_buf[64];
    int avail = Serial.available();
    if (avail > 0) {
        size_t to_read = static_cast<size_t>(avail);
        if (to_read > sizeof(rx_buf)) to_read = sizeof(rx_buf);
        size_t n = Serial.readBytes(rx_buf, to_read);
        g_screen.feed(rx_buf, n);
        flush_lcd();
    }

    uint32_t now = millis();
    for (uint8_t k = 0; k < bbscord_esp32::KeypadEncoder::NUM_KEYS; ++k) {
        bool pressed = (digitalRead(KEY_PINS[k]) == LOW);
        if (const char* seq = g_keypad.poll(k, pressed, now)) {
            Serial.print(seq);
        }
    }
}
