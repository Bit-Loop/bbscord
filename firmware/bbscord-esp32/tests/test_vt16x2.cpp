#include <cassert>
#include <cstring>
#include <iostream>

#include "../include/vt16x2.h"

using bbscord_esp32::KeypadEncoder;
using bbscord_esp32::Vt16x2Screen;

static void feed_str(Vt16x2Screen& scr, const char* s) {
    scr.feed(reinterpret_cast<const uint8_t*>(s), std::strlen(s));
}

int main() {
    // 1. Startup banner + dirty flags.
    Vt16x2Screen scr;
    scr.write_banner("BBSCORD 1.0 16x2", "Waiting USB host");
    assert(std::strcmp(scr.row_text(0), "BBSCORD 1.0 16x2") == 0);
    assert(std::strcmp(scr.row_text(1), "Waiting USB host") == 0);
    assert(scr.is_dirty(0) && scr.is_dirty(1));
    scr.clear_dirty(0);
    scr.clear_dirty(1);
    assert(!scr.is_dirty(0) && !scr.is_dirty(1));

    // 2. Clear screen, strip DEC private modes + SGR + OSC + APC, paint 2 rows without scrolling.
    feed_str(scr,
        "\x1b[?1049h\x1b[?7l\x1b[?25l\x1b[2J"
        "\x1b]0;bbscord\x07"
        "\x1b_Gi=31;OK\x1b\\"
        "\x1b[1;1H\x1b[1;36m#general  1/4   \x1b[0m"
        "\x1b[2;1Hhi from esp32!!!");
    assert(std::strcmp(scr.row_text(0), "#general  1/4   ") == 0);
    assert(std::strcmp(scr.row_text(1), "hi from esp32!!!") == 0);
    assert(scr.is_dirty(0) && scr.is_dirty(1));

    // 3. Redrawing identical row 0 leaves dirty[0] false while changing row 1 marks dirty[1] true.
    scr.clear_dirty(0);
    scr.clear_dirty(1);
    feed_str(scr, "\x1b[1;1H#general  1/4   \x1b[2;1Hnew msg         ");
    assert(!scr.is_dirty(0));
    assert(scr.is_dirty(1));
    assert(std::strcmp(scr.row_text(1), "new msg         ") == 0);

    // 4. Erase to end of line (`CSI K`).
    feed_str(scr, "\x1b[2;4H\x1b[K");
    assert(std::strcmp(scr.row_text(1), "new             ") == 0);

    // 5. 6-button keypad debounce, directional auto-repeat, and non-repeating Enter/Esc.
    KeypadEncoder kp;
    assert(kp.poll(KeypadEncoder::Up, true, 0) == nullptr);    // inside debounce window
    assert(kp.poll(KeypadEncoder::Up, true, 20) == nullptr);   // still inside 35 ms debounce
    const char* up1 = kp.poll(KeypadEncoder::Up, true, 40);
    assert(up1 != nullptr && std::strcmp(up1, "\x1b[A") == 0);
    assert(kp.poll(KeypadEncoder::Up, true, 300) == nullptr);  // before 400 ms repeat delay
    const char* up2 = kp.poll(KeypadEncoder::Up, true, 445);   // first auto-repeat
    assert(up2 != nullptr && std::strcmp(up2, "\x1b[A") == 0);
    const char* up3 = kp.poll(KeypadEncoder::Up, true, 560);   // 110 ms repeat cadence
    assert(up3 != nullptr && std::strcmp(up3, "\x1b[A") == 0);

    // Enter emits once on press and does NOT auto-repeat.
    assert(kp.poll(KeypadEncoder::Enter, true, 100) == nullptr);
    const char* ent = kp.poll(KeypadEncoder::Enter, true, 140);
    assert(ent != nullptr && std::strcmp(ent, "\r") == 0);
    assert(kp.poll(KeypadEncoder::Enter, true, 1000) == nullptr);

    // Esc emits `\x1b` cleanly.
    assert(kp.poll(KeypadEncoder::Esc, true, 200) == nullptr);
    const char* esc = kp.poll(KeypadEncoder::Esc, true, 240);
    assert(esc != nullptr && std::strcmp(esc, "\x1b") == 0);

    std::cout << "bbscord-esp32 vt16x2 host unit tests PASS\n";
    return 0;
}
