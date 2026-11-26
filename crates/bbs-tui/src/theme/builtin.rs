//! The ten built-in themes of §8.2, each with a hand-tuned `[ansi16]` table.

use bbs_core::{
    Ansi16Gradient, Ansi16Palette, ColorGradient, RgbColor, ThemePalette, ThemeSource, ThemeSpec,
};

/// Slug of the built-in theme that follows the terminal's own 16-colour palette at every depth.
pub const TTY_SLUG: &str = "tty";

/// xterm/VGA default RGB for ANSI index `0..=15` (used only to give the `tty` theme sane RGB
/// values for code that reads `spec().palette` directly; `Theme` never emits them).
#[must_use]
pub const fn vga_rgb(idx: u8) -> RgbColor {
    const T: [u32; 16] = [
        0x00_0000, 0xAA_0000, 0x00_AA00, 0xAA_5500, 0x00_00AA, 0xAA_00AA, 0x00_AAAA, 0xAA_AAAA,
        0x55_5555, 0xFF_5555, 0x55_FF55, 0xFF_FF55, 0x55_55FF, 0xFF_55FF, 0x55_FFFF, 0xFF_FFFF,
    ];
    RgbColor::from_u32(T[(idx & 0x0F) as usize])
}

/// Lowercase ASCII slug: alphanumerics kept, every other run collapsed to a single `-`
/// (`"ANSI-BBS Royal"` → `"ansi-bbs-royal"`, `"btop/tokyo-night"` → `"btop-tokyo-night"`).
#[must_use]
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            dash = true;
        }
    }
    out
}

/// Compact RGB definition of a built-in (hex `0xRRGGBB`).
struct Def {
    name: &'static str,
    bg: u32,
    fg: u32,
    title: u32,
    hi: u32,
    sel_bg: u32,
    sel_fg: u32,
    inactive: u32,
    graph_text: u32,
    meter_bg: u32,
    misc: u32,
    /// sidebar / chat / voice / members.
    boxes: [u32; 4],
    div: u32,
    border: [u32; 3],
    latency: [u32; 3],
    vu_remote: [u32; 3],
    vu_mic: [u32; 3],
    unread: [u32; 3],
    selection: [u32; 3],
    free: [u32; 3],
    cached: [u32; 3],
    available: [u32; 3],
}

const fn c(hex: u32) -> RgbColor {
    RgbColor::from_u32(hex)
}

const fn g3(s: [u32; 3]) -> ColorGradient {
    ColorGradient::new_3stop(c(s[0]), c(s[1]), c(s[2]))
}

const fn a3(s: [u8; 3]) -> Ansi16Gradient {
    Ansi16Gradient::new_3stop(s[0], s[1], s[2])
}

fn build(d: &Def, ansi16: Ansi16Palette) -> ThemeSpec {
    ThemeSpec {
        name: d.name.to_string(),
        source: ThemeSource::BuiltIn,
        palette: ThemePalette {
            main_bg: Some(c(d.bg)),
            main_fg: c(d.fg),
            title: c(d.title),
            hi_fg: c(d.hi),
            selected_bg: c(d.sel_bg),
            selected_fg: c(d.sel_fg),
            inactive_fg: c(d.inactive),
            graph_text: c(d.graph_text),
            meter_bg: c(d.meter_bg),
            proc_misc: c(d.misc),
            cpu_box: c(d.boxes[0]),
            mem_box: c(d.boxes[1]),
            net_box: c(d.boxes[2]),
            proc_box: c(d.boxes[3]),
            div_line: c(d.div),
            temp_gradient: g3(d.latency),
            cpu_gradient: g3(d.border),
            free_gradient: g3(d.free),
            cached_gradient: g3(d.cached),
            available_gradient: g3(d.available),
            used_gradient: g3(d.unread),
            download_gradient: g3(d.vu_remote),
            upload_gradient: g3(d.vu_mic),
            process_gradient: g3(d.selection),
            ansi16: Some(ansi16),
        },
    }
}

/// Map an indexed gradient to its VGA RGB equivalent.
fn vga_grad(g: Ansi16Gradient) -> ColorGradient {
    ColorGradient {
        start: vga_rgb(g.start),
        mid: g.mid.map(vga_rgb),
        end: g.end.map(vga_rgb),
    }
}

/// The `tty` theme: ANSI-16 only (§8.2), RGB mirrors VGA defaults of the same indices.
fn tty() -> ThemeSpec {
    let a = Ansi16Palette::tty_default();
    ThemeSpec {
        name: TTY_SLUG.to_string(),
        source: ThemeSource::BuiltIn,
        palette: ThemePalette {
            main_bg: a.main_bg.map(vga_rgb),
            main_fg: vga_rgb(a.main_fg),
            title: vga_rgb(a.title),
            hi_fg: vga_rgb(a.hi_fg),
            selected_bg: vga_rgb(a.selected_bg),
            selected_fg: vga_rgb(a.selected_fg),
            inactive_fg: vga_rgb(a.inactive_fg),
            graph_text: vga_rgb(a.graph_text),
            meter_bg: vga_rgb(a.meter_bg),
            proc_misc: vga_rgb(a.proc_misc),
            cpu_box: vga_rgb(a.cpu_box),
            mem_box: vga_rgb(a.mem_box),
            net_box: vga_rgb(a.net_box),
            proc_box: vga_rgb(a.proc_box),
            div_line: vga_rgb(a.div_line),
            temp_gradient: vga_grad(a.temp_gradient),
            cpu_gradient: vga_grad(a.cpu_gradient),
            free_gradient: vga_grad(a.free_gradient),
            cached_gradient: vga_grad(a.cached_gradient),
            available_gradient: vga_grad(a.available_gradient),
            used_gradient: vga_grad(a.used_gradient),
            download_gradient: vga_grad(a.download_gradient),
            upload_gradient: vga_grad(a.upload_gradient),
            process_gradient: vga_grad(a.process_gradient),
            ansi16: Some(a),
        },
    }
}

fn ansi_bbs_royal() -> ThemeSpec {
    build(
        &Def {
            name: "ANSI-BBS Royal",
            bg: 0x00_00AA,
            fg: 0xAA_AAAA,
            title: 0xFF_FF55,
            hi: 0x55_FFFF,
            sel_bg: 0xAA_AAAA,
            sel_fg: 0x00_0000,
            inactive: 0x00_AAAA,
            graph_text: 0xFF_FFFF,
            meter_bg: 0x00_0055,
            misc: 0xFF_55FF,
            boxes: [0x55_55FF, 0x55_FFFF, 0x55_FF55, 0xFF_55FF],
            div: 0x55_55FF,
            border: [0x55_55FF, 0x55_FFFF, 0xFF_FFFF],
            latency: [0x55_FF55, 0xFF_FF55, 0xFF_5555],
            vu_remote: [0x55_FF55, 0xFF_FF55, 0xFF_5555],
            vu_mic: [0x55_FFFF, 0xFF_55FF, 0xFF_5555],
            unread: [0x55_FFFF, 0xFF_FF55, 0xFF_5555],
            selection: [0x00_AAAA, 0x55_FFFF, 0xFF_FFFF],
            free: [0x55_FF55, 0x55_FFFF, 0xFF_FF55],
            cached: [0x55_FFFF, 0xFF_55FF, 0xFF_FFFF],
            available: [0xFF_FF55, 0xFF_55FF, 0x55_FF55],
        },
        Ansi16Palette {
            main_bg: Some(4),
            main_fg: 7,
            title: 11,
            hi_fg: 14,
            selected_bg: 7,
            selected_fg: 0,
            inactive_fg: 6,
            graph_text: 15,
            meter_bg: 0,
            proc_misc: 13,
            cpu_box: 12,
            mem_box: 14,
            net_box: 10,
            proc_box: 13,
            div_line: 12,
            temp_gradient: a3([10, 11, 9]),
            cpu_gradient: a3([12, 14, 15]),
            free_gradient: a3([10, 14, 11]),
            cached_gradient: a3([14, 13, 15]),
            available_gradient: a3([11, 13, 10]),
            used_gradient: a3([14, 11, 9]),
            download_gradient: a3([10, 11, 9]),
            upload_gradient: a3([14, 13, 9]),
            process_gradient: a3([6, 14, 15]),
        },
    )
}

/// Monochrome-hue CRT themes share one ANSI-16 shape: `lo` dim hue, `hi` bright hue.
fn mono_ansi16(lo: u8, hi: u8) -> Ansi16Palette {
    Ansi16Palette {
        main_bg: Some(0),
        main_fg: hi,
        title: 15,
        hi_fg: 15,
        selected_bg: lo,
        selected_fg: 0,
        inactive_fg: lo,
        graph_text: hi,
        meter_bg: 8,
        proc_misc: 15,
        cpu_box: lo,
        mem_box: hi,
        net_box: lo,
        proc_box: lo,
        div_line: lo,
        temp_gradient: a3([lo, hi, 15]),
        cpu_gradient: a3([lo, hi, 15]),
        free_gradient: a3([lo, hi, 15]),
        cached_gradient: a3([hi, 15, hi]),
        available_gradient: a3([hi, lo, 15]),
        used_gradient: a3([lo, hi, 15]),
        download_gradient: a3([lo, hi, 15]),
        upload_gradient: a3([lo, hi, 15]),
        process_gradient: a3([lo, lo, hi]),
    }
}

fn amber_phosphor() -> ThemeSpec {
    build(
        &Def {
            name: "Amber Phosphor",
            bg: 0x0D_0700,
            fg: 0xFF_B000,
            title: 0xFF_D27F,
            hi: 0xFF_D27F,
            sel_bg: 0x7A_4A00,
            sel_fg: 0xFF_E6A8,
            inactive: 0x9A_6800,
            graph_text: 0xFF_C640,
            meter_bg: 0x2A_1A00,
            misc: 0xFF_E6A8,
            boxes: [0xB0_7800, 0xFF_B000, 0xD9_9500, 0x9A_6800],
            div: 0x5C_3A00,
            border: [0x7A_4A00, 0xFF_B000, 0xFF_E6A8],
            latency: [0xB0_7800, 0xFF_B000, 0xFF_F0C8],
            vu_remote: [0x5C_3A00, 0xFF_B000, 0xFF_E6A8],
            vu_mic: [0x7A_4A00, 0xFF_C640, 0xFF_F0C8],
            unread: [0x8A_5F00, 0xFF_B000, 0xFF_E6A8],
            selection: [0x3D_2600, 0x5C_3A00, 0x7A_4A00],
            free: [0xCC_8A00, 0xFF_B000, 0xFF_D27F],
            cached: [0xB0_7800, 0xFF_C640, 0xFF_E6A8],
            available: [0xD9_9500, 0xFF_D27F, 0xFF_F0C8],
        },
        mono_ansi16(3, 11),
    )
}

fn p1_green() -> ThemeSpec {
    build(
        &Def {
            name: "P1 Green CRT",
            bg: 0x00_0A00,
            fg: 0x33_FF33,
            title: 0xB6_FFB6,
            hi: 0xB6_FFB6,
            sel_bg: 0x0B_5D0B,
            sel_fg: 0xD8_FFD8,
            inactive: 0x1F_8F1F,
            graph_text: 0x66_FF66,
            meter_bg: 0x05_2605,
            misc: 0x99_FF99,
            boxes: [0x1F_BF1F, 0x33_FF33, 0x28_D828, 0x17_9017],
            div: 0x0B_5D0B,
            border: [0x0B_5D0B, 0x33_FF33, 0xB6_FFB6],
            latency: [0x17_9017, 0x33_FF33, 0xD8_FFD8],
            vu_remote: [0x0B_5D0B, 0x33_FF33, 0xB6_FFB6],
            vu_mic: [0x1F_BF1F, 0x66_FF66, 0xD8_FFD8],
            unread: [0x1F_8F1F, 0x33_FF33, 0xB6_FFB6],
            selection: [0x05_2605, 0x0B_5D0B, 0x1F_8F1F],
            free: [0x28_D828, 0x66_FF66, 0xB6_FFB6],
            cached: [0x1F_BF1F, 0x99_FF99, 0xD8_FFD8],
            available: [0x33_FF33, 0x80_FF80, 0xCC_FFCC],
        },
        mono_ansi16(2, 10),
    )
}

/// Baseline for the dark "editor" themes (black bg, grey selection, bright accents).
fn dark_ansi16() -> Ansi16Palette {
    Ansi16Palette {
        main_bg: Some(0),
        main_fg: 7,
        title: 15,
        hi_fg: 11,
        selected_bg: 8,
        selected_fg: 15,
        inactive_fg: 8,
        graph_text: 15,
        meter_bg: 8,
        proc_misc: 13,
        cpu_box: 12,
        mem_box: 14,
        net_box: 10,
        proc_box: 13,
        div_line: 8,
        temp_gradient: a3([10, 11, 9]),
        cpu_gradient: a3([12, 14, 15]),
        free_gradient: a3([14, 12, 13]),
        cached_gradient: a3([10, 14, 12]),
        available_gradient: a3([11, 3, 13]),
        used_gradient: a3([14, 11, 9]),
        download_gradient: a3([14, 12, 13]),
        upload_gradient: a3([10, 11, 9]),
        process_gradient: a3([8, 7, 15]),
    }
}

fn gruvbox() -> ThemeSpec {
    build(
        &Def {
            name: "Gruvbox",
            bg: 0x28_2828,
            fg: 0xEB_DBB2,
            title: 0xFA_BD2F,
            hi: 0xFE_8019,
            sel_bg: 0x50_4945,
            sel_fg: 0xFB_F1C7,
            inactive: 0x92_8374,
            graph_text: 0xD5_C4A1,
            meter_bg: 0x3C_3836,
            misc: 0xD3_869B,
            boxes: [0x83_A598, 0x8E_C07C, 0xB8_BB26, 0xD3_869B],
            div: 0x50_4945,
            border: [0xB8_BB26, 0xFA_BD2F, 0xFB_4934],
            latency: [0xB8_BB26, 0xFA_BD2F, 0xFB_4934],
            vu_remote: [0x8E_C07C, 0xFA_BD2F, 0xFB_4934],
            vu_mic: [0x83_A598, 0xD3_869B, 0xFB_4934],
            unread: [0x8E_C07C, 0xFE_8019, 0xFB_4934],
            selection: [0x50_4945, 0x66_5C54, 0x7C_6F64],
            free: [0x8E_C07C, 0x83_A598, 0xD3_869B],
            cached: [0xB8_BB26, 0xFA_BD2F, 0xFE_8019],
            available: [0x83_A598, 0x8E_C07C, 0xFA_BD2F],
        },
        Ansi16Palette {
            title: 11,
            hi_fg: 3,
            cpu_gradient: a3([10, 11, 9]),
            download_gradient: a3([14, 11, 9]),
            upload_gradient: a3([12, 13, 9]),
            used_gradient: a3([14, 3, 9]),
            cached_gradient: a3([10, 11, 3]),
            available_gradient: a3([12, 14, 11]),
            ..dark_ansi16()
        },
    )
}

fn tokyo_night() -> ThemeSpec {
    build(
        &Def {
            name: "Tokyo Night",
            bg: 0x1A_1B26,
            fg: 0xC0_CAF5,
            title: 0x7A_A2F7,
            hi: 0x7D_CFFF,
            sel_bg: 0x33_467C,
            sel_fg: 0xC0_CAF5,
            inactive: 0x56_5F89,
            graph_text: 0xA9_B1D6,
            meter_bg: 0x29_2E42,
            misc: 0xFF_9E64,
            boxes: [0x7A_A2F7, 0xBB_9AF7, 0x9E_CE6A, 0x7D_CFFF],
            div: 0x3B_4261,
            border: [0x7A_A2F7, 0xBB_9AF7, 0xF7_768E],
            latency: [0x9E_CE6A, 0xE0_AF68, 0xF7_768E],
            vu_remote: [0x73_DACA, 0x7A_A2F7, 0xBB_9AF7],
            vu_mic: [0x9E_CE6A, 0xE0_AF68, 0xF7_768E],
            unread: [0x7D_CFFF, 0xE0_AF68, 0xF7_768E],
            selection: [0x29_2E42, 0x33_467C, 0x3D_59A1],
            free: [0x7D_CFFF, 0x7A_A2F7, 0xBB_9AF7],
            cached: [0x9E_CE6A, 0x73_DACA, 0x2A_C3DE],
            available: [0xE0_AF68, 0xFF_9E64, 0xF7_768E],
        },
        Ansi16Palette {
            title: 12,
            hi_fg: 14,
            selected_bg: 4,
            proc_misc: 11,
            cpu_box: 12,
            mem_box: 13,
            net_box: 10,
            proc_box: 14,
            cpu_gradient: a3([12, 13, 9]),
            download_gradient: a3([14, 12, 13]),
            cached_gradient: a3([10, 6, 14]),
            available_gradient: a3([11, 3, 9]),
            process_gradient: a3([8, 4, 12]),
            ..dark_ansi16()
        },
    )
}

fn catppuccin_mocha() -> ThemeSpec {
    build(
        &Def {
            name: "Catppuccin Mocha",
            bg: 0x1E_1E2E,
            fg: 0xCD_D6F4,
            title: 0xB4_BEFE,
            hi: 0xF5_C2E7,
            sel_bg: 0x45_475A,
            sel_fg: 0xCD_D6F4,
            inactive: 0x6C_7086,
            graph_text: 0xBA_C2DE,
            meter_bg: 0x31_3244,
            misc: 0xFA_B387,
            boxes: [0x89_B4FA, 0xCB_A6F7, 0xA6_E3A1, 0x94_E2D5],
            div: 0x45_475A,
            border: [0x89_B4FA, 0xCB_A6F7, 0xF3_8BA8],
            latency: [0xA6_E3A1, 0xF9_E2AF, 0xF3_8BA8],
            vu_remote: [0x94_E2D5, 0x89_B4FA, 0xCB_A6F7],
            vu_mic: [0xA6_E3A1, 0xF9_E2AF, 0xF3_8BA8],
            unread: [0x89_DCEB, 0xFA_B387, 0xF3_8BA8],
            selection: [0x31_3244, 0x45_475A, 0x58_5B70],
            free: [0x89_B4FA, 0xB4_BEFE, 0xCB_A6F7],
            cached: [0xA6_E3A1, 0x94_E2D5, 0x89_DCEB],
            available: [0xF9_E2AF, 0xFA_B387, 0xEB_A0AC],
        },
        Ansi16Palette {
            title: 12,
            hi_fg: 13,
            proc_misc: 11,
            cpu_box: 12,
            mem_box: 13,
            net_box: 10,
            proc_box: 14,
            cpu_gradient: a3([12, 13, 9]),
            free_gradient: a3([12, 13, 5]),
            cached_gradient: a3([10, 14, 6]),
            available_gradient: a3([11, 3, 13]),
            ..dark_ansi16()
        },
    )
}

fn dracula() -> ThemeSpec {
    build(
        &Def {
            name: "Dracula",
            bg: 0x28_2A36,
            fg: 0xF8_F8F2,
            title: 0xBD_93F9,
            hi: 0xFF_79C6,
            sel_bg: 0x44_475A,
            sel_fg: 0xF8_F8F2,
            inactive: 0x62_72A4,
            graph_text: 0xF8_F8F2,
            meter_bg: 0x34_3746,
            misc: 0xF1_FA8C,
            boxes: [0xBD_93F9, 0x8B_E9FD, 0x50_FA7B, 0xFF_79C6],
            div: 0x44_475A,
            border: [0xBD_93F9, 0x8B_E9FD, 0x50_FA7B],
            latency: [0x50_FA7B, 0xF1_FA8C, 0xFF_5555],
            vu_remote: [0x8B_E9FD, 0xBD_93F9, 0xFF_79C6],
            vu_mic: [0x50_FA7B, 0xF1_FA8C, 0xFF_5555],
            unread: [0x8B_E9FD, 0xFF_B86C, 0xFF_5555],
            selection: [0x34_3746, 0x44_475A, 0x62_72A4],
            free: [0x8B_E9FD, 0xBD_93F9, 0xFF_79C6],
            cached: [0x50_FA7B, 0x8B_E9FD, 0xBD_93F9],
            available: [0xF1_FA8C, 0xFF_B86C, 0xFF_79C6],
        },
        Ansi16Palette {
            main_fg: 15,
            title: 12,
            hi_fg: 13,
            proc_misc: 11,
            cpu_gradient: a3([13, 14, 10]),
            used_gradient: a3([14, 3, 9]),
            ..dark_ansi16()
        },
    )
}

fn nord() -> ThemeSpec {
    build(
        &Def {
            name: "Nord",
            bg: 0x2E_3440,
            fg: 0xD8_DEE9,
            title: 0xEC_EFF4,
            hi: 0x88_C0D0,
            sel_bg: 0x43_4C5E,
            sel_fg: 0xEC_EFF4,
            inactive: 0x61_6E88,
            graph_text: 0xE5_E9F0,
            meter_bg: 0x3B_4252,
            misc: 0xEB_CB8B,
            boxes: [0x5E_81AC, 0x81_A1C1, 0xA3_BE8C, 0x88_C0D0],
            div: 0x4C_566A,
            border: [0x5E_81AC, 0x88_C0D0, 0x8F_BCBB],
            latency: [0xA3_BE8C, 0xEB_CB8B, 0xBF_616A],
            vu_remote: [0x8F_BCBB, 0x88_C0D0, 0x81_A1C1],
            vu_mic: [0xA3_BE8C, 0xEB_CB8B, 0xBF_616A],
            unread: [0x88_C0D0, 0xD0_8770, 0xBF_616A],
            selection: [0x3B_4252, 0x43_4C5E, 0x4C_566A],
            free: [0x81_A1C1, 0x88_C0D0, 0xB4_8EAD],
            cached: [0xA3_BE8C, 0x8F_BCBB, 0x88_C0D0],
            available: [0xEB_CB8B, 0xD0_8770, 0xB4_8EAD],
        },
        Ansi16Palette {
            hi_fg: 14,
            proc_misc: 11,
            cpu_box: 12,
            mem_box: 6,
            net_box: 2,
            proc_box: 14,
            cpu_gradient: a3([12, 14, 6]),
            download_gradient: a3([14, 6, 12]),
            free_gradient: a3([12, 14, 13]),
            cached_gradient: a3([10, 6, 14]),
            used_gradient: a3([14, 3, 9]),
            ..dark_ansi16()
        },
    )
}

fn cyberpunk_bbs() -> ThemeSpec {
    build(
        &Def {
            name: "Cyberpunk BBS",
            bg: 0x0A_0014,
            fg: 0xE0_E0FF,
            title: 0x00_F0FF,
            hi: 0xF3_E600,
            sel_bg: 0x3A_0066,
            sel_fg: 0xFF_FFFF,
            inactive: 0x6B_5B95,
            graph_text: 0xE0_E0FF,
            meter_bg: 0x1E_0A33,
            misc: 0xFF_00A0,
            boxes: [0xFF_00A0, 0x00_F0FF, 0xF3_E600, 0xA0_40FF],
            div: 0x3A_1A5C,
            border: [0xFF_00A0, 0x80_00FF, 0x00_F0FF],
            latency: [0x00_FF9C, 0xF3_E600, 0xFF_0055],
            vu_remote: [0x00_F0FF, 0x80_00FF, 0xFF_00A0],
            vu_mic: [0x00_FF9C, 0xF3_E600, 0xFF_0055],
            unread: [0x00_F0FF, 0xF3_E600, 0xFF_00A0],
            selection: [0x1E_0A33, 0x3A_0066, 0x5A_00A0],
            free: [0x00_F0FF, 0xA0_40FF, 0xFF_00A0],
            cached: [0x00_FF9C, 0x00_F0FF, 0xA0_40FF],
            available: [0xF3_E600, 0xFF_8C00, 0xFF_00A0],
        },
        Ansi16Palette {
            main_fg: 15,
            title: 14,
            hi_fg: 11,
            selected_bg: 5,
            cpu_box: 13,
            mem_box: 14,
            net_box: 11,
            proc_box: 12,
            div_line: 5,
            cpu_gradient: a3([13, 5, 14]),
            download_gradient: a3([14, 5, 13]),
            used_gradient: a3([14, 11, 13]),
            process_gradient: a3([5, 13, 15]),
            ..dark_ansi16()
        },
    )
}

/// The ten built-in themes (§8.2), in switcher order.
#[must_use]
pub fn builtin_specs() -> Vec<ThemeSpec> {
    vec![
        ansi_bbs_royal(),
        amber_phosphor(),
        p1_green(),
        tty(),
        gruvbox(),
        tokyo_night(),
        catppuccin_mocha(),
        dracula(),
        nord(),
        cyberpunk_bbs(),
    ]
}

/// Look up a built-in by name or slug, case-insensitively (`"amber-phosphor"`, `"Amber Phosphor"`).
#[must_use]
pub fn builtin_by_name(name: &str) -> Option<ThemeSpec> {
    let want = slug(name);
    if want.is_empty() {
        return None;
    }
    builtin_specs().into_iter().find(|t| slug(&t.name) == want)
}

/// `true` for the built-in `tty` theme, which must emit `Color::Indexed(0..=15)` at every depth.
#[must_use]
pub fn is_tty_spec(spec: &ThemeSpec) -> bool {
    spec.source == ThemeSource::BuiltIn && slug(&spec.name) == TTY_SLUG
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_builtins_with_spec_colors() {
        let specs = builtin_specs();
        assert_eq!(specs.len(), 10);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "ANSI-BBS Royal",
                "Amber Phosphor",
                "P1 Green CRT",
                "tty",
                "Gruvbox",
                "Tokyo Night",
                "Catppuccin Mocha",
                "Dracula",
                "Nord",
                "Cyberpunk BBS"
            ]
        );
        // (bg, fg, hi, border start/mid/end) from the §8.2 table.
        let table: [(u32, u32, u32, [u32; 3]); 10] = [
            (0x0000AA, 0xAAAAAA, 0x55FFFF, [0x5555FF, 0x55FFFF, 0xFFFFFF]),
            (0x0D0700, 0xFFB000, 0xFFD27F, [0x7A4A00, 0xFFB000, 0xFFE6A8]),
            (0x000A00, 0x33FF33, 0xB6FFB6, [0x0B5D0B, 0x33FF33, 0xB6FFB6]),
            (0x000000, 0xAAAAAA, 0x55FFFF, [0x0000AA, 0x00AAAA, 0xFFFFFF]),
            (0x282828, 0xEBDBB2, 0xFE8019, [0xB8BB26, 0xFABD2F, 0xFB4934]),
            (0x1A1B26, 0xC0CAF5, 0x7DCFFF, [0x7AA2F7, 0xBB9AF7, 0xF7768E]),
            (0x1E1E2E, 0xCDD6F4, 0xF5C2E7, [0x89B4FA, 0xCBA6F7, 0xF38BA8]),
            (0x282A36, 0xF8F8F2, 0xFF79C6, [0xBD93F9, 0x8BE9FD, 0x50FA7B]),
            (0x2E3440, 0xD8DEE9, 0x88C0D0, [0x5E81AC, 0x88C0D0, 0x8FBCBB]),
            (0x0A0014, 0xE0E0FF, 0xF3E600, [0xFF00A0, 0x8000FF, 0x00F0FF]),
        ];
        for (s, (bg, fg, hi, b)) in specs.iter().zip(table) {
            let p = &s.palette;
            assert_eq!(p.main_bg, Some(RgbColor::from_u32(bg)), "{}", s.name);
            assert_eq!(p.main_fg, RgbColor::from_u32(fg), "{}", s.name);
            assert_eq!(p.hi_fg, RgbColor::from_u32(hi), "{}", s.name);
            assert_eq!(p.cpu_gradient, g3(b), "{}", s.name);
            assert!(p.ansi16.is_some(), "{} lacks ansi16", s.name);
            assert_eq!(s.source, ThemeSource::BuiltIn);
        }
        assert_eq!(specs[0].palette.title, RgbColor::from_u32(0xFFFF55));
        assert_eq!(specs[3].palette.ansi16, Some(Ansi16Palette::tty_default()));
    }

    #[test]
    fn ansi16_indices_in_range() {
        for s in builtin_specs() {
            let a = s.palette.ansi16.clone().unwrap_or_default();
            for v in [
                a.main_fg,
                a.title,
                a.hi_fg,
                a.selected_bg,
                a.selected_fg,
                a.inactive_fg,
                a.graph_text,
                a.meter_bg,
                a.proc_misc,
                a.cpu_box,
                a.mem_box,
                a.net_box,
                a.proc_box,
                a.div_line,
            ] {
                assert!(v <= 15, "{}", s.name);
            }
            // Legibility: text never equals the background index.
            assert_ne!(a.main_bg, Some(a.main_fg), "{}", s.name);
            assert_ne!(a.selected_bg, a.selected_fg, "{}", s.name);
        }
    }

    #[test]
    fn lookup_by_name_or_slug() {
        assert_eq!(slug("ANSI-BBS Royal"), "ansi-bbs-royal");
        assert_eq!(slug("  P1 Green CRT "), "p1-green-crt");
        assert!(builtin_by_name("amber-phosphor").is_some());
        assert!(builtin_by_name("AMBER PHOSPHOR").is_some());
        assert!(builtin_by_name("catppuccin_mocha").is_some());
        assert!(builtin_by_name("TTY").is_some());
        assert!(builtin_by_name("").is_none());
        assert!(builtin_by_name("nope").is_none());
        assert!(is_tty_spec(&builtin_by_name("tty").unwrap_or_default()));
        assert!(!is_tty_spec(&builtin_by_name("nord").unwrap_or_default()));
    }
}
