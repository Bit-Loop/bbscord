#pragma once

#include <cstdint>
#include <cstring>

namespace bbscord_esp32 {

static constexpr uint8_t LCD_COLS = 16;
static constexpr uint8_t LCD_ROWS = 2;

/// Compact zero-allocation VT100/ANSI terminal parser targeting `bbscord --geometry 16x2 --charset ascii`.
///
/// Handles cursor positioning (`CSI r;c H` / `CSI H`), screen/line erase (`CSI 2 J`, `CSI K`),
/// strips SGR (`CSI ... m`), DEC private modes (`CSI ? ... h/l`), OSC (`ESC ] ... BEL/ST`),
/// and APC/DCS sequences cleanly while maintaining per-row dirty flags for flicker-free I2C LCD updates.
class Vt16x2Screen {
public:
    Vt16x2Screen() { clear(); }

    void clear() {
        for (uint8_t r = 0; r < LCD_ROWS; ++r) {
            std::memset(cells_[r], ' ', LCD_COLS);
            cells_[r][LCD_COLS] = '\0';
            dirty_[r] = true;
        }
        row_ = 0;
        col_ = 0;
        state_ = State::Normal;
        csi_len_ = 0;
    }

    void write_banner(const char* line0, const char* line1) {
        clear();
        for (uint8_t c = 0; c < LCD_COLS && line0[c] != '\0'; ++c) {
            cells_[0][c] = sanitize_hd44780(static_cast<uint8_t>(line0[c]));
        }
        for (uint8_t c = 0; c < LCD_COLS && line1[c] != '\0'; ++c) {
            cells_[1][c] = sanitize_hd44780(static_cast<uint8_t>(line1[c]));
        }
        dirty_[0] = true;
        dirty_[1] = true;
    }

    void feed(const uint8_t* data, size_t len) {
        for (size_t i = 0; i < len; ++i) {
            feed_byte(data[i]);
        }
    }

    void feed_byte(uint8_t b) {
        switch (state_) {
            case State::Normal:
                handle_normal(b);
                break;
            case State::Esc:
                handle_esc(b);
                break;
            case State::Csi:
                handle_csi(b);
                break;
            case State::Osc:
                if (b == 0x07) {
                    state_ = State::Normal;
                } else if (b == 0x1B) {
                    state_ = State::StringEsc;
                }
                break;
            case State::StringBody:
                if (b == 0x1B) {
                    state_ = State::StringEsc;
                }
                break;
            case State::StringEsc:
                if (b == '\\') {
                    state_ = State::Normal;
                } else if (b != 0x1B) {
                    state_ = State::StringBody;
                }
                break;
        }
    }

    const char* row_text(uint8_t r) const { return cells_[r < LCD_ROWS ? r : 0]; }
    bool is_dirty(uint8_t r) const { return r < LCD_ROWS && dirty_[r]; }
    void clear_dirty(uint8_t r) {
        if (r < LCD_ROWS) dirty_[r] = false;
    }
    uint8_t cursor_row() const { return row_; }
    uint8_t cursor_col() const { return col_; }

private:
    enum class State : uint8_t { Normal, Esc, Csi, Osc, StringBody, StringEsc };

    static char sanitize_hd44780(uint8_t b) {
        return (b >= 0x20 && b <= 0x7E) ? static_cast<char>(b) : ' ';
    }

    void put_char(char ch) {
        if (row_ < LCD_ROWS && col_ < LCD_COLS) {
            if (cells_[row_][col_] != ch) {
                cells_[row_][col_] = ch;
                dirty_[row_] = true;
            }
            if (col_ + 1 < LCD_COLS) {
                ++col_;
            }
        }
    }

    void handle_normal(uint8_t b) {
        if (b == 0x1B) {
            state_ = State::Esc;
            return;
        }
        if (b == '\r') {
            col_ = 0;
            return;
        }
        if (b == '\n') {
            if (row_ + 1 < LCD_ROWS) {
                ++row_;
            }
            return;
        }
        if (b == '\b') {
            if (col_ > 0) --col_;
            return;
        }
        if (b >= 0x20 && b <= 0x7E) {
            put_char(static_cast<char>(b));
        }
    }

    void handle_esc(uint8_t b) {
        if (b == '[') {
            state_ = State::Csi;
            csi_len_ = 0;
        } else if (b == ']') {
            state_ = State::Osc;
        } else if (b == 'P' || b == '_' || b == '^' || b == 'X') {
            state_ = State::StringBody;
        } else {
            state_ = State::Normal;
        }
    }

    void handle_csi(uint8_t b) {
        if (b >= 0x40 && b <= 0x7E) {
            dispatch_csi(static_cast<char>(b));
            state_ = State::Normal;
            csi_len_ = 0;
            return;
        }
        if (static_cast<size_t>(csi_len_) + 1 < sizeof(csi_buf_)) {
            csi_buf_[csi_len_++] = static_cast<char>(b);
        }
    }

    uint16_t parse_param(uint8_t index, uint16_t default_val) const {
        uint8_t cur_idx = 0;
        uint16_t val = 0;
        bool has_digits = false;
        for (uint8_t i = 0; i < csi_len_; ++i) {
            char c = csi_buf_[i];
            if (c == ';') {
                if (cur_idx == index) {
                    return has_digits ? val : default_val;
                }
                ++cur_idx;
                val = 0;
                has_digits = false;
            } else if (c >= '0' && c <= '9') {
                if (cur_idx == index) {
                    val = static_cast<uint16_t>(val * 10 + static_cast<uint16_t>(c - '0'));
                    has_digits = true;
                }
            }
        }
        if (cur_idx == index && has_digits) {
            return val;
        }
        return default_val;
    }

    void dispatch_csi(char final_byte) {
        // Ignore private-mode sequences (e.g. `?1049h`, `?2026h`, `?25l`, `?7l`) cleanly.
        if (csi_len_ > 0 && (csi_buf_[0] == '?' || csi_buf_[0] == '>' || csi_buf_[0] == '<')) {
            return;
        }
        switch (final_byte) {
            case 'H':
            case 'f': {
                uint16_t r = parse_param(0, 1);
                uint16_t c = parse_param(1, 1);
                row_ = (r == 0) ? 0 : static_cast<uint8_t>((r - 1) < LCD_ROWS ? (r - 1) : (LCD_ROWS - 1));
                col_ = (c == 0) ? 0 : static_cast<uint8_t>((c - 1) < LCD_COLS ? (c - 1) : (LCD_COLS - 1));
                break;
            }
            case 'J': {
                uint16_t mode = parse_param(0, 0);
                if (mode == 2 || mode == 3) {
                    clear();
                } else if (mode == 0) {
                    erase_line_from(row_, col_);
                    for (uint8_t r = row_ + 1; r < LCD_ROWS; ++r) {
                        erase_line_from(r, 0);
                    }
                }
                break;
            }
            case 'K': {
                uint16_t mode = parse_param(0, 0);
                if (mode == 0) {
                    erase_line_from(row_, col_);
                } else if (mode == 2) {
                    erase_line_from(row_, 0);
                }
                break;
            }
            case 'G': {
                uint16_t c = parse_param(0, 1);
                col_ = (c == 0) ? 0 : static_cast<uint8_t>((c - 1) < LCD_COLS ? (c - 1) : (LCD_COLS - 1));
                break;
            }
            case 'A': {
                uint16_t n = parse_param(0, 1);
                row_ = (n >= row_) ? 0 : static_cast<uint8_t>(row_ - n);
                break;
            }
            case 'B': {
                uint16_t n = parse_param(0, 1);
                uint16_t next = static_cast<uint16_t>(row_) + n;
                row_ = static_cast<uint8_t>(next < LCD_ROWS ? next : (LCD_ROWS - 1));
                break;
            }
            case 'C': {
                uint16_t n = parse_param(0, 1);
                uint16_t next = static_cast<uint16_t>(col_) + n;
                col_ = static_cast<uint8_t>(next < LCD_COLS ? next : (LCD_COLS - 1));
                break;
            }
            case 'D': {
                uint16_t n = parse_param(0, 1);
                col_ = (n >= col_) ? 0 : static_cast<uint8_t>(col_ - n);
                break;
            }
            default:
                // SGR (`m`) and unsupported CSI sequences are silently consumed.
                break;
        }
    }

    void erase_line_from(uint8_t r, uint8_t start_col) {
        if (r >= LCD_ROWS) return;
        for (uint8_t c = start_col; c < LCD_COLS; ++c) {
            if (cells_[r][c] != ' ') {
                cells_[r][c] = ' ';
                dirty_[r] = true;
            }
        }
    }

    char cells_[LCD_ROWS][LCD_COLS + 1];
    bool dirty_[LCD_ROWS];
    uint8_t row_ = 0;
    uint8_t col_ = 0;
    State state_ = State::Normal;
    char csi_buf_[32] = {};
    uint8_t csi_len_ = 0;
};

/// 6-button debounced hardware keypad producing the exact ANSI keys consumed by `bbscord` 16x2 Micro Mode:
/// `UP` (`ESC [ A`), `DOWN` (`ESC [ B`), `LEFT` (`ESC [ D`), `RIGHT` (`ESC [ C`), `ENTER` (`\r`), `ESC` (`\x1b`).
class KeypadEncoder {
public:
    static constexpr uint8_t NUM_KEYS = 6;
    static constexpr uint32_t DEBOUNCE_MS = 35;
    static constexpr uint32_t REPEAT_DELAY_MS = 400;
    static constexpr uint32_t REPEAT_INTERVAL_MS = 110;

    enum Key : uint8_t { Up = 0, Down = 1, Left = 2, Right = 3, Enter = 4, Esc = 5 };

    static const char* ansi_sequence(uint8_t key) {
        switch (key) {
            case Up:    return "\x1b[A";
            case Down:  return "\x1b[B";
            case Left:  return "\x1b[D";
            case Right: return "\x1b[C";
            case Enter: return "\r";
            case Esc:   return "\x1b";
            default:    return "";
        }
    }

    /// Poll a single key's active-high `pressed` state at monotonic timestamp `now_ms`.
    /// Returns the ANSI byte sequence to send over serial, or `nullptr` if no event fired.
    const char* poll(uint8_t key, bool pressed, uint32_t now_ms) {
        if (key >= NUM_KEYS) return nullptr;
        ButtonState& s = states_[key];
        if (pressed != s.raw) {
            s.raw = pressed;
            s.last_edge_ms = now_ms;
        }
        if ((now_ms - s.last_edge_ms) < DEBOUNCE_MS) {
            return nullptr;
        }
        if (s.raw != s.stable) {
            s.stable = s.raw;
            if (s.stable) {
                s.last_emit_ms = now_ms;
                s.repeating = false;
                return ansi_sequence(key);
            }
            return nullptr;
        }
        // Auto-repeat directional keys while held down (never repeat Enter/Esc).
        if (s.stable && key <= Right) {
            uint32_t threshold = s.repeating ? REPEAT_INTERVAL_MS : REPEAT_DELAY_MS;
            if ((now_ms - s.last_emit_ms) >= threshold) {
                s.last_emit_ms = now_ms;
                s.repeating = true;
                return ansi_sequence(key);
            }
        }
        return nullptr;
    }

private:
    struct ButtonState {
        bool raw = false;
        bool stable = false;
        bool repeating = false;
        uint32_t last_edge_ms = 0;
        uint32_t last_emit_ms = 0;
    };

    ButtonState states_[NUM_KEYS] = {};
};

} // namespace bbscord_esp32
