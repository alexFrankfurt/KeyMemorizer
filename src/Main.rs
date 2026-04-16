use rdev::{Event, EventType, Key};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::thread;

use winapi::shared::minwindef::{DWORD, HKL, LOWORD};
use winapi::um::processthreadsapi::GetCurrentThreadId;
use winapi::um::winuser::{
    ActivateKeyboardLayout, AttachThreadInput, GetAsyncKeyState, GetForegroundWindow,
    GetClassNameW, GetKeyboardLayout, GetKeyboardLayoutList, GetKeyboardState, GetWindowTextW,
    GetWindowThreadProcessId, MapVirtualKeyExW, ToUnicodeEx, MAPVK_VK_TO_VSC,
};

fn debug_enabled() -> bool {
    // Off by default. Enable via env var (KEYMEM_DEBUG=1/true) or by creating:
    // D:\Dev\KeyMemorizer\debug.enabled
    if fs::metadata(r"D:\Dev\KeyMemorizer\debug.enabled").is_ok() {
        return true;
    }

    env::var("KEYMEM_DEBUG")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes" || v == "on"
        })
        .unwrap_or(false)
}

fn debug_verbose() -> bool {
    if !debug_enabled() {
        return false;
    }

    // Optional: create an empty file to enable per-key debug spam.
    // Default is non-verbose to avoid interfering with focus/typing.
    fs::metadata(r"D:\Dev\KeyMemorizer\debug.verbose").is_ok()
}

fn contains_cyrillic(s: &str) -> bool {
    s.chars().any(|c| {
        let u = c as u32;
        (0x0400..=0x04FF).contains(&u) || (0x0500..=0x052F).contains(&u)
    })
}

fn log_debug(line: &str) {
    if !debug_enabled() {
        return;
    }

    eprintln!("[DEBUG] {}", line);
}

#[derive(Debug, Clone)]
struct ForegroundState {
    hwnd: usize,
    hkl: usize,
}

static LAST_FOREGROUND: OnceLock<Mutex<ForegroundState>> = OnceLock::new();
static LAST_CYRILLIC_HKL: OnceLock<Mutex<usize>> = OnceLock::new();
static FALLBACK_CYRILLIC_HKL: OnceLock<usize> = OnceLock::new();
static FORCE_CYRILLIC_HKL: OnceLock<Mutex<Option<usize>>> = OnceLock::new();
static EXPECTED_LANG: OnceLock<Mutex<u16>> = OnceLock::new();

fn get_expected_lang() -> u16 {
    *EXPECTED_LANG
        .get_or_init(|| Mutex::new(0x0409))
        .lock()
        .unwrap()
}

fn set_expected_lang(lang_id: u16) {
    *EXPECTED_LANG
        .get_or_init(|| Mutex::new(0x0409))
        .lock()
        .unwrap() = lang_id;
}

fn mark_layout_switch() {
    // Treat the layout switch hotkey as a toggle between English and a Cyrillic layout.
    // This matches user expectation and fixes the "second switch stays Russian" issue.
    let current = get_expected_lang();
    if current == 0x0409 {
        // Switch to Cyrillic
        if let Some(hkl) = get_last_cyrillic_hkl() {
            let lang_id = LOWORD(hkl as usize as DWORD) as u16;
            set_expected_lang(lang_id);
        } else {
            // If no Cyrillic layout exists, keep English.
            set_expected_lang(0x0409);
        }
        // We'll force Cyrillic only if we detect the race.
        *FORCE_CYRILLIC_HKL.get_or_init(|| Mutex::new(None)).lock().unwrap() = None;
    } else {
        // Switch back to English
        set_expected_lang(0x0409);
        *FORCE_CYRILLIC_HKL.get_or_init(|| Mutex::new(None)).lock().unwrap() = None;
    }

    log_debug(&format!(
        "layout switch hotkey detected (expected_lang=0x{:04X})",
        get_expected_lang()
    ));
}

fn update_last_cyrillic_hkl(hkl: HKL) {
    let lang_id = LOWORD(hkl as usize as DWORD) as u16;
    if !is_cyrillic_layout(lang_id) {
        return;
    }
    let lock = LAST_CYRILLIC_HKL.get_or_init(|| Mutex::new(0));
    *lock.lock().unwrap() = hkl as usize;
}

fn maybe_confirm_expected_lang(lang_id: u16) {
    // If Windows reports a non-English HKL for the foreground window, treat it as authoritative.
    // Also, if we are in English, never keep a forced Cyrillic HKL.
    if lang_id != 0x0409 {
        set_expected_lang(lang_id);
        *FORCE_CYRILLIC_HKL.get_or_init(|| Mutex::new(None)).lock().unwrap() = None;
    } else if get_expected_lang() == 0x0409 {
        *FORCE_CYRILLIC_HKL.get_or_init(|| Mutex::new(None)).lock().unwrap() = None;
    }
}

fn get_forced_cyrillic_hkl() -> Option<HKL> {
    let lock = FORCE_CYRILLIC_HKL.get_or_init(|| Mutex::new(None));
    lock.lock().unwrap().map(|h| h as HKL)
}

fn set_forced_cyrillic_hkl(hkl: HKL) {
    *FORCE_CYRILLIC_HKL.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(hkl as usize);
    log_debug(&format!(
        "forcing Cyrillic HKL until OS confirms: 0x{:X} ({})",
        hkl as usize,
        get_layout_name(hkl)
    ));
}

fn get_last_cyrillic_hkl() -> Option<HKL> {
    let lock = LAST_CYRILLIC_HKL.get_or_init(|| Mutex::new(0));
    let v = *lock.lock().unwrap();
    if v == 0 {
        fallback_cyrillic_hkl()
    } else {
        Some(v as HKL)
    }
}

fn fallback_cyrillic_hkl() -> Option<HKL> {
    let v = *FALLBACK_CYRILLIC_HKL.get_or_init(|| {
        // Pick the first installed Cyrillic layout (RU/UA/BY). This gives us a stable
        // fallback even if we haven't yet observed a Cyrillic HKL in the foreground.
        let layouts = get_installed_keyboard_layouts();
        for hkl in layouts {
            let lang_id = LOWORD(hkl as usize as DWORD) as u16;
            if is_cyrillic_layout(lang_id) {
                return hkl as usize;
            }
        }
        0usize
    });
    if v == 0 { None } else { Some(v as HKL) }
}

fn maybe_log_foreground_change(hwnd: usize, hkl: HKL, title: &str, class_name: &str) {
    if !debug_enabled() {
        return;
    }

    let last = LAST_FOREGROUND.get_or_init(|| {
        Mutex::new(ForegroundState {
            hwnd: 0,
            hkl: 0,
        })
    });

    let mut guard = last.lock().unwrap();
    let hkl_usize = hkl as usize;
    if guard.hwnd != hwnd || guard.hkl != hkl_usize {
        guard.hwnd = hwnd;
        guard.hkl = hkl_usize;

        let lang_id = LOWORD(hkl as usize as DWORD) as u16;
        maybe_confirm_expected_lang(lang_id);
        update_last_cyrillic_hkl(hkl);

        log_debug(&format!(
            "FOREGROUND hwnd=0x{:X} class='{}' title='{}' hkl=0x{:X} lang={}",
            hwnd,
            class_name,
            title,
            hkl_usize,
            get_layout_name(hkl)
        ));
    }
}

fn initialize_layout_state() {
    let (hkl, hwnd, title, class_name) = unsafe { get_foreground_context() };
    let lang_id = LOWORD(hkl as usize as DWORD) as u16;

    set_expected_lang(lang_id);
    update_last_cyrillic_hkl(hkl);
    *FORCE_CYRILLIC_HKL.get_or_init(|| Mutex::new(None)).lock().unwrap() = None;

    if debug_enabled() {
        log_debug(&format!(
            "startup layout initialized hwnd=0x{:X} class='{}' title='{}' hkl=0x{:X} lang={}",
            hwnd,
            class_name,
            title,
            hkl as usize,
            get_layout_name(hkl)
        ));
    }
}

fn main() {
    let folder_path = r"D:\Dev\KeyMemorizer";
    fs::create_dir_all(folder_path).expect("Could not create folder");

    if debug_enabled() {
        eprintln!("[DEBUG] Enabled (console)");
        log_debug("KeyMemorizer started");
        if !debug_verbose() {
            log_debug("Tip: create D:\\Dev\\KeyMemorizer\\debug.verbose for per-key debug");
        }
    }

    println!("Monitoring started. Saving to D:\\Dev\\KeyMemorizer\\ai_history.log");
    println!("Multi-language keyboard support enabled!");
    println!("Switch keyboard layout (e.g., to Russian) and type - characters will be logged correctly.");
    
    // Print installed keyboard layouts for diagnostic
    print_installed_layouts();
    initialize_layout_state();

    // Track modifier key states
    let shift_pressed = Arc::new(AtomicBool::new(false));
    let alt_pressed = Arc::new(AtomicBool::new(false));
    let ctrl_pressed = Arc::new(AtomicBool::new(false));
    let win_pressed = Arc::new(AtomicBool::new(false));
    let caps_lock_on = Arc::new(AtomicBool::new(false));

    // Track Alt+Tab state
    let alt_tab_active = Arc::new(AtomicBool::new(false));

    let shift_clone = shift_pressed.clone();
    let alt_clone = alt_pressed.clone();
    let ctrl_clone = ctrl_pressed.clone();
    let win_clone = win_pressed.clone();
    let caps_clone = caps_lock_on.clone();
    let alt_tab_clone = alt_tab_active.clone();

    if debug_enabled() {
        if let Some(hkl) = fallback_cyrillic_hkl() {
            log_debug(&format!("fallback Cyrillic HKL selected: 0x{:X} ({})", hkl as usize, get_layout_name(hkl)));
        } else {
            log_debug("no fallback Cyrillic HKL found (RU/UA/BY not installed?)");
        }
    }

    // Start keyboard event listener
    thread::spawn(move || {
        let callback = move |event: Event| {
            match event.event_type {
                EventType::KeyPress(key) => {
                    // Handle modifier keys - these don't produce characters
                    match key {
                        // Track Windows keys (Win+Space is a common layout switch)
                        Key::Unknown(0x5B) | Key::Unknown(0x5C) => {
                            win_clone.store(true, Ordering::SeqCst);
                            return;
                        }
                        // Some layouts/IME paths can emit Unknown(0); ignore it.
                        Key::Unknown(0) => {
                            return;
                        }
                        Key::ShiftLeft | Key::ShiftRight => {
                            shift_clone.store(true, Ordering::SeqCst);

                            // Common Windows layout switch hotkeys include Alt+Shift and Ctrl+Shift.
                            if alt_clone.load(Ordering::SeqCst) || ctrl_clone.load(Ordering::SeqCst) {
                                mark_layout_switch();
                            }
                            return;
                        }
                        Key::Alt => {
                            alt_clone.store(true, Ordering::SeqCst);
                            alt_tab_clone.store(true, Ordering::SeqCst);

                            // Alt+Shift (if Shift already down)
                            if shift_clone.load(Ordering::SeqCst) {
                                mark_layout_switch();
                            }
                            return;
                        }
                        Key::CapsLock => {
                            let current = caps_clone.load(Ordering::SeqCst);
                            caps_clone.store(!current, Ordering::SeqCst);
                            return;
                        }
                        Key::ControlLeft | Key::ControlRight => {
                            ctrl_clone.store(true, Ordering::SeqCst);

                            // Ctrl+Shift (if Shift already down)
                            if shift_clone.load(Ordering::SeqCst) {
                                mark_layout_switch();
                            }
                            return; // Don't log control keys
                        }
                        _ => {}
                    }

                    // Check for Alt+Tab
                    if alt_clone.load(Ordering::SeqCst) {
                        if key == Key::Tab {
                            return; // Don't log Alt+Tab
                        } else {
                            alt_tab_clone.store(false, Ordering::SeqCst);
                        }
                    }

                    let ctrl_pressed = ctrl_clone.load(Ordering::SeqCst);
                    let alt_pressed = alt_clone.load(Ordering::SeqCst);
                    let win_pressed = win_clone.load(Ordering::SeqCst);

                    if ctrl_pressed || alt_pressed || win_pressed {
                        if win_pressed && key == Key::Space {
                            mark_layout_switch();
                        }
                        return;
                    }

                    // Handle special keys that don't produce characters
                    match key {
                        Key::Return => {
                            log_char("\n");
                        }
                        Key::Space => {
                            log_char(" ");
                        }
                        Key::Backspace => {
                            log_char("[BACKSPACE]");
                        }
                        Key::Tab => {
                            log_char("[TAB]");
                        }
                        Key::Escape => {
                            log_char("[ESC]");
                        }
                        // Handle function keys F1-F12
                        Key::F1 | Key::F2 | Key::F3 | Key::F4 | Key::F5 | Key::F6 
                        | Key::F7 | Key::F8 | Key::F9 | Key::F10 | Key::F11 | Key::F12 => {
                            log_char(&format!("[{:?}]", key));
                        }
                        _ => {
                            // Get active HKL once per keypress.
                            let (hkl, hwnd, title, class_name) = unsafe { get_foreground_context() };
                            let lang_id = LOWORD(hkl as usize as DWORD) as u16;

                            maybe_confirm_expected_lang(lang_id);

                            maybe_log_foreground_change(hwnd, hkl, &title, &class_name);

                            // Prefer OS-provided character from rdev, but override it when it looks
                            // inconsistent with the current layout (e.g., Russian HKL but Latin letters).
                            if let Some(name) = event.name.as_ref() {
                                if !name.is_empty() && !name.chars().any(|c| c.is_control()) {
                                    // If we already established that Cyrillic is intended but Windows still
                                    // reports English for the focused window, keep using the forced HKL.
                                    if lang_id == 0x0409 && get_expected_lang() != 0x0409 && is_basic_latin_letter(name) {
                                        if let Some(forced_hkl) = get_forced_cyrillic_hkl() {
                                            let is_shift = shift_clone.load(Ordering::SeqCst);
                                            let is_caps = caps_clone.load(Ordering::SeqCst);
                                            if let Some(fixed) = get_char_from_key_layout_with_hkl(
                                                &key,
                                                is_shift,
                                                is_caps,
                                                forced_hkl,
                                            ) {
                                                if !fixed.is_empty() && contains_cyrillic(&fixed) {
                                                    log_debug(&format!(
                                                        "forced-fix '{}' -> '{}' key={:?} forced_lang={}",
                                                        name,
                                                        fixed,
                                                        key,
                                                        get_layout_name(forced_hkl)
                                                    ));
                                                    log_char(&fixed);
                                                    return;
                                                }
                                            }
                                        }
                                    }

                                    let should_override = should_override_rdev_name(lang_id, name);
                                    if should_override {
                                        let is_shift = shift_clone.load(Ordering::SeqCst);
                                        let is_caps = caps_clone.load(Ordering::SeqCst);
                                        if let Some(fixed) = get_char_from_key_layout_with_hkl(&key, is_shift, is_caps, hkl) {
                                            if !fixed.is_empty() {
                                                if contains_cyrillic(&fixed) {
                                                    update_last_cyrillic_hkl(hkl);
                                                }
                                                log_debug(&format!(
                                                    "override rdev.name='{}' -> '{}' key={:?} lang={}",
                                                    name,
                                                    fixed,
                                                    key,
                                                    get_layout_name(hkl)
                                                ));
                                                log_char(&fixed);
                                                return;
                                            }
                                        }
                                    }

                                    // Race fix: when switching EN->Cyrillic while staying focused in the same
                                    // window, Windows may keep reporting English for a bit. If we *expect*
                                    // Cyrillic, try a Cyrillic HKL and, on success, force it until OS confirms.
                                    if lang_id == 0x0409 && get_expected_lang() != 0x0409 && is_basic_latin_letter(name) {
                                        if let Some(alt_hkl) = get_last_cyrillic_hkl() {
                                            let is_shift = shift_clone.load(Ordering::SeqCst);
                                            let is_caps = caps_clone.load(Ordering::SeqCst);
                                            if let Some(fixed) = get_char_from_key_layout_with_hkl(
                                                &key,
                                                is_shift,
                                                is_caps,
                                                alt_hkl,
                                            ) {
                                                if !fixed.is_empty() && contains_cyrillic(&fixed) {
                                                    update_last_cyrillic_hkl(alt_hkl);
                                                    // Confirm that we're now in Cyrillic mode.
                                                    set_expected_lang(LOWORD(alt_hkl as usize as DWORD) as u16);
                                                    set_forced_cyrillic_hkl(alt_hkl);
                                                    log_debug(&format!(
                                                        "race-fix '{}' -> '{}' key={:?} current_lang={} alt_lang={}",
                                                        name,
                                                        fixed,
                                                        key,
                                                        get_layout_name(hkl),
                                                        get_layout_name(alt_hkl)
                                                    ));
                                                    log_char(&fixed);
                                                    return;
                                                }
                                            }
                                        }
                                    }

                                    if debug_verbose() {
                                        log_debug(&format!(
                                            "rdev.name='{}' key={:?} lang={}",
                                            name,
                                            key,
                                            get_layout_name(hkl)
                                        ));
                                    }
                                    log_char(name);
                                    return;
                                }
                            }

                            // Fallback: translate using Windows API for proper layout support
                            let is_shift = shift_clone.load(Ordering::SeqCst);
                            let is_caps = caps_clone.load(Ordering::SeqCst);
                            
                            if let Some(char_result) = get_char_from_key_layout_with_hkl(&key, is_shift, is_caps, hkl) {
                                if !char_result.is_empty() {
                                    log_char(&char_result);
                                }
                            } else {
                                // Fallback to rdev's key name
                                let key_name = format!("{:?}", key);
                                if key_name.len() == 1 {
                                    let char_to_log = if is_shift != is_caps {
                                        key_name.to_uppercase()
                                    } else {
                                        key_name.to_lowercase()
                                    };
                                    log_char(&char_to_log);
                                } else {
                                    log_char(&format!("[{}]", key_name));
                                }
                            }
                        }
                    }
                }
                EventType::KeyRelease(key) => {
                    match key {
                        Key::Unknown(0x5B) | Key::Unknown(0x5C) => {
                            win_clone.store(false, Ordering::SeqCst);
                        }
                        Key::ShiftLeft | Key::ShiftRight => {
                            shift_clone.store(false, Ordering::SeqCst);
                        }
                        Key::Alt => {
                            alt_clone.store(false, Ordering::SeqCst);
                            alt_tab_clone.store(false, Ordering::SeqCst);
                        }
                        Key::ControlLeft | Key::ControlRight => {
                            ctrl_clone.store(false, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        };

        if let Err(e) = rdev::listen(callback) {
            eprintln!("Error starting keyboard listener: {:?}", e);
        }
    });

    // Keep main thread alive
    loop {
        thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Get the system-wide current keyboard layout
/// Returns the HKL (Handle to Keyboard Layout) for the system
#[allow(dead_code)]
fn get_system_keyboard_layout() -> HKL {
    unsafe {
        // ActivateKeyboardLayout with 0 returns the current system layout without changing it
        ActivateKeyboardLayout(std::ptr::null_mut(), 0)
    }
}

/// Get all installed keyboard layouts for the system
#[allow(dead_code)]
fn get_installed_keyboard_layouts() -> Vec<HKL> {
    unsafe {
        // First get the count of layouts
        let count = GetKeyboardLayoutList(0, std::ptr::null_mut());
        if count == 0 {
            return Vec::new();
        }
        
        let mut layouts: Vec<HKL> = vec![0 as HKL; count as usize];
        let actual_count = GetKeyboardLayoutList(count, layouts.as_mut_ptr());
        
        if actual_count == 0 {
            return Vec::new();
        }
        
        layouts.truncate(actual_count as usize);
        layouts
    }
}

/// Diagnostic: Print all installed keyboard layouts
fn print_installed_layouts() {
    unsafe {
        let count = GetKeyboardLayoutList(0, std::ptr::null_mut());
        if count == 0 {
            eprintln!("[DIAGNOSTIC] No keyboard layouts found!");
            return;
        }
        
        let mut layouts: Vec<HKL> = vec![0 as HKL; count as usize];
        let actual_count = GetKeyboardLayoutList(count, layouts.as_mut_ptr());
        
        eprintln!("[DIAGNOSTIC] Installed keyboard layouts ({}):", actual_count);
        for i in 0..actual_count as usize {
            let hkl = layouts[i];
            let lang_id = LOWORD(hkl as usize as DWORD);
            eprintln!("  HKL[{}] = 0x{:08X}, LANGID: 0x{:04X} ({})", 
                i, hkl as usize, lang_id, get_layout_name(hkl));
        }
    }
}

/// Get human-readable name for a keyboard layout HKL
#[allow(dead_code)]
fn get_layout_name(hkl: HKL) -> String {
    // HKL is a pointer, we need to cast it to get the language ID
    let lang_id = LOWORD(hkl as usize as u32) as u16;
    
    // Common language IDs
    let lang_name = match lang_id {
        0x0409 => "English (US)",
        0x0419 => "Russian",
        0x0407 => "German",
        0x040C => "French",
        0x0410 => "Italian",
        0x040A => "Spanish",
        0x0415 => "Polish",
        0x041F => "Turkish",
        0x041A => "Croatian/Serbian",
        0x0424 => "Slovenian",
        0x0422 => "Ukrainian",
        0x0423 => "Belarusian",
        _ => "Unknown",
    };
    
    format!("{} (0x{:04X})", lang_name, lang_id)
}

fn is_cyrillic_layout(lang_id: u16) -> bool {
    matches!(lang_id, 0x0419 | 0x0422 | 0x0423) // Russian, Ukrainian, Belarusian
}

fn is_basic_latin_letter(s: &str) -> bool {
    // Most common failure mode here is "ghbdtn" (latin letters) while Russian is active.
    // Keep the heuristic tight to avoid overriding valid symbols.
    s.chars().all(|c| c.is_ascii_alphabetic())
}

fn should_override_rdev_name(lang_id: u16, name: &str) -> bool {
    is_cyrillic_layout(lang_id) && is_basic_latin_letter(name)
}

/// Get the active HKL for the foreground window in a race-resistant way.
unsafe fn get_foreground_hkl() -> HKL {
    let hwnd = GetForegroundWindow();
    if !hwnd.is_null() {
        let foreground_tid = GetWindowThreadProcessId(hwnd, std::ptr::null_mut());
        if foreground_tid != 0 {
            let current_tid = GetCurrentThreadId();
            if AttachThreadInput(current_tid, foreground_tid, 1) != 0 {
                let attached_hkl = GetKeyboardLayout(0);
                AttachThreadInput(current_tid, foreground_tid, 0);
                return attached_hkl;
            }
            return GetKeyboardLayout(foreground_tid);
        }
    }
    GetKeyboardLayout(0)
}

unsafe fn get_foreground_window_text(hwnd: *mut winapi::shared::windef::HWND__) -> String {
    // 512 wchar buffer should be plenty for a window title.
    let mut buf: [u16; 512] = [0; 512];
    let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::new()
    }
}

unsafe fn get_foreground_class_name(hwnd: *mut winapi::shared::windef::HWND__) -> String {
    let mut buf: [u16; 256] = [0; 256];
    let len = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::new()
    }
}

/// Returns (hkl, hwnd_usize, title, class_name)
unsafe fn get_foreground_context() -> (HKL, usize, String, String) {
    let hwnd = GetForegroundWindow();
    if hwnd.is_null() {
        return (GetKeyboardLayout(0), 0, String::new(), String::new());
    }

    let title = get_foreground_window_text(hwnd);
    let class_name = get_foreground_class_name(hwnd);
    (get_foreground_hkl(), hwnd as usize, title, class_name)
}

/// Get character from key using Windows API for proper keyboard layout support
fn get_char_from_key_layout_with_hkl(key: &Key, is_shift: bool, is_caps: bool, hkl: HKL) -> Option<String> {
    let vk_code = key_to_vk(key)?;

    unsafe {
        // Get full keyboard state from OS (more reliable than synthesizing only modifiers)
        let mut keyboard_state: [u8; 256] = [0; 256];
        let got_state = GetKeyboardState(keyboard_state.as_mut_ptr());

        // Override with our tracked modifier intent
        keyboard_state[0x10] = if is_shift { 0x80 } else { keyboard_state[0x10] & 0x7F }; // VK_SHIFT
        keyboard_state[0x14] = if is_caps { 0x01 } else { keyboard_state[0x14] & 0xFE }; // VK_CAPITAL toggle

        // Keep Ctrl/Alt in sync via async state (AltGr often appears as Ctrl+Alt)
        if GetAsyncKeyState(0x11) != 0 {
            keyboard_state[0x11] |= 0x80; // VK_CONTROL
        }
        if GetAsyncKeyState(0x12) != 0 {
            keyboard_state[0x12] |= 0x80; // VK_MENU (Alt)
        }
        
        // Get scan code from virtual key using the active layout
        let scancode = MapVirtualKeyExW(vk_code as u32, MAPVK_VK_TO_VSC, hkl) as u32;

        log_debug(&format!(
            "Key={:?} vk=0x{:02X} sc=0x{:02X} hwnd=0x{:X} hkl=0x{:X} lang={} shift={} caps={} got_state={}",
            key,
            vk_code as u32,
            scancode,
            GetForegroundWindow() as usize,
            hkl as usize,
            get_layout_name(hkl),
            is_shift,
            is_caps,
            got_state
        ));
        
        // Buffer for Unicode characters
        let mut unicode_buf: [u16; 4] = [0; 4];
        
        // Call ToUnicodeEx to get the actual character(s) from the key press
        // We use the HKL of the window where typing is happening
        let chars_written = ToUnicodeEx(
            vk_code as u32,
            scancode,
            keyboard_state.as_ptr(),
            unicode_buf.as_mut_ptr(),
            unicode_buf.len() as i32,
            0,
            hkl,
        );
        
        if chars_written > 0 {
            let mut result = String::new();
            for i in 0..chars_written as usize {
                if let Some(c) = char::from_u32(unicode_buf[i] as u32) {
                    result.push(c);
                }
            }

            log_debug(&format!("ToUnicodeEx wrote {} -> '{}'", chars_written, result));
            
            if !result.is_empty() && !result.chars().any(|c| c.is_control()) {
                return Some(result);
            }
        } else if chars_written < 0 {
            // Dead key detected
            log_debug("ToUnicodeEx returned dead-key");
            return Some(String::new());
        } else {
            log_debug("ToUnicodeEx wrote 0");
        }
    }
    
    None
}

/// Convert rdev Key to Windows virtual key code
fn key_to_vk(key: &Key) -> Option<i32> {
    match key {
        // Letters
        Key::KeyA => Some(0x41),
        Key::KeyB => Some(0x42),
        Key::KeyC => Some(0x43),
        Key::KeyD => Some(0x44),
        Key::KeyE => Some(0x45),
        Key::KeyF => Some(0x46),
        Key::KeyG => Some(0x47),
        Key::KeyH => Some(0x48),
        Key::KeyI => Some(0x49),
        Key::KeyJ => Some(0x4A),
        Key::KeyK => Some(0x4B),
        Key::KeyL => Some(0x4C),
        Key::KeyM => Some(0x4D),
        Key::KeyN => Some(0x4E),
        Key::KeyO => Some(0x4F),
        Key::KeyP => Some(0x50),
        Key::KeyQ => Some(0x51),
        Key::KeyR => Some(0x52),
        Key::KeyS => Some(0x53),
        Key::KeyT => Some(0x54),
        Key::KeyU => Some(0x55),
        Key::KeyV => Some(0x56),
        Key::KeyW => Some(0x57),
        Key::KeyX => Some(0x58),
        Key::KeyY => Some(0x59),
        Key::KeyZ => Some(0x5A),
        
        // Numbers (top row)
        Key::Num1 => Some(0x31),
        Key::Num2 => Some(0x32),
        Key::Num3 => Some(0x33),
        Key::Num4 => Some(0x34),
        Key::Num5 => Some(0x35),
        Key::Num6 => Some(0x36),
        Key::Num7 => Some(0x37),
        Key::Num8 => Some(0x38),
        Key::Num9 => Some(0x39),
        Key::Num0 => Some(0x30),
        
        // Special keys
        Key::Space => Some(0x20),
        Key::Return => Some(0x0D),
        Key::Backspace => Some(0x08),
        Key::Tab => Some(0x09),
        Key::Escape => Some(0x1B),
        
        Key::Equal => Some(0xBB),
        Key::Minus => Some(0xBD),
        Key::LeftBracket => Some(0xDB),
        Key::RightBracket => Some(0xDD),
        Key::Quote => Some(0xDE),
        Key::Comma => Some(0xBC),
        Key::Slash => Some(0xBF),
        Key::BackSlash => Some(0xDC),
        Key::SemiColon => Some(0xBA),
        Key::BackQuote => Some(0xC0),
        
        Key::Delete => Some(0x2E),
        Key::Insert => Some(0x2D),
        Key::Home => Some(0x24),
        Key::End => Some(0x23),
        Key::PageUp => Some(0x21),
        Key::PageDown => Some(0x22),

        // On Windows, rdev may emit Unknown(<VK_CODE>) for system keys.
        // If it's in the VK range, we can pass it through.
        Key::Unknown(vk) if *vk <= 0xFF => Some(*vk as i32),

        _ => None,
    }
}

fn log_char(text: &str) {
    let path = r"D:\Dev\KeyMemorizer\ai_history.log";

    match text {
        "[BACKSPACE]" => {
            remove_last_logged_char(path);
            return;
        }
        "[TAB]" => append_logged_text(path, "\t"),
        _ if should_ignore_logged_token(text) => return,
        _ => append_logged_text(path, text),
    }
}

fn should_ignore_logged_token(text: &str) -> bool {
    matches!(
        text,
        "[ESC]"
            | "[LeftArrow]"
            | "[RightArrow]"
            | "[UpArrow]"
            | "[DownArrow]"
            | "[Home]"
            | "[End]"
            | "[Delete]"
            | "[Insert]"
            | "[PageUp]"
            | "[PageDown]"
            | "[F1]"
            | "[F2]"
            | "[F3]"
            | "[F4]"
            | "[F5]"
            | "[F6]"
            | "[F7]"
            | "[F8]"
            | "[F9]"
            | "[F10]"
            | "[F11]"
            | "[F12]"
    ) || text.starts_with("[Unknown(")
}

fn append_logged_text(path: &str, text: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();

    write!(file, "{}", text).ok();
    file.flush().ok();

    // Removed console output to prevent console from stealing focus
}

fn remove_last_logged_char(path: &str) {
    let mut file = match OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(_) => return,
    };

    let len = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => return,
    };

    if len == 0 {
        return;
    }

    let tail_len = len.min(4) as usize;
    if file.seek(SeekFrom::End(-(tail_len as i64))).is_err() {
        return;
    }

    let mut tail = vec![0; tail_len];
    if file.read_exact(&mut tail).is_err() {
        return;
    }

    let char_start = tail
        .iter()
        .rposition(|byte| (byte & 0b1100_0000) != 0b1000_0000)
        .unwrap_or(tail_len - 1);
    let bytes_to_trim = (tail_len - char_start) as u64;

    if file.set_len(len.saturating_sub(bytes_to_trim)).is_ok() {
        file.flush().ok();
    }
}
