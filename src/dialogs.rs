//! Dialogs module for Screen Time Manager
//! Contains passcode verification and settings dialog implementations

use std::cell::RefCell;
use std::mem::zeroed;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering as AtomicOrdering};
use std::sync::Mutex;
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject,
            DrawTextW, DT_CALCRECT, Ellipse, EndPaint, FillRect, GetMonitorInfoW, GetStockObject, GetSysColorBrush,
            InvalidateRect, LineTo, MonitorFromWindow, MoveToEx, ScreenToClient, SelectObject, SetBkColor,
            SetBkMode, SetTextColor, SetViewportOrgEx, DT_CENTER, DT_END_ELLIPSIS, DT_SINGLELINE, DT_VCENTER,
            DT_WORDBREAK, FW_BOLD, FW_NORMAL, GetSysColor, HDC, HGDIOBJ, MONITORINFO, COLOR_BTNFACE,
            MONITOR_DEFAULTTOPRIMARY, PAINTSTRUCT, PS_SOLID, SYSTEM_FONT, TRANSPARENT,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::*,
            Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled, SetFocus, VK_ESCAPE, VK_RETURN},
            Shell::{Shell_NotifyIconW, NIF_TIP, NIM_MODIFY},
            WindowsAndMessaging::*,
        },
    },
};

use crate::constants::*;
use crate::database::{get_setting, set_setting, set_telegram_config, get_telegram_config, set_discord_config, get_discord_config, WEEKDAY_KEYS, get_pause_used_today, get_pause_config, get_pause_log_today, is_pause_enabled, is_idle_enabled, get_idle_timeout_minutes};
use crate::dpi::scale;
use crate::i18n::{self, Language};
use crate::passcode_guard::{self, Attempt};

// ============================================================================
// Shared dialog plumbing
// ============================================================================

thread_local! {
    /// Fonts (and other GDI objects) a dialog created for its own controls,
    /// by owning window, deleted when that window is gone (WM_NCDESTROY) -
    /// they used to be leaked on every open.
    static DIALOG_GDI: RefCell<Vec<(usize, HGDIOBJ)>> = const { RefCell::new(Vec::new()) };
}

/// Remember `obj` for deletion when `owner` is destroyed; returns it.
fn track_gdi<T: Copy + Into<HGDIOBJ>>(owner: HWND, obj: T) -> T {
    let handle: HGDIOBJ = obj.into();
    if !handle.is_invalid() {
        DIALOG_GDI.with(|v| v.borrow_mut().push((owner.0 as usize, handle)));
    }
    obj
}

/// Delete everything `track_gdi` recorded for `owner`. Call from
/// WM_NCDESTROY, which comes after the child controls using the fonts are
/// destroyed.
unsafe fn release_gdi(owner: HWND) {
    let objects: Vec<HGDIOBJ> = DIALOG_GDI.with(|v| {
        let mut v = v.borrow_mut();
        let (mine, rest): (Vec<_>, Vec<_>) = v.drain(..).partition(|(o, _)| *o == owner.0 as usize);
        *v = rest;
        mine.into_iter().map(|(_, obj)| obj).collect()
    });
    for obj in objects {
        let _ = DeleteObject(obj);
    }
}

/// Select a stock font into `hdc`, so the fonts a paint function created
/// can actually be deleted - DeleteObject fails on a font that is still
/// selected, and the wizard pages used to leak one that way on every repaint
/// (every 2 seconds while waiting for the Telegram message).
unsafe fn deselect_font(hdc: HDC) {
    SelectObject(hdc, GetStockObject(SYSTEM_FONT));
}

/// Register a dialog window class with a solid background. The class (and
/// its brush) is registered once per process; on later opens registration
/// fails because the class exists, and the brush made for that attempt is
/// deleted instead of leaked.
unsafe fn register_dialog_class(class_name: PCWSTR, proc: WNDPROC, background: u32) {
    let hinstance = GetModuleHandleW(None).unwrap_or_default();
    let brush = CreateSolidBrush(COLORREF(background));
    let wnd_class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: proc,
        hInstance: hinstance.into(),
        lpszClassName: class_name,
        hbrBackground: brush,
        hCursor: LoadCursorW(None, IDC_ARROW).ok().unwrap_or_default(),
        ..zeroed()
    };
    if RegisterClassW(&wnd_class) == 0 {
        let _ = DeleteObject(brush);
    }
}

/// Run a nested message loop until `dlg` is destroyed.
///
/// The dialogs used to end their loop with PostQuitMessage, and the setup
/// wizards posted it on WM_CLOSE without destroying the window, so each
/// click on a wizard's X ended one more loop: the wizard's, then Settings',
/// then the app's main loop - and the app exited without its quit marker,
/// which the watchdog reported as tampering. Now a dialog's loop ends only
/// when its own window is gone, and a WM_QUIT that arrives meanwhile (a real
/// shutdown of the app) is handed on to the enclosing loop instead of being
/// swallowed.
unsafe fn run_modal_loop(dlg: HWND) {
    let mut msg: MSG = zeroed();
    while IsWindow(dlg).as_bool() {
        match GetMessageW(&mut msg, None, 0, 0).0 {
            0 => {
                PostQuitMessage(msg.wParam.0 as i32);
                break;
            }
            -1 => break,
            _ => {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

thread_local! {
    /// Owners that `begin_modal` disabled, by the dialog they wait for.
    static MODAL_OWNERS: RefCell<Vec<(usize, usize)>> = const { RefCell::new(Vec::new()) };
}

/// Make `dlg` modal to `owner`: the owner takes no input until `end_modal`.
/// The setup wizards used to leave Settings clickable behind them, so its
/// Save could write the old fields back over what the wizard had just saved.
/// An owner that is already disabled (it is itself waiting for another
/// dialog) is left to whoever disabled it.
unsafe fn begin_modal(dlg: HWND, owner: HWND) {
    if owner.0.is_null() || !IsWindow(owner).as_bool() || !IsWindowEnabled(owner).as_bool() {
        return;
    }
    let _ = EnableWindow(owner, false);
    MODAL_OWNERS.with(|m| m.borrow_mut().push((dlg.0 as usize, owner.0 as usize)));
}

/// Give back the owner `begin_modal` disabled for `dlg`, once; later calls
/// do nothing. Called before the dialog is destroyed where possible, so
/// Windows activates the owner rather than some other program's window.
unsafe fn end_modal(dlg: HWND) {
    let owner = MODAL_OWNERS.with(|m| {
        let mut m = m.borrow_mut();
        let i = m.iter().position(|(d, _)| *d == dlg.0 as usize)?;
        Some(m.remove(i).1)
    });
    if let Some(owner) = owner {
        let owner = HWND(owner as *mut std::ffi::c_void);
        if IsWindow(owner).as_bool() {
            let _ = EnableWindow(owner, true);
        }
    }
}

/// Close a modal dialog (Finish, Cancel, its close button).
unsafe fn close_modal(dlg: HWND) {
    end_modal(dlg);
    let _ = DestroyWindow(dlg);
}

/// Show `dlg` modal to `owner` and wait until it is gone. However it ends -
/// Finish, Cancel, the close button, the owner being destroyed, or the app
/// quitting (WM_QUIT) - the owner gets its input back and, if it still
/// exists, the focus.
unsafe fn run_owned_modal(dlg: HWND, owner: HWND) {
    begin_modal(dlg, owner);
    let _ = ShowWindow(dlg, SW_SHOW);
    let _ = SetForegroundWindow(dlg);
    run_modal_loop(dlg);
    end_modal(dlg);
    if IsWindow(dlg).as_bool() {
        // The loop ended on WM_QUIT: the app is closing.
        let _ = DestroyWindow(dlg);
    }
    if !owner.0.is_null() && IsWindow(owner).as_bool() {
        let _ = SetForegroundWindow(owner);
    }
}

// Control IDs for settings dialog
const ID_SETTINGS_BASE: i32 = 2000;
const ID_SETTINGS_SAVE: i32 = 2100;
const ID_SETTINGS_CANCEL: i32 = 2101;
const ID_CURRENT_PASSCODE: i32 = 2110;
const ID_NEW_PASSCODE: i32 = 2111;
const ID_CONFIRM_PASSCODE: i32 = 2112;
const ID_LANGUAGE_COMBO: i32 = 2120;
const ID_TELEGRAM_WIZARD: i32 = 2130;
const ID_DISCORD_WIZARD: i32 = 2140;

// Settings dialog state
static mut SETTINGS_EDIT_HANDLES: Option<SettingsEditHandles> = None;

struct SettingsEditHandles {
    daily_limits: [HWND; 7],
    warning1_minutes: HWND,
    warning1_message: HWND,
    warning2_minutes: HWND,
    warning2_message: HWND,
    blocking_message: HWND,
    current_passcode: HWND,
    new_passcode: HWND,
    confirm_passcode: HWND,
    rotating_pin_enabled: HWND,
    // Telegram settings
    telegram_token: HWND,
    telegram_chat_id: HWND,
    telegram_enabled: HWND,
    // Discord settings
    discord_token: HWND,
    discord_channel_id: HWND,
    discord_admin_user_id: HWND,
    discord_enabled: HWND,
    // Lock screen timeout
    lock_screen_timeout: HWND,
    // Idle detection settings
    idle_enabled: HWND,
    idle_timeout_minutes: HWND,
    // Language setting
    language: HWND,
    // Pause settings
    pause_daily_budget: HWND,
    pause_max_duration: HWND,
    pause_cooldown: HWND,
}

/// What a passcode prompt is for, so the dialog can say so (one per tray
/// menu item that asks for the passcode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasscodeAction {
    Stats,
    Settings,
    Extend15,
    Extend45,
    FreeTime,
    EndFreeTime,
    Quit,
}

impl PasscodeAction {
    fn prompt_key(self) -> &'static str {
        match self {
            PasscodeAction::Stats => "passcode.action.stats",
            PasscodeAction::Settings => "passcode.action.settings",
            PasscodeAction::Extend15 => "passcode.action.extend_15",
            PasscodeAction::Extend45 => "passcode.action.extend_45",
            PasscodeAction::FreeTime => "passcode.action.free_time",
            PasscodeAction::EndFreeTime => "passcode.action.end_free_time",
            PasscodeAction::Quit => "passcode.action.quit",
        }
    }
}

/// Ask for the passcode, naming the action it will allow. Fails closed: if
/// the passcode can't be read the dialog says so and nothing is allowed
/// (it used to let everything through when the read failed). Attempts are
/// limited by `passcode_guard`.
pub unsafe fn verify_passcode_for_action(parent_hwnd: HWND, action: PasscodeAction) -> bool {
    let dialog_class = w!("ScreenTimePasscodeDialogNice");
    let hinstance = GetModuleHandleW(None).expect("Failed to get module handle");

    static mut DIALOG_RESULT: Option<bool> = None;
    static mut DIALOG_EDIT_HWND: Option<HWND> = None;
    static mut DIALOG_ERROR: Option<String> = None;
    static mut DIALOG_PROMPT_KEY: &str = "passcode.subtitle";
    /// The prompt currently open, if any.
    static OPEN_PROMPT: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

    // One prompt at a time: the tray menu stays usable while a prompt is
    // open, and a second prompt would take over the state above from the
    // first (whose OK would then read the second's edit control, destroyed
    // by then). Bring the open one to the front instead; the second request
    // is simply not granted.
    let open = HWND(OPEN_PROMPT.load(AtomicOrdering::SeqCst));
    if !open.0.is_null() && IsWindow(open).as_bool() {
        let _ = ShowWindow(open, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(open);
        if let Some(edit) = DIALOG_EDIT_HWND {
            let _ = SetFocus(edit);
        }
        return false;
    }

    DIALOG_RESULT = None;
    DIALOG_EDIT_HWND = None;
    DIALOG_ERROR = None;
    DIALOG_PROMPT_KEY = action.prompt_key();

    // A plain WS_CHILD EDIT control owns keyboard focus once created, so
    // WM_KEYDOWN never reaches this dialog's own window proc below (Win32
    // routes key messages to the focused window, not its parent) - the
    // VK_RETURN/VK_ESCAPE handling further down would otherwise be dead code.
    // Subclassing the edit control to relay just those two keys up to the
    // parent lets the existing dialog-level handler do the rest unchanged.
    unsafe extern "system" fn edit_subclass_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_KEYDOWN && (wparam.0 == VK_RETURN.0 as usize || wparam.0 == VK_ESCAPE.0 as usize) {
            if let Ok(parent) = GetParent(hwnd) {
                SendMessageW(parent, msg, wparam, lparam);
            }
            return LRESULT(0);
        }
        let orig_proc: WNDPROC = std::mem::transmute(GetWindowLongPtrW(hwnd, GWLP_USERDATA));
        CallWindowProcW(orig_proc, hwnd, msg, wparam, lparam)
    }

    unsafe extern "system" fn dialog_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_CREATE => {
                let hinstance = GetModuleHandleW(None).unwrap();

                let edit = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("EDIT"),
                    w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER
                        | WINDOW_STYLE(ES_CENTER as u32 | ES_PASSWORD as u32 | ES_NUMBER as u32),
                    scale(100), scale(100), scale(150), scale(36),
                    hwnd,
                    HMENU(101 as _),
                    hinstance,
                    None,
                ).ok();

                if let Some(e) = edit {
                    DIALOG_EDIT_HWND = Some(e);
                    SendMessageW(e, EM_SETLIMITTEXT, WPARAM(4), LPARAM(0));

                    let hfont = track_gdi(hwnd, CreateFontW(
                        scale(28), 0, 0, 0,
                        FW_BOLD.0 as i32,
                        0, 0, 0, 0, 0, 0, 5, 0,
                        w!("Segoe UI"),
                    ));
                    SendMessageW(e, WM_SETFONT, WPARAM(hfont.0 as usize), LPARAM(1));
                    let orig = SetWindowLongPtrW(e, GWLP_WNDPROC, edit_subclass_proc as *const () as isize);
                    SetWindowLongPtrW(e, GWLP_USERDATA, orig);
                    let _ = SetFocus(e);
                }

                // Button font
                let btn_font = track_gdi(hwnd, CreateFontW(
                    scale(14), 0, 0, 0,
                    FW_NORMAL.0 as i32,
                    0, 0, 0, 0, 0, 0, 5, 0,
                    w!("Segoe UI"),
                ));

                // OK Button
                let ok_btn_text = i18n::wide("button.ok");
                let ok_btn = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("BUTTON"),
                    PCWSTR(ok_btn_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                    scale(70), scale(200), scale(100), scale(40),
                    hwnd,
                    HMENU(1 as _),
                    hinstance,
                    None,
                );
                if let Ok(h) = ok_btn { SendMessageW(h, WM_SETFONT, WPARAM(btn_font.0 as usize), LPARAM(1)); }

                // Cancel Button
                let cancel_btn_text = i18n::wide("button.cancel");
                let cancel_btn = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("BUTTON"),
                    PCWSTR(cancel_btn_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                    scale(180), scale(200), scale(100), scale(40),
                    hwnd,
                    HMENU(2 as _),
                    hinstance,
                    None,
                );
                if let Ok(h) = cancel_btn { SendMessageW(h, WM_SETFONT, WPARAM(btn_font.0 as usize), LPARAM(1)); }

                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);

                let mut rect: RECT = zeroed();
                GetClientRect(hwnd, &mut rect).ok();

                let bg_brush = CreateSolidBrush(COLORREF(0x00F0F0F0));
                FillRect(hdc, &rect, bg_brush);
                let _ = DeleteObject(bg_brush);

                let title_font = CreateFontW(
                    scale(20), 0, 0, 0,
                    FW_BOLD.0 as i32,
                    0, 0, 0, 0, 0, 0, 5, 0,
                    w!("Segoe UI"),
                );
                let old_font = SelectObject(hdc, title_font);
                SetTextColor(hdc, COLORREF(0x00333333));
                SetBkMode(hdc, TRANSPARENT);

                let mut title_rect = RECT { left: 0, top: scale(25), right: rect.right, bottom: scale(55) };
                let title_text: Vec<u16> = i18n::t("window.passcode").encode_utf16().collect();
                DrawTextW(
                    hdc,
                    &mut title_text.clone(),
                    &mut title_rect,
                    DT_CENTER | DT_SINGLELINE,
                );

                let sub_font = CreateFontW(
                    scale(13), 0, 0, 0,
                    FW_NORMAL.0 as i32,
                    0, 0, 0, 0, 0, 0, 5, 0,
                    w!("Segoe UI"),
                );
                SelectObject(hdc, sub_font);
                SetTextColor(hdc, COLORREF(0x00666666));

                // Names the action being authorised (or the generic prompt).
                let mut sub_rect = RECT { left: 0, top: scale(55), right: rect.right, bottom: scale(80) };
                let sub_text: Vec<u16> = i18n::t(DIALOG_PROMPT_KEY).encode_utf16().collect();
                DrawTextW(
                    hdc,
                    &mut sub_text.clone(),
                    &mut sub_rect,
                    DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                );

                let error_ref = std::ptr::addr_of!(DIALOG_ERROR);
                if let Some(error) = (*error_ref).as_deref() {
                    SetTextColor(hdc, COLORREF(COLOR_ERROR));
                    let mut err_rect = RECT { left: 0, top: scale(150), right: rect.right, bottom: scale(170) };
                    let mut err_text: Vec<u16> = error.encode_utf16().collect();
                    DrawTextW(
                        hdc,
                        &mut err_text,
                        &mut err_rect,
                        DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                    );
                }

                SelectObject(hdc, old_font);
                let _ = DeleteObject(title_font);
                let _ = DeleteObject(sub_font);

                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as u16;
                match id {
                    1 => { // OK
                        if let Some(edit_hwnd) = DIALOG_EDIT_HWND {
                            let mut buffer = [0u16; 16];
                            let len = GetWindowTextW(edit_hwnd, &mut buffer);
                            let entered: String = String::from_utf16_lossy(&buffer[..len as usize]);

                            let error = match passcode_guard::attempt(&entered) {
                                Attempt::Correct => None,
                                Attempt::Wrong { locked_for_secs: 0 } => {
                                    Some(i18n::t("passcode.incorrect").to_string())
                                }
                                Attempt::Wrong { locked_for_secs: secs }
                                | Attempt::LockedOut { secs_remaining: secs } => Some(format!(
                                    "{} {}",
                                    i18n::t("passcode.locked"),
                                    passcode_guard::format_wait(secs)
                                )),
                                Attempt::Unavailable => Some(i18n::t("passcode.unavailable").to_string()),
                            };
                            if error.is_none() {
                                DIALOG_RESULT = Some(true);
                                DestroyWindow(hwnd).ok();
                            } else {
                                DIALOG_ERROR = error;
                                let _ = InvalidateRect(hwnd, None, true);
                                SetWindowTextW(edit_hwnd, w!("")).ok();
                                let _ = SetFocus(edit_hwnd);
                            }
                        }
                    }
                    2 => { // Cancel
                        DIALOG_RESULT = Some(false);
                        DestroyWindow(hwnd).ok();
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if wparam.0 == VK_RETURN.0 as usize {
                    SendMessageW(hwnd, WM_COMMAND, WPARAM(1), LPARAM(0));
                } else if wparam.0 == VK_ESCAPE.0 as usize {
                    DIALOG_RESULT = Some(false);
                    DestroyWindow(hwnd).ok();
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                DIALOG_RESULT = Some(false);
                DestroyWindow(hwnd).ok();
                LRESULT(0)
            }
            WM_NCDESTROY => {
                release_gdi(hwnd);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    register_dialog_class(dialog_class, Some(dialog_proc), 0x00F0F0F0);

    let screen_width = GetSystemMetrics(SM_CXSCREEN);
    let screen_height = GetSystemMetrics(SM_CYSCREEN);
    let dialog_width = scale(350);
    let dialog_height = scale(300);

    let dialog_hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_DLGMODALFRAME,
        dialog_class,
        w!(""),
        WS_POPUP | WS_CAPTION | WS_SYSMENU,
        (screen_width - dialog_width) / 2,
        (screen_height - dialog_height) / 2,
        dialog_width,
        dialog_height,
        parent_hwnd,
        HMENU::default(),
        hinstance,
        None,
    );

    if let Ok(dlg) = dialog_hwnd {
        OPEN_PROMPT.store(dlg.0, AtomicOrdering::SeqCst);
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetForegroundWindow(dlg);
        run_modal_loop(dlg);
        OPEN_PROMPT.store(std::ptr::null_mut(), AtomicOrdering::SeqCst);
    }
    DIALOG_EDIT_HWND = None;

    DIALOG_RESULT.unwrap_or(false)
}

/// Show the settings dialog
pub unsafe fn show_settings_dialog(parent_hwnd: HWND) {
    let dialog_class = w!("ScreenTimeSettingsDialog");
    let hinstance = GetModuleHandleW(None).expect("Failed to get module handle");

    static mut SETTINGS_DIALOG_OPEN: bool = false;

    if SETTINGS_DIALOG_OPEN {
        return;
    }
    SETTINGS_DIALOG_OPEN = true;

    unsafe extern "system" fn settings_dialog_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_CREATE => {
                let hinstance = GetModuleHandleW(None).unwrap();

                // Font quality: 5 = CLEARTYPE_QUALITY for crisp rendering
                let label_font = track_gdi(hwnd, CreateFontW(
                    scale(14), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"),
                ));
                let title_font = track_gdi(hwnd, CreateFontW(
                    scale(16), 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"),
                ));
                let edit_font = track_gdi(hwnd, CreateFontW(
                    scale(14), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"),
                ));

                SETTINGS_SCROLL_POS = 0;
                SETTINGS_DEFAULT_PASSCODE_WARNING = HWND::default();

                let mut y_pos = scale(10);

                // ===== Language Section =====
                let lang_label = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), w!("Language / Sprache:"),
                    WS_CHILD | WS_VISIBLE, scale(15), y_pos + scale(2), scale(140), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = lang_label { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }

                let lang_combo = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("COMBOBOX"), w!(""),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(CBS_DROPDOWNLIST as u32),
                    scale(160), y_pos, scale(100), scale(200), hwnd, HMENU(ID_LANGUAGE_COMBO as _), hinstance, None,
                );
                let mut lang_combo_hwnd = HWND::default();
                if let Ok(h) = lang_combo {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    // Add language options
                    for lang in Language::all() {
                        let name: Vec<u16> = lang.name().encode_utf16().chain(std::iter::once(0)).collect();
                        SendMessageW(h, CB_ADDSTRING, WPARAM(0), LPARAM(name.as_ptr() as isize));
                    }
                    // Select current language
                    let current_lang = i18n::current();
                    let index = match current_lang {
                        Language::English => 0,
                        Language::German => 1,
                    };
                    SendMessageW(h, CB_SETCURSEL, WPARAM(index), LPARAM(0));
                    lang_combo_hwnd = h;
                }
                y_pos += scale(28);

                // ===== Daily Limits Section =====
                let title1_text = i18n::wide("settings.daily_limits");
                let title1 = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(title1_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(15), y_pos, scale(350), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = title1 { SendMessageW(h, WM_SETFONT, WPARAM(title_font.0 as usize), LPARAM(1)); }
                y_pos += scale(22);

                let mut daily_handles: [HWND; 7] = [HWND::default(); 7];

                // Create day controls in pairs (two columns per row)
                // Row 0: Monday (0), Tuesday (1)
                // Row 1: Wednesday (2), Thursday (3)
                // Row 2: Friday (4), Saturday (5)
                // Row 3: Sunday (6) alone
                for row in 0..4 {
                    let i = row * 2; // First column day index

                    // First column
                    let label_text: Vec<u16> = format!("{}:\0", i18n::weekday(i)).encode_utf16().collect();
                    let label = CreateWindowExW(
                        WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(label_text.as_ptr()),
                        WS_CHILD | WS_VISIBLE, scale(25), y_pos + scale(2), scale(90), scale(20), hwnd, HMENU::default(), hinstance, None,
                    );
                    if let Ok(h) = label { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }

                    let edit = CreateWindowExW(
                        WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                        WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_NUMBER as u32 | ES_CENTER as u32),
                        scale(120), y_pos, scale(60), scale(22), hwnd, HMENU((ID_SETTINGS_BASE + i as i32) as _), hinstance, None,
                    );
                    if let Ok(h) = edit {
                        SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                        SendMessageW(h, EM_SETLIMITTEXT, WPARAM(4), LPARAM(0));
                        // What the app uses: an empty or broken stored value
                        // reads as the default, so show that, not the text.
                        let value = crate::database::get_daily_limit(i as u32).to_string();
                        let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                        SetWindowTextW(h, PCWSTR(wide.as_ptr())).ok();
                        daily_handles[i] = h;
                    }

                    // Second column (only if there's a second day in this row)
                    let i2 = i + 1;
                    if i2 < 7 {
                        let label_text2: Vec<u16> = format!("{}:\0", i18n::weekday(i2)).encode_utf16().collect();
                        let label2 = CreateWindowExW(
                            WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(label_text2.as_ptr()),
                            WS_CHILD | WS_VISIBLE, scale(210), y_pos + scale(2), scale(90), scale(20), hwnd, HMENU::default(), hinstance, None,
                        );
                        if let Ok(h) = label2 { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }

                        let edit2 = CreateWindowExW(
                            WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                            WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_NUMBER as u32 | ES_CENTER as u32),
                            scale(305), y_pos, scale(60), scale(22), hwnd, HMENU((ID_SETTINGS_BASE + i2 as i32) as _), hinstance, None,
                        );
                        if let Ok(h) = edit2 {
                            SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                            SendMessageW(h, EM_SETLIMITTEXT, WPARAM(4), LPARAM(0));
                            let value = crate::database::get_daily_limit(i2 as u32).to_string();
                            let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                            SetWindowTextW(h, PCWSTR(wide.as_ptr())).ok();
                            daily_handles[i2] = h;
                        }
                    }

                    y_pos += scale(24);
                }

                // ===== Warning 1 Section =====
                y_pos += scale(10);
                let title2_text = i18n::wide("settings.warning1");
                let title2 = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(title2_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(15), y_pos, scale(350), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = title2 { SendMessageW(h, WM_SETFONT, WPARAM(title_font.0 as usize), LPARAM(1)); }
                y_pos += scale(20);

                let min_label1_text = i18n::wide("settings.minutes_before");
                let min_label1 = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(min_label1_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(25), y_pos + scale(2), scale(100), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = min_label1 { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                let w1_min = CreateWindowExW(
                    WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_NUMBER as u32 | ES_CENTER as u32),
                    scale(130), y_pos, scale(50), scale(22), hwnd, HMENU((ID_SETTINGS_BASE + 20) as _), hinstance, None,
                );
                let mut w1_min_hwnd = HWND::default();
                if let Ok(h) = w1_min {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    SendMessageW(h, EM_SETLIMITTEXT, WPARAM(MINUTES_FIELD_MAX_CHARS), LPARAM(0));
                    // What the app uses, as for the daily limits.
                    let value = crate::database::get_warning_config(1).0.to_string();
                    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                    SetWindowTextW(h, PCWSTR(wide.as_ptr())).ok();
                    w1_min_hwnd = h;
                }
                y_pos += scale(24);

                let msg_label1_text = i18n::wide("settings.message");
                let msg_label1 = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(msg_label1_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(25), y_pos + scale(2), scale(60), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = msg_label1 { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                let w1_msg = CreateWindowExW(
                    WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER,
                    scale(90), y_pos, scale(275), scale(22), hwnd, HMENU((ID_SETTINGS_BASE + 21) as _), hinstance, None,
                );
                let mut w1_msg_hwnd = HWND::default();
                if let Ok(h) = w1_msg {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    let value = get_setting("warning1_message").unwrap_or_else(|| "10 minutes remaining!".to_string());
                    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                    SetWindowTextW(h, PCWSTR(wide.as_ptr())).ok();
                    w1_msg_hwnd = h;
                }
                y_pos += scale(24);

                // ===== Warning 2 Section =====
                y_pos += scale(10);
                let title3_text = i18n::wide("settings.warning2");
                let title3 = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(title3_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(15), y_pos, scale(350), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = title3 { SendMessageW(h, WM_SETFONT, WPARAM(title_font.0 as usize), LPARAM(1)); }
                y_pos += scale(20);

                let min_label2_text = i18n::wide("settings.minutes_before");
                let min_label2 = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(min_label2_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(25), y_pos + scale(2), scale(100), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = min_label2 { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                let w2_min = CreateWindowExW(
                    WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_NUMBER as u32 | ES_CENTER as u32),
                    scale(130), y_pos, scale(50), scale(22), hwnd, HMENU((ID_SETTINGS_BASE + 30) as _), hinstance, None,
                );
                let mut w2_min_hwnd = HWND::default();
                if let Ok(h) = w2_min {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    SendMessageW(h, EM_SETLIMITTEXT, WPARAM(MINUTES_FIELD_MAX_CHARS), LPARAM(0));
                    let value = crate::database::get_warning_config(2).0.to_string();
                    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                    SetWindowTextW(h, PCWSTR(wide.as_ptr())).ok();
                    w2_min_hwnd = h;
                }
                y_pos += scale(24);

                let msg_label2_text = i18n::wide("settings.message");
                let msg_label2 = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(msg_label2_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(25), y_pos + scale(2), scale(60), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = msg_label2 { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                let w2_msg = CreateWindowExW(
                    WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER,
                    scale(90), y_pos, scale(275), scale(22), hwnd, HMENU((ID_SETTINGS_BASE + 31) as _), hinstance, None,
                );
                let mut w2_msg_hwnd = HWND::default();
                if let Ok(h) = w2_msg {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    let value = get_setting("warning2_message").unwrap_or_else(|| "5 minutes remaining!".to_string());
                    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                    SetWindowTextW(h, PCWSTR(wide.as_ptr())).ok();
                    w2_msg_hwnd = h;
                }
                y_pos += scale(24);

                // ===== Blocking Message Section =====
                y_pos += scale(10);
                let title4_text = i18n::wide("settings.blocking_message");
                let title4 = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(title4_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(15), y_pos, scale(350), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = title4 { SendMessageW(h, WM_SETFONT, WPARAM(title_font.0 as usize), LPARAM(1)); }
                y_pos += scale(20);

                let block_msg = CreateWindowExW(
                    WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER,
                    scale(25), y_pos, scale(340), scale(22), hwnd, HMENU((ID_SETTINGS_BASE + 40) as _), hinstance, None,
                );
                let mut block_msg_hwnd = HWND::default();
                if let Ok(h) = block_msg {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    let value = get_setting("blocking_message").unwrap_or_else(|| "Your screen time limit has been reached.".to_string());
                    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                    SetWindowTextW(h, PCWSTR(wide.as_ptr())).ok();
                    block_msg_hwnd = h;
                }
                y_pos += scale(24);

                // ===== Change Passcode Section =====
                y_pos += scale(10);
                let title5_text = i18n::wide("settings.passcode");
                let title5 = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(title5_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(15), y_pos, scale(360), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = title5 { SendMessageW(h, WM_SETFONT, WPARAM(title_font.0 as usize), LPARAM(1)); }
                y_pos += scale(20);

                // Still the factory passcode: say so, in red (see
                // WM_CTLCOLORSTATIC). This row only exists in that case.
                if crate::database::passcode_is_default() {
                    let warning_text = i18n::wide("settings.passcode_default_warning");
                    let warning = CreateWindowExW(
                        WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(warning_text.as_ptr()),
                        WS_CHILD | WS_VISIBLE, scale(25), y_pos, scale(350), scale(20), hwnd, HMENU::default(), hinstance, None,
                    );
                    if let Ok(h) = warning {
                        SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1));
                        SETTINGS_DEFAULT_PASSCODE_WARNING = h;
                    }
                    y_pos += scale(20);
                }

                let curr_label_text = i18n::wide("settings.current");
                let curr_label = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(curr_label_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(25), y_pos + scale(2), scale(55), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = curr_label { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                let curr_pass = CreateWindowExW(
                    WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_PASSWORD as u32 | ES_NUMBER as u32 | ES_CENTER as u32),
                    scale(80), y_pos, scale(60), scale(22), hwnd, HMENU(ID_CURRENT_PASSCODE as _), hinstance, None,
                );
                let mut curr_pass_hwnd = HWND::default();
                if let Ok(h) = curr_pass {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    SendMessageW(h, EM_SETLIMITTEXT, WPARAM(4), LPARAM(0));
                    curr_pass_hwnd = h;
                }

                let new_label_text = i18n::wide("settings.new");
                let new_label = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(new_label_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(155), y_pos + scale(2), scale(35), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = new_label { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                let new_pass = CreateWindowExW(
                    WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_PASSWORD as u32 | ES_NUMBER as u32 | ES_CENTER as u32),
                    scale(190), y_pos, scale(60), scale(22), hwnd, HMENU(ID_NEW_PASSCODE as _), hinstance, None,
                );
                let mut new_pass_hwnd = HWND::default();
                if let Ok(h) = new_pass {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    SendMessageW(h, EM_SETLIMITTEXT, WPARAM(4), LPARAM(0));
                    new_pass_hwnd = h;
                }

                let confirm_label_text = i18n::wide("settings.confirm");
                let confirm_label = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(confirm_label_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(265), y_pos + scale(2), scale(50), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = confirm_label { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                let confirm_pass = CreateWindowExW(
                    WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_PASSWORD as u32 | ES_NUMBER as u32 | ES_CENTER as u32),
                    scale(315), y_pos, scale(60), scale(22), hwnd, HMENU(ID_CONFIRM_PASSCODE as _), hinstance, None,
                );
                let mut confirm_pass_hwnd = HWND::default();
                if let Ok(h) = confirm_pass {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    SendMessageW(h, EM_SETLIMITTEXT, WPARAM(4), LPARAM(0));
                    confirm_pass_hwnd = h;
                }
                y_pos += scale(26);

                // Rotating daily PIN - an additional code (alongside the
                // passcode above, not a replacement) that changes every day;
                // works everywhere the passcode does. Off by default.
                let rotating_pin_chk_text = i18n::wide("settings.enable_rotating_pin");
                let rotating_pin_chk = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("BUTTON"), PCWSTR(rotating_pin_chk_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                    scale(25), y_pos, scale(340), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                let mut rotating_pin_enabled_hwnd = HWND::default();
                if let Ok(h) = rotating_pin_chk {
                    SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1));
                    if crate::database::is_rotating_pin_enabled() {
                        SendMessageW(h, BM_SETCHECK, WPARAM(1), LPARAM(0));
                    }
                    rotating_pin_enabled_hwnd = h;
                }
                y_pos += scale(24);

                // ===== Telegram Bot Section =====
                y_pos += scale(10);
                let title6_text = i18n::wide("settings.telegram");
                let title6 = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(title6_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(15), y_pos, scale(360), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = title6 { SendMessageW(h, WM_SETFONT, WPARAM(title_font.0 as usize), LPARAM(1)); }
                y_pos += scale(20);

                // Enable checkbox
                let telegram_chk_text = i18n::wide("settings.enable_telegram");
                let telegram_enabled_chk = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("BUTTON"), PCWSTR(telegram_chk_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                    scale(25), y_pos, scale(200), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                let mut telegram_enabled_hwnd = HWND::default();
                if let Ok(h) = telegram_enabled_chk {
                    SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1));
                    let config = get_telegram_config();
                    if config.enabled {
                        SendMessageW(h, BM_SETCHECK, WPARAM(1), LPARAM(0));
                    }
                    telegram_enabled_hwnd = h;
                }
                y_pos += scale(22);

                // Bot Token
                let bot_token_label_text = i18n::wide("settings.bot_token");
                let bot_token_label = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(bot_token_label_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(25), y_pos + scale(2), scale(70), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = bot_token_label { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                let telegram_token = CreateWindowExW(
                    WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_PASSWORD as u32 | ES_AUTOHSCROLL as u32),
                    scale(100), y_pos, scale(265), scale(22), hwnd, HMENU::default(), hinstance, None,
                );
                let mut telegram_token_hwnd = HWND::default();
                if let Ok(h) = telegram_token {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    // Allow long bot tokens (up to 200 chars)
                    SendMessageW(h, EM_SETLIMITTEXT, WPARAM(200), LPARAM(0));
                    let config = get_telegram_config();
                    if let Some(token) = config.bot_token {
                        let wide: Vec<u16> = token.encode_utf16().chain(std::iter::once(0)).collect();
                        SetWindowTextW(h, PCWSTR(wide.as_ptr())).ok();
                    }
                    telegram_token_hwnd = h;
                }
                y_pos += scale(24);

                // Admin Chat ID
                let chat_id_label_text = i18n::wide("settings.chat_id");
                let chat_id_label = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(chat_id_label_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(25), y_pos + scale(2), scale(70), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = chat_id_label { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                // Not ES_NUMBER: a group chat's ID is negative (checked on Save).
                let telegram_chat_id = CreateWindowExW(
                    WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER,
                    scale(100), y_pos, scale(120), scale(22), hwnd, HMENU::default(), hinstance, None,
                );
                let mut telegram_chat_id_hwnd = HWND::default();
                if let Ok(h) = telegram_chat_id {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    let config = get_telegram_config();
                    if let Some(chat_id) = config.admin_chat_id {
                        let value = chat_id.to_string();
                        let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                        SetWindowTextW(h, PCWSTR(wide.as_ptr())).ok();
                    }
                    telegram_chat_id_hwnd = h;
                }

                // Setup Wizard button (to the right of chat_id)
                let wizard_btn_text = i18n::wide("settings.setup_wizard");
                let wizard_btn = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("BUTTON"), PCWSTR(wizard_btn_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                    scale(230), y_pos, scale(135), scale(22), hwnd, HMENU(ID_TELEGRAM_WIZARD as _), hinstance, None,
                );
                if let Ok(h) = wizard_btn { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                y_pos += scale(24);

                // ===== Discord Bot Section =====
                y_pos += scale(10);
                let title_dc_text = i18n::wide("settings.discord");
                let title_dc = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(title_dc_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(15), y_pos, scale(360), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = title_dc { SendMessageW(h, WM_SETFONT, WPARAM(title_font.0 as usize), LPARAM(1)); }
                y_pos += scale(20);

                // Enable checkbox
                let discord_chk_text = i18n::wide("settings.enable_discord");
                let discord_enabled_chk = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("BUTTON"), PCWSTR(discord_chk_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                    scale(25), y_pos, scale(200), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                let mut discord_enabled_hwnd = HWND::default();
                if let Ok(h) = discord_enabled_chk {
                    SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1));
                    let config = get_discord_config();
                    if config.enabled {
                        SendMessageW(h, BM_SETCHECK, WPARAM(1), LPARAM(0));
                    }
                    discord_enabled_hwnd = h;
                }
                y_pos += scale(22);

                // Bot Token
                let dc_token_label_text = i18n::wide("settings.bot_token");
                let dc_token_label = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(dc_token_label_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(25), y_pos + scale(2), scale(70), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = dc_token_label { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                let discord_token = CreateWindowExW(
                    WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_PASSWORD as u32 | ES_AUTOHSCROLL as u32),
                    scale(100), y_pos, scale(265), scale(22), hwnd, HMENU::default(), hinstance, None,
                );
                let mut discord_token_hwnd = HWND::default();
                if let Ok(h) = discord_token {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    // Allow long bot tokens
                    SendMessageW(h, EM_SETLIMITTEXT, WPARAM(200), LPARAM(0));
                    let config = get_discord_config();
                    if let Some(token) = config.bot_token {
                        let wide: Vec<u16> = token.encode_utf16().chain(std::iter::once(0)).collect();
                        SetWindowTextW(h, PCWSTR(wide.as_ptr())).ok();
                    }
                    discord_token_hwnd = h;
                }
                y_pos += scale(24);

                // Channel ID
                let dc_channel_label_text = i18n::wide("settings.channel_id");
                let dc_channel_label = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(dc_channel_label_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(25), y_pos + scale(2), scale(70), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = dc_channel_label { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                let discord_channel_id = CreateWindowExW(
                    WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_NUMBER as u32),
                    scale(100), y_pos, scale(265), scale(22), hwnd, HMENU::default(), hinstance, None,
                );
                let mut discord_channel_id_hwnd = HWND::default();
                if let Ok(h) = discord_channel_id {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    SendMessageW(h, EM_SETLIMITTEXT, WPARAM(32), LPARAM(0));
                    let config = get_discord_config();
                    if let Some(channel_id) = config.channel_id {
                        let value = channel_id.to_string();
                        let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                        SetWindowTextW(h, PCWSTR(wide.as_ptr())).ok();
                    }
                    discord_channel_id_hwnd = h;
                }
                y_pos += scale(24);

                // Admin User ID
                let dc_user_label_text = i18n::wide("settings.discord_user_id");
                let dc_user_label = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(dc_user_label_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(25), y_pos + scale(2), scale(70), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = dc_user_label { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                let discord_admin_user_id = CreateWindowExW(
                    WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_NUMBER as u32),
                    scale(100), y_pos, scale(265), scale(22), hwnd, HMENU::default(), hinstance, None,
                );
                let mut discord_admin_user_id_hwnd = HWND::default();
                if let Ok(h) = discord_admin_user_id {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    SendMessageW(h, EM_SETLIMITTEXT, WPARAM(32), LPARAM(0));
                    let config = get_discord_config();
                    if let Some(user_id) = config.admin_user_id {
                        let value = user_id.to_string();
                        let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                        SetWindowTextW(h, PCWSTR(wide.as_ptr())).ok();
                    }
                    discord_admin_user_id_hwnd = h;
                }
                y_pos += scale(24);

                // Setup Wizard button (its own row, right-aligned)
                let dc_wizard_btn_text = i18n::wide("settings.setup_wizard");
                let dc_wizard_btn = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("BUTTON"), PCWSTR(dc_wizard_btn_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                    scale(230), y_pos, scale(135), scale(22), hwnd, HMENU(ID_DISCORD_WIZARD as _), hinstance, None,
                );
                if let Ok(h) = dc_wizard_btn { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                y_pos += scale(24);

                // ===== Lock Screen Timeout =====
                y_pos += scale(10);
                let title7_text = i18n::wide("settings.lock_screen");
                let title7 = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(title7_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(15), y_pos, scale(360), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = title7 { SendMessageW(h, WM_SETFONT, WPARAM(title_font.0 as usize), LPARAM(1)); }
                y_pos += scale(20);

                // Lock screen timeout input
                let shutdown_label_text = i18n::wide("settings.shutdown_timeout");
                let shutdown_label = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(shutdown_label_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(25), y_pos + scale(2), scale(150), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = shutdown_label { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                let lock_timeout_edit = CreateWindowExW(
                    WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_NUMBER as u32),
                    scale(180), y_pos, scale(60), scale(22), hwnd, HMENU::default(), hinstance, None,
                );
                let mut lock_timeout_hwnd = HWND::default();
                if let Ok(h) = lock_timeout_edit {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    SendMessageW(h, EM_SETLIMITTEXT, WPARAM(MINUTES_FIELD_MAX_CHARS), LPARAM(0));
                    // Load current value (convert seconds to minutes for display)
                    let timeout_secs = crate::database::get_lock_screen_timeout();
                    let timeout_mins = timeout_secs / 60;
                    let value = timeout_mins.to_string();
                    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                    SetWindowTextW(h, PCWSTR(wide.as_ptr())).ok();
                    lock_timeout_hwnd = h;
                }
                let shutdown_hint_text = i18n::wide("settings.shutdown_timeout_hint");
                let shutdown_hint = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(shutdown_hint_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(250), y_pos + scale(2), scale(125), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = shutdown_hint { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                y_pos += scale(24);

                // ===== Idle Detection Section =====
                y_pos += scale(10);
                let title_idle_text = i18n::wide("settings.idle");
                let title_idle = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(title_idle_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(15), y_pos, scale(360), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = title_idle { SendMessageW(h, WM_SETFONT, WPARAM(title_font.0 as usize), LPARAM(1)); }
                y_pos += scale(20);

                // Enable checkbox
                let idle_chk_text = i18n::wide("settings.auto_pause_idle");
                let idle_enabled_chk = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("BUTTON"), PCWSTR(idle_chk_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                    scale(25), y_pos, scale(200), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                let mut idle_enabled_hwnd = HWND::default();
                if let Ok(h) = idle_enabled_chk {
                    SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1));
                    if is_idle_enabled() {
                        SendMessageW(h, BM_SETCHECK, WPARAM(1), LPARAM(0));
                    }
                    idle_enabled_hwnd = h;
                }
                y_pos += scale(22);

                // Timeout minutes
                let idle_timeout_label_text = i18n::wide("settings.idle_timeout");
                let idle_timeout_label = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(idle_timeout_label_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(25), y_pos + scale(2), scale(150), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = idle_timeout_label { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                let idle_timeout_edit = CreateWindowExW(
                    WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                    WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_NUMBER as u32 | ES_CENTER as u32),
                    scale(180), y_pos, scale(50), scale(22), hwnd, HMENU::default(), hinstance, None,
                );
                let mut idle_timeout_hwnd = HWND::default();
                if let Ok(h) = idle_timeout_edit {
                    SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                    SendMessageW(h, EM_SETLIMITTEXT, WPARAM(3), LPARAM(0));
                    let value = get_idle_timeout_minutes().to_string();
                    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                    SetWindowTextW(h, PCWSTR(wide.as_ptr())).ok();
                    idle_timeout_hwnd = h;
                }
                y_pos += scale(24);

                // ===== Pause Section =====
                // Three numeric rows built exactly like "Idle timeout" above
                // (label at 25, edit at 180), plus a short hint to the right
                // of the edit where one is needed.
                y_pos += scale(10);
                let title_pause_text = i18n::wide("settings.pause");
                let title_pause = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(title_pause_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE, scale(15), y_pos, scale(360), scale(20), hwnd, HMENU::default(), hinstance, None,
                );
                if let Ok(h) = title_pause { SendMessageW(h, WM_SETFONT, WPARAM(title_font.0 as usize), LPARAM(1)); }
                y_pos += scale(20);

                let pause_config = get_pause_config();
                // Pausing switched off shows as a budget of 0 (and saving 0
                // switches it off - see the Save handler).
                let budget_value = if is_pause_enabled() { pause_config.daily_budget_minutes } else { 0 };
                let pause_rows = [
                    ("settings.pause_budget", Some("settings.pause_budget_hint"), budget_value),
                    ("settings.pause_max", Some("settings.pause_max_hint"), pause_config.max_duration_minutes),
                    ("settings.pause_cooldown", None, pause_config.cooldown_minutes),
                ];
                let mut pause_hwnds = [HWND::default(); 3];
                for (i, (label_key, hint_key, value)) in pause_rows.into_iter().enumerate() {
                    let label_text = i18n::wide(label_key);
                    let label = CreateWindowExW(
                        WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(label_text.as_ptr()),
                        WS_CHILD | WS_VISIBLE, scale(25), y_pos + scale(2), scale(150), scale(20), hwnd, HMENU::default(), hinstance, None,
                    );
                    if let Ok(h) = label { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                    let edit = CreateWindowExW(
                        WINDOW_EX_STYLE(0x200), w!("EDIT"), w!(""),
                        WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_NUMBER as u32 | ES_CENTER as u32),
                        scale(180), y_pos, scale(50), scale(22), hwnd, HMENU::default(), hinstance, None,
                    );
                    if let Ok(h) = edit {
                        SendMessageW(h, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
                        SendMessageW(h, EM_SETLIMITTEXT, WPARAM(3), LPARAM(0));
                        let wide: Vec<u16> = value.to_string().encode_utf16().chain(std::iter::once(0)).collect();
                        SetWindowTextW(h, PCWSTR(wide.as_ptr())).ok();
                        pause_hwnds[i] = h;
                    }
                    if let Some(hint_key) = hint_key {
                        let hint_text = i18n::wide(hint_key);
                        let hint = CreateWindowExW(
                            WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(hint_text.as_ptr()),
                            WS_CHILD | WS_VISIBLE, scale(240), y_pos + scale(2), scale(135), scale(20), hwnd, HMENU::default(), hinstance, None,
                        );
                        if let Ok(h) = hint { SendMessageW(h, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1)); }
                    }
                    y_pos += scale(24);
                }
                y_pos += scale(4);

                // ===== Buttons =====
                let btn_font = track_gdi(hwnd, CreateFontW(
                    scale(14), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"),
                ));

                let save_btn_text = i18n::wide("button.save");
                let save_btn = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("BUTTON"), PCWSTR(save_btn_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                    scale(100), y_pos, scale(90), scale(30), hwnd, HMENU(ID_SETTINGS_SAVE as _), hinstance, None,
                );
                if let Ok(h) = save_btn { SendMessageW(h, WM_SETFONT, WPARAM(btn_font.0 as usize), LPARAM(1)); }

                let cancel_btn_text = i18n::wide("button.cancel");
                let cancel_btn = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("BUTTON"), PCWSTR(cancel_btn_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                    scale(200), y_pos, scale(90), scale(30), hwnd, HMENU(ID_SETTINGS_CANCEL as _), hinstance, None,
                );
                if let Ok(h) = cancel_btn { SendMessageW(h, WM_SETFONT, WPARAM(btn_font.0 as usize), LPARAM(1)); }

                // Client height the controls need: buttons plus a margin.
                SETTINGS_CONTENT_HEIGHT = y_pos + scale(30) + scale(12);

                SETTINGS_EDIT_HANDLES = Some(SettingsEditHandles {
                    daily_limits: daily_handles,
                    warning1_minutes: w1_min_hwnd,
                    warning1_message: w1_msg_hwnd,
                    warning2_minutes: w2_min_hwnd,
                    warning2_message: w2_msg_hwnd,
                    blocking_message: block_msg_hwnd,
                    current_passcode: curr_pass_hwnd,
                    new_passcode: new_pass_hwnd,
                    confirm_passcode: confirm_pass_hwnd,
                    rotating_pin_enabled: rotating_pin_enabled_hwnd,
                    telegram_token: telegram_token_hwnd,
                    telegram_chat_id: telegram_chat_id_hwnd,
                    telegram_enabled: telegram_enabled_hwnd,
                    discord_token: discord_token_hwnd,
                    discord_channel_id: discord_channel_id_hwnd,
                    discord_admin_user_id: discord_admin_user_id_hwnd,
                    discord_enabled: discord_enabled_hwnd,
                    lock_screen_timeout: lock_timeout_hwnd,
                    idle_enabled: idle_enabled_hwnd,
                    idle_timeout_minutes: idle_timeout_hwnd,
                    language: lang_combo_hwnd,
                    pause_daily_budget: pause_hwnds[0],
                    pause_max_duration: pause_hwnds[1],
                    pause_cooldown: pause_hwnds[2],
                });
                // What the fields showed on opening: Save writes only what
                // differs from this.
                let handles_ref = std::ptr::addr_of!(SETTINGS_EDIT_HANDLES);
                SETTINGS_LOADED = (*handles_ref).as_ref().map(|h| read_settings_form(h));

                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;

                if id == ID_SETTINGS_SAVE {
                    if let Some(ref handles) = SETTINGS_EDIT_HANDLES {
                        // Only what the parent changed here is written (a
                        // limit, message or PIN setting changed meanwhile by
                        // a bot command is kept), and only once every changed
                        // field is acceptable - nothing is written before.
                        let now = read_settings_form(handles);
                        let loaded_ref = std::ptr::addr_of!(SETTINGS_LOADED);
                        let loaded = (*loaded_ref).clone().unwrap_or_else(|| now.clone());
                        let min_lock = u32::try_from(crate::database::MIN_LOCK_SCREEN_TIMEOUT_MINS).unwrap_or(1);
                        let changes = match plan_settings_save(&loaded, &now, min_lock) {
                            Ok(changes) => changes,
                            Err(error) => {
                                let edit = settings_field_edit(handles, error.field);
                                if error.kind == SettingsErrorKind::BelowOneMinute {
                                    SetWindowTextW(edit, w!("1")).ok();
                                }
                                show_settings_field_error(hwnd, edit, &settings_error_text(&error));
                                return LRESULT(0);
                            }
                        };

                        // Handle passcode change first
                        let mut current_pass = String::new();
                        let mut new_pass = String::new();
                        let mut confirm_pass = String::new();

                        if !handles.current_passcode.0.is_null() {
                            let mut buffer = [0u16; 16];
                            let len = GetWindowTextW(handles.current_passcode, &mut buffer);
                            current_pass = String::from_utf16_lossy(&buffer[..len as usize]);
                        }
                        if !handles.new_passcode.0.is_null() {
                            let mut buffer = [0u16; 16];
                            let len = GetWindowTextW(handles.new_passcode, &mut buffer);
                            new_pass = String::from_utf16_lossy(&buffer[..len as usize]);
                        }
                        if !handles.confirm_passcode.0.is_null() {
                            let mut buffer = [0u16; 16];
                            let len = GetWindowTextW(handles.confirm_passcode, &mut buffer);
                            confirm_pass = String::from_utf16_lossy(&buffer[..len as usize]);
                        }

                        // Check if user wants to change passcode
                        if !new_pass.is_empty() || !confirm_pass.is_empty() {
                            // Verify current passcode (an unreadable one
                            // matches nothing)
                            let stored = crate::database::effective_passcode();
                            if stored.as_deref() != Some(current_pass.as_str()) {
                                let msg = i18n::wide("settings.error.current_incorrect");
                                let title = i18n::wide("settings.error");
                                MessageBoxW(hwnd, PCWSTR(msg.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | MB_ICONERROR);
                                return LRESULT(0);
                            }

                            // Check new passcode requirements: four digits,
                            // as the bots' set-passcode command requires (only
                            // digits can be typed into the passcode prompts,
                            // so pasted letters would lock the parent out)
                            if !is_valid_new_passcode(&new_pass) {
                                let msg = i18n::wide("settings.error.passcode_length");
                                let title = i18n::wide("settings.error");
                                MessageBoxW(hwnd, PCWSTR(msg.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | MB_ICONERROR);
                                return LRESULT(0);
                            }

                            // Check that new passcodes match
                            if new_pass != confirm_pass {
                                let msg = i18n::wide("settings.error.passcode_mismatch");
                                let title = i18n::wide("settings.error");
                                MessageBoxW(hwnd, PCWSTR(msg.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | MB_ICONERROR);
                                return LRESULT(0);
                            }

                            // Save new passcode (shared machine-wide, so it
                            // works no matter which Windows account is
                            // logged in - see database::set_passcode)
                            crate::database::set_passcode(&new_pass);
                        }

                        // Save rotating PIN toggle
                        if let Some(enabled) = changes.rotating_pin {
                            crate::database::set_rotating_pin_enabled(enabled);
                        }

                        // Save other settings
                        for (i, minutes) in changes.daily_limits.iter().enumerate() {
                            if let Some(minutes) = minutes {
                                set_setting(WEEKDAY_KEYS[i], &minutes.to_string());
                            }
                        }

                        // If today's limit changed, move the remaining time
                        // by the difference (nothing if it didn't change)
                        crate::blocking::apply_todays_limit_change();

                        if let Some(minutes) = changes.warning1_minutes {
                            set_setting("warning1_minutes", &minutes.to_string());
                        }
                        if let Some(ref message) = changes.warning1_message {
                            set_setting("warning1_message", message);
                        }

                        if let Some(minutes) = changes.warning2_minutes {
                            set_setting("warning2_minutes", &minutes.to_string());
                        }
                        if let Some(ref message) = changes.warning2_message {
                            set_setting("warning2_message", message);
                        }

                        if let Some(ref message) = changes.blocking_message {
                            set_setting("blocking_message", message);
                        }

                        // Save Telegram and Discord settings - what was
                        // changed here, the stored values for the rest - and
                        // restart a bot whose settings changed, below
                        let telegram_before = get_telegram_config();
                        let discord_before = get_discord_config();
                        if let Some((token, chat_id, enabled)) = merged_telegram(&changes.telegram, &telegram_before) {
                            set_telegram_config(&token, &chat_id, enabled);
                        }
                        if let Some((token, channel_id, user_id, enabled)) = merged_discord(&changes.discord, &discord_before) {
                            set_discord_config(&token, &channel_id, &user_id, enabled);
                        }

                        let telegram_changed = get_telegram_config() != telegram_before;
                        let discord_changed = get_discord_config() != discord_before;
                        if telegram_changed || discord_changed {
                            crate::session::restart_remote_bots(telegram_changed, discord_changed);
                        }

                        // Save lock screen timeout (convert minutes to seconds)
                        // At least a minute (0 meant "shut down a second
                        // after every block") and at most a day; validated
                        // above, and the minimum enforced again where it's
                        // read (database::get_lock_screen_timeout).
                        if let Some(secs) = changes.lock_screen_timeout_secs {
                            set_setting("lock_screen_timeout", &secs.to_string());
                        }

                        // Save idle detection settings
                        if let Some(enabled) = changes.idle_enabled {
                            set_setting("idle_enabled", if enabled { "1" } else { "0" });
                        }
                        if let Some(minutes) = changes.idle_timeout {
                            set_setting("idle_timeout_minutes", &minutes.to_string());
                        }

                        // Save pause settings. A daily budget of 0 switches
                        // pausing off.
                        if let Some(budget) = changes.pause_budget {
                            set_setting("pause_daily_budget", &budget.to_string());
                            set_setting("pause_enabled", if budget > 0 { "1" } else { "0" });
                        }
                        if let Some(max) = changes.pause_max {
                            set_setting("pause_max_duration", &max.to_string());
                        }
                        if let Some(cooldown) = changes.pause_cooldown {
                            set_setting("pause_cooldown", &cooldown.to_string());
                        }

                        // Save language setting
                        if let Some(index) = changes.language {
                            let lang = match index {
                                1 => Language::German,
                                _ => Language::English,
                            };
                            let changed = lang != i18n::current();
                            i18n::set_language(lang);
                            if changed {
                                refresh_tray_tooltip();
                            }
                        }
                    }

                    // Saving Settings (behind the passcode) is how a parent
                    // accepts a time-zone change, as the alert about it says.
                    crate::database::adopt_current_time_zone();

                    let msg = i18n::wide("settings.success.saved");
                    let title = i18n::wide("settings.success");
                    MessageBoxW(hwnd, PCWSTR(msg.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | MB_ICONINFORMATION);
                    DestroyWindow(hwnd).ok();
                } else if id == ID_SETTINGS_CANCEL {
                    DestroyWindow(hwnd).ok();
                } else if id == ID_TELEGRAM_WIZARD {
                    // Open Telegram setup wizard. If it saved, show what it
                    // saved - otherwise Save would write the fields' old
                    // values back over it - and start the bot with it.
                    if show_telegram_wizard(hwnd) {
                        if let Some(ref handles) = SETTINGS_EDIT_HANDLES {
                            refresh_telegram_fields(handles);
                            mark_bot_fields_loaded(handles, BotKind::Telegram);
                        }
                        crate::session::restart_remote_bots(true, false);
                    }
                    let _ = InvalidateRect(hwnd, None, true);
                } else if id == ID_DISCORD_WIZARD {
                    // Open Discord setup wizard (same as above)
                    if show_discord_wizard(hwnd) {
                        if let Some(ref handles) = SETTINGS_EDIT_HANDLES {
                            refresh_discord_fields(handles);
                            mark_bot_fields_loaded(handles, BotKind::Discord);
                        }
                        crate::session::restart_remote_bots(false, true);
                    }
                    let _ = InvalidateRect(hwnd, None, true);
                }

                LRESULT(0)
            }
            WM_CTLCOLORSTATIC => {
                // The default-passcode warning in red; every other label
                // keeps the default colours.
                let warning = SETTINGS_DEFAULT_PASSCODE_WARNING;
                if !warning.0.is_null() && lparam.0 == warning.0 as isize {
                    let hdc = HDC(wparam.0 as _);
                    SetTextColor(hdc, COLORREF(COLOR_ERROR));
                    SetBkColor(hdc, COLORREF(GetSysColor(COLOR_BTNFACE)));
                    return LRESULT(GetSysColorBrush(COLOR_BTNFACE).0 as isize);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_VSCROLL => {
                let mut info = SCROLLINFO {
                    cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                    fMask: SIF_ALL,
                    ..zeroed()
                };
                let _ = GetScrollInfo(hwnd, SB_VERT, &mut info);
                let page = info.nPage as i32;
                let target = match SCROLLBAR_COMMAND((wparam.0 & 0xFFFF) as i32) {
                    SB_LINEUP => SETTINGS_SCROLL_POS - scale(24),
                    SB_LINEDOWN => SETTINGS_SCROLL_POS + scale(24),
                    SB_PAGEUP => SETTINGS_SCROLL_POS - page,
                    SB_PAGEDOWN => SETTINGS_SCROLL_POS + page,
                    SB_THUMBTRACK | SB_THUMBPOSITION => info.nTrackPos,
                    SB_TOP => 0,
                    SB_BOTTOM => i32::MAX,
                    _ => SETTINGS_SCROLL_POS,
                };
                settings_scroll_to(hwnd, target);
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                if SETTINGS_SCROLLABLE {
                    let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                    settings_scroll_to(hwnd, SETTINGS_SCROLL_POS - delta * scale(72) / 120);
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_CLOSE => {
                DestroyWindow(hwnd).ok();
                LRESULT(0)
            }
            WM_DESTROY => {
                SETTINGS_EDIT_HANDLES = None;
                SETTINGS_LOADED = None;
                SETTINGS_DIALOG_OPEN = false;
                SETTINGS_DEFAULT_PASSCODE_WARNING = HWND::default();
                LRESULT(0)
            }
            WM_NCDESTROY => {
                release_gdi(hwnd);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    register_dialog_class(dialog_class, Some(settings_dialog_proc), 0x00F5F5F5);

    let screen_width = GetSystemMetrics(SM_CXSCREEN);
    let screen_height = GetSystemMetrics(SM_CYSCREEN);
    let dialog_width = scale(400);
    let dialog_height = scale(970);
    SETTINGS_CONTENT_HEIGHT = 0;
    SETTINGS_SCROLLABLE = false;

    let window_title = i18n::wide("window.settings");
    let dialog_hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_DLGMODALFRAME,
        dialog_class,
        PCWSTR(window_title.as_ptr()),
        WS_POPUP | WS_CAPTION | WS_SYSMENU,
        (screen_width - dialog_width) / 2,
        (screen_height - dialog_height) / 2,
        dialog_width,
        dialog_height,
        parent_hwnd,
        HMENU::default(),
        hinstance,
        None,
    );

    if let Ok(dlg) = dialog_hwnd {
        fit_settings_dialog(dlg);
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetForegroundWindow(dlg);
        run_modal_loop(dlg);
    }

    SETTINGS_DIALOG_OPEN = false;
}

/// Client height the Settings controls need (set at the end of WM_CREATE).
static mut SETTINGS_CONTENT_HEIGHT: i32 = 0;
/// How far the Settings content is scrolled up, in pixels.
static mut SETTINGS_SCROLL_POS: i32 = 0;
/// Whether `fit_settings_dialog` had to add a scroll bar.
static mut SETTINGS_SCROLLABLE: bool = false;
/// The red "still the default passcode" label, if shown.
static mut SETTINGS_DEFAULT_PASSCODE_WARNING: HWND = HWND(std::ptr::null_mut());

/// Show the tray icon's tooltip in the language now set (it used to keep
/// the old language until the app restarted). Menus, dialogs and messages
/// read the language each time they're shown; the tooltip is set once.
unsafe fn refresh_tray_tooltip() {
    let nid = std::ptr::addr_of_mut!(crate::tray::NOTIFY_ICON_DATA);
    let Some(nid) = (*nid).as_mut() else {
        return; // no tray icon (yet); it's added with the current language
    };
    nid.szTip = [0; 128];
    for (i, c) in i18n::t("tray.tooltip").encode_utf16().take(127).enumerate() {
        nid.szTip[i] = c;
    }
    let mut modify = *nid;
    modify.uFlags = NIF_TIP;
    let _ = Shell_NotifyIconW(NIM_MODIFY, &modify);
}

/// Set a Settings edit field's text (nothing if the field doesn't exist).
unsafe fn set_field_text(edit: HWND, text: &str) {
    if edit.0.is_null() {
        return;
    }
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    SetWindowTextW(edit, PCWSTR(wide.as_ptr())).ok();
}

/// Tick or untick a Settings checkbox (nothing if it doesn't exist).
unsafe fn set_checkbox(checkbox: HWND, checked: bool) {
    if !checkbox.0.is_null() {
        SendMessageW(checkbox, BM_SETCHECK, WPARAM(usize::from(checked)), LPARAM(0));
    }
}

/// Show the saved Telegram settings in the Settings fields (after the setup
/// wizard saved new ones).
unsafe fn refresh_telegram_fields(handles: &SettingsEditHandles) {
    let config = get_telegram_config();
    set_checkbox(handles.telegram_enabled, config.enabled);
    set_field_text(handles.telegram_token, config.bot_token.as_deref().unwrap_or(""));
    set_field_text(handles.telegram_chat_id, &config.admin_chat_id.map(|id| id.to_string()).unwrap_or_default());
}

/// Show the saved Discord settings in the Settings fields (after the setup
/// wizard saved new ones).
unsafe fn refresh_discord_fields(handles: &SettingsEditHandles) {
    let config = get_discord_config();
    set_checkbox(handles.discord_enabled, config.enabled);
    set_field_text(handles.discord_token, config.bot_token.as_deref().unwrap_or(""));
    set_field_text(handles.discord_channel_id, &config.channel_id.map(|id| id.to_string()).unwrap_or_default());
    set_field_text(handles.discord_admin_user_id, &config.admin_user_id.map(|id| id.to_string()).unwrap_or_default());
}

/// An edit field's text, at most `buffer_len - 1` characters ("" if the
/// field doesn't exist).
unsafe fn field_text(edit: HWND, buffer_len: usize) -> String {
    if edit.0.is_null() {
        return String::new();
    }
    let mut buffer = vec![0u16; buffer_len];
    let len = GetWindowTextW(edit, &mut buffer);
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

/// Whether a Settings checkbox is ticked (`false` if it doesn't exist).
unsafe fn checkbox_checked(checkbox: HWND) -> bool {
    !checkbox.0.is_null() && SendMessageW(checkbox, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == 1
}

/// Characters a minutes field takes where no smaller limit is set (9999
/// minutes at most), so a long number can't be typed in at all.
const MINUTES_FIELD_MAX_CHARS: usize = 4;
/// The longest lock-screen countdown Settings accepts: a day.
const MAX_LOCK_SCREEN_TIMEOUT_MINS: u32 = 24 * 60;

/// What the Settings fields hold, as shown (the passcode fields aside).
/// Read when the dialog opens (`SETTINGS_LOADED`) and again on Save; Save
/// writes only the fields whose text differs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SettingsForm {
    daily_limits: [String; 7],
    warning1_minutes: String,
    warning1_message: String,
    warning2_minutes: String,
    warning2_message: String,
    blocking_message: String,
    rotating_pin: bool,
    telegram_enabled: bool,
    telegram_token: String,
    telegram_chat_id: String,
    discord_enabled: bool,
    discord_token: String,
    discord_channel_id: String,
    discord_admin_user_id: String,
    lock_screen_timeout: String,
    idle_enabled: bool,
    idle_timeout: String,
    pause_budget: String,
    pause_max: String,
    pause_cooldown: String,
    /// The language list's selection (0 English, 1 German; -1 if none).
    language: isize,
}

/// The Settings fields as they were when the dialog opened (bot fields
/// updated after a wizard saved them).
static mut SETTINGS_LOADED: Option<SettingsForm> = None;

/// Read the Settings fields. Each is read with the buffer Save always
/// used for it, so a text is cut where it was before.
unsafe fn read_settings_form(h: &SettingsEditHandles) -> SettingsForm {
    SettingsForm {
        daily_limits: std::array::from_fn(|i| field_text(h.daily_limits[i], 16)),
        warning1_minutes: field_text(h.warning1_minutes, 16),
        warning1_message: field_text(h.warning1_message, 256),
        warning2_minutes: field_text(h.warning2_minutes, 16),
        warning2_message: field_text(h.warning2_message, 256),
        blocking_message: field_text(h.blocking_message, 256),
        rotating_pin: checkbox_checked(h.rotating_pin_enabled),
        telegram_enabled: checkbox_checked(h.telegram_enabled),
        telegram_token: field_text(h.telegram_token, 512),
        telegram_chat_id: field_text(h.telegram_chat_id, 64),
        discord_enabled: checkbox_checked(h.discord_enabled),
        discord_token: field_text(h.discord_token, 512),
        discord_channel_id: field_text(h.discord_channel_id, 64),
        discord_admin_user_id: field_text(h.discord_admin_user_id, 64),
        lock_screen_timeout: field_text(h.lock_screen_timeout, 16),
        idle_enabled: checkbox_checked(h.idle_enabled),
        idle_timeout: field_text(h.idle_timeout_minutes, 16),
        pause_budget: field_text(h.pause_daily_budget, 16),
        pause_max: field_text(h.pause_max_duration, 16),
        pause_cooldown: field_text(h.pause_cooldown, 16),
        language: if h.language.0.is_null() {
            -1
        } else {
            SendMessageW(h.language, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BotKind {
    Telegram,
    Discord,
}

/// After a setup wizard saved and its bot's fields were refreshed: take
/// them as loaded, so Save doesn't count them as the parent's changes.
unsafe fn mark_bot_fields_loaded(handles: &SettingsEditHandles, bot: BotKind) {
    let now = read_settings_form(handles);
    let loaded = std::ptr::addr_of_mut!(SETTINGS_LOADED);
    let Some(loaded) = (*loaded).as_mut() else { return };
    match bot {
        BotKind::Telegram => {
            loaded.telegram_enabled = now.telegram_enabled;
            loaded.telegram_token = now.telegram_token;
            loaded.telegram_chat_id = now.telegram_chat_id;
        }
        BotKind::Discord => {
            loaded.discord_enabled = now.discord_enabled;
            loaded.discord_token = now.discord_token;
            loaded.discord_channel_id = now.discord_channel_id;
            loaded.discord_admin_user_id = now.discord_admin_user_id;
        }
    }
}

/// A Settings field that Save can refuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsField {
    DailyLimit(usize),
    Warning1Minutes,
    Warning1Message,
    Warning2Minutes,
    Warning2Message,
    BlockingMessage,
    TelegramChatId,
    DiscordChannelId,
    DiscordUserId,
    LockScreenTimeout,
    IdleTimeout,
    PauseBudget,
    PauseMax,
    PauseCooldown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsErrorKind {
    /// 0 where at least a minute is needed (the field is then set to 1).
    BelowOneMinute,
    /// Empty, not a whole number, or outside `min..=max` minutes.
    Minutes { min: u32, max: u32 },
    /// A message emptied: it would show a blank warning or lock screen.
    EmptyMessage,
    /// Not a usable chat, channel or user ID.
    BadId,
}

/// Why Save refused, and which field to put the cursor in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SettingsError {
    field: SettingsField,
    kind: SettingsErrorKind,
}

/// Bot settings changed in the dialog (`None`: leave as stored).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TelegramChanges {
    token: Option<String>,
    chat_id: Option<String>,
    enabled: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DiscordChanges {
    token: Option<String>,
    channel_id: Option<String>,
    admin_user_id: Option<String>,
    enabled: Option<bool>,
}

/// What Save writes: only what the parent changed in the dialog, as it is
/// to be stored (`None`: leave the stored value alone - it may have been
/// changed by a bot command since the dialog opened).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SettingsChanges {
    daily_limits: [Option<u32>; 7],
    warning1_minutes: Option<u32>,
    warning1_message: Option<String>,
    warning2_minutes: Option<u32>,
    warning2_message: Option<String>,
    blocking_message: Option<String>,
    rotating_pin: Option<bool>,
    telegram: TelegramChanges,
    discord: DiscordChanges,
    lock_screen_timeout_secs: Option<i32>,
    idle_enabled: Option<bool>,
    idle_timeout: Option<u32>,
    pause_budget: Option<u32>,
    pause_max: Option<u32>,
    pause_cooldown: Option<u32>,
    language: Option<isize>,
}

/// A minutes field's value: a whole number in `min..=max`. `None` for
/// anything else, including an empty field - it used to be saved as "",
/// which the app then read as its default (120 minutes for a daily limit,
/// 5 for a warning) while Settings showed it empty.
fn parse_minutes(text: &str, min: u32, max: u32) -> Option<u32> {
    text.trim().parse::<u32>().ok().filter(|n| (min..=max).contains(n))
}

/// A Telegram chat ID as typed: a whole number other than 0, negative for a
/// group or channel (the field used to take digits only, so a group could
/// only be set up through the wizard).
fn parse_telegram_chat_id(text: &str) -> Option<i64> {
    text.trim().parse::<i64>().ok().filter(|&id| id != 0)
}

/// A new passcode: exactly four digits 0-9, as the bots' set-passcode
/// command requires. Anything else (pasted letters, say) could never be
/// typed into the digits-only passcode prompts again.
fn is_valid_new_passcode(code: &str) -> bool {
    code.len() == 4 && code.bytes().all(|b| b.is_ascii_digit())
}

/// Check the fields the parent changed (`now` against `loaded`) and work
/// out what to store. The first unacceptable field, top to bottom (after
/// the two fields that must be at least a minute, checked first as
/// before), refuses the whole save. Fields left as they were are neither
/// checked nor written. `min_lock` is the shortest lock-screen countdown.
fn plan_settings_save(loaded: &SettingsForm, now: &SettingsForm, min_lock: u32) -> Result<SettingsChanges, SettingsError> {
    use SettingsErrorKind::*;
    use SettingsField::*;
    let refuse = |field, kind| Err(SettingsError { field, kind });

    for (field, before, after) in [
        (LockScreenTimeout, &loaded.lock_screen_timeout, &now.lock_screen_timeout),
        (PauseMax, &loaded.pause_max, &now.pause_max),
    ] {
        if after != before && after.trim().parse::<u32>() == Ok(0) {
            return refuse(field, BelowOneMinute);
        }
    }

    let minutes = |field, before: &String, after: &String, min, max| -> Result<Option<u32>, SettingsError> {
        if after == before {
            return Ok(None);
        }
        parse_minutes(after, min, max).map(Some).ok_or(SettingsError { field, kind: Minutes { min, max } })
    };
    let message = |field, before: &String, after: &String| -> Result<Option<String>, SettingsError> {
        if after == before {
            Ok(None)
        } else if after.trim().is_empty() {
            Err(SettingsError { field, kind: EmptyMessage })
        } else {
            Ok(Some(after.clone()))
        }
    };
    // Tokens and IDs are stored trimmed (a pasted trailing space or newline
    // used to be kept, and the bot then never connected); an emptied field
    // clears the setting, as before.
    let changed_text = |before: &String, after: &String| (after != before).then(|| after.trim().to_string());
    let id = |field, before: &String, after: &String, parse: &dyn Fn(&str) -> Option<String>| -> Result<Option<String>, SettingsError> {
        match changed_text(before, after) {
            None => Ok(None),
            Some(text) if text.is_empty() => Ok(Some(text)),
            Some(text) => parse(&text).map(Some).ok_or(SettingsError { field, kind: BadId }),
        }
    };
    let flag = |before: bool, after: bool| (after != before).then_some(after);

    let mut daily_limits = [None; 7];
    for (i, limit) in daily_limits.iter_mut().enumerate() {
        *limit = minutes(DailyLimit(i), &loaded.daily_limits[i], &now.daily_limits[i], 0, 9999)?;
    }
    let warning1_minutes = minutes(Warning1Minutes, &loaded.warning1_minutes, &now.warning1_minutes, 0, 9999)?;
    let warning1_message = message(Warning1Message, &loaded.warning1_message, &now.warning1_message)?;
    let warning2_minutes = minutes(Warning2Minutes, &loaded.warning2_minutes, &now.warning2_minutes, 0, 9999)?;
    let warning2_message = message(Warning2Message, &loaded.warning2_message, &now.warning2_message)?;
    let blocking_message = message(BlockingMessage, &loaded.blocking_message, &now.blocking_message)?;

    let telegram = TelegramChanges {
        token: changed_text(&loaded.telegram_token, &now.telegram_token),
        chat_id: id(TelegramChatId, &loaded.telegram_chat_id, &now.telegram_chat_id, &|t| {
            parse_telegram_chat_id(t).map(|n| n.to_string())
        })?,
        enabled: flag(loaded.telegram_enabled, now.telegram_enabled),
    };
    let discord = DiscordChanges {
        token: changed_text(&loaded.discord_token, &now.discord_token),
        channel_id: id(DiscordChannelId, &loaded.discord_channel_id, &now.discord_channel_id, &|t| {
            parse_discord_id(t).map(|n| n.to_string())
        })?,
        admin_user_id: id(DiscordUserId, &loaded.discord_admin_user_id, &now.discord_admin_user_id, &|t| {
            parse_discord_id(t).map(|n| n.to_string())
        })?,
        enabled: flag(loaded.discord_enabled, now.discord_enabled),
    };

    let lock_screen_timeout_secs = minutes(
        LockScreenTimeout,
        &loaded.lock_screen_timeout,
        &now.lock_screen_timeout,
        min_lock,
        MAX_LOCK_SCREEN_TIMEOUT_MINS,
    )?
    .and_then(lock_screen_timeout_secs);
    // 0 minutes of idle time is taken as 1, as before.
    let idle_timeout = minutes(IdleTimeout, &loaded.idle_timeout, &now.idle_timeout, 0, 999)?.map(|m| m.max(1));
    let pause_budget = minutes(PauseBudget, &loaded.pause_budget, &now.pause_budget, 0, 999)?;
    let pause_max = minutes(PauseMax, &loaded.pause_max, &now.pause_max, 1, 999)?;
    let pause_cooldown = minutes(PauseCooldown, &loaded.pause_cooldown, &now.pause_cooldown, 0, 999)?;

    Ok(SettingsChanges {
        daily_limits,
        warning1_minutes,
        warning1_message,
        warning2_minutes,
        warning2_message,
        blocking_message,
        rotating_pin: flag(loaded.rotating_pin, now.rotating_pin),
        telegram,
        discord,
        lock_screen_timeout_secs,
        idle_enabled: flag(loaded.idle_enabled, now.idle_enabled),
        idle_timeout,
        pause_budget,
        pause_max,
        pause_cooldown,
        language: (now.language != loaded.language && now.language >= 0).then_some(now.language),
    })
}

/// The Telegram settings to store: the parent's changes over the stored
/// `config`. `None` if nothing changed.
fn merged_telegram(changes: &TelegramChanges, config: &crate::database::TelegramConfig) -> Option<(String, String, bool)> {
    if *changes == TelegramChanges::default() {
        return None;
    }
    Some((
        changes.token.clone().unwrap_or_else(|| config.bot_token.clone().unwrap_or_default()),
        changes.chat_id.clone().unwrap_or_else(|| config.admin_chat_id.map(|id| id.to_string()).unwrap_or_default()),
        changes.enabled.unwrap_or(config.enabled),
    ))
}

/// The Discord settings to store: the parent's changes over the stored
/// `config`. `None` if nothing changed.
fn merged_discord(changes: &DiscordChanges, config: &crate::database::DiscordConfig) -> Option<(String, String, String, bool)> {
    if *changes == DiscordChanges::default() {
        return None;
    }
    let stored_id = |id: Option<u64>| id.map(|id| id.to_string()).unwrap_or_default();
    Some((
        changes.token.clone().unwrap_or_else(|| config.bot_token.clone().unwrap_or_default()),
        changes.channel_id.clone().unwrap_or_else(|| stored_id(config.channel_id)),
        changes.admin_user_id.clone().unwrap_or_else(|| stored_id(config.admin_user_id)),
        changes.enabled.unwrap_or(config.enabled),
    ))
}

/// The edit control of a refusable Settings field.
fn settings_field_edit(h: &SettingsEditHandles, field: SettingsField) -> HWND {
    match field {
        SettingsField::DailyLimit(i) => h.daily_limits.get(i).copied().unwrap_or_default(),
        SettingsField::Warning1Minutes => h.warning1_minutes,
        SettingsField::Warning1Message => h.warning1_message,
        SettingsField::Warning2Minutes => h.warning2_minutes,
        SettingsField::Warning2Message => h.warning2_message,
        SettingsField::BlockingMessage => h.blocking_message,
        SettingsField::TelegramChatId => h.telegram_chat_id,
        SettingsField::DiscordChannelId => h.discord_channel_id,
        SettingsField::DiscordUserId => h.discord_admin_user_id,
        SettingsField::LockScreenTimeout => h.lock_screen_timeout,
        SettingsField::IdleTimeout => h.idle_timeout_minutes,
        SettingsField::PauseBudget => h.pause_daily_budget,
        SettingsField::PauseMax => h.pause_max_duration,
        SettingsField::PauseCooldown => h.pause_cooldown,
    }
}

/// The i18n key of the message for a refused Save.
fn settings_error_key(error: &SettingsError) -> &'static str {
    match (error.kind, error.field) {
        (SettingsErrorKind::BelowOneMinute, SettingsField::PauseMax) => "settings.error.pause_max_min",
        (SettingsErrorKind::BelowOneMinute, _) => "settings.error.shutdown_min",
        (SettingsErrorKind::Minutes { .. }, _) => "settings.error.minutes_range",
        (SettingsErrorKind::EmptyMessage, _) => "settings.error.message_empty",
        (SettingsErrorKind::BadId, SettingsField::TelegramChatId) => "settings.error.telegram_chat_id",
        (SettingsErrorKind::BadId, SettingsField::DiscordUserId) => "settings.error.discord_user_id",
        (SettingsErrorKind::BadId, _) => "settings.error.discord_channel_id",
    }
}

/// The message for a refused Save, in the current language.
fn settings_error_text(error: &SettingsError) -> String {
    let text = i18n::t(settings_error_key(error));
    match error.kind {
        SettingsErrorKind::Minutes { min, max } => {
            text.replace("{min}", &min.to_string()).replace("{max}", &max.to_string())
        }
        _ => text.to_string(),
    }
}

/// Refuse a Settings save: show `message` and put the cursor in `edit`
/// with its text selected.
unsafe fn show_settings_field_error(hwnd: HWND, edit: HWND, message: &str) {
    let msg = i18n::to_wide(message);
    let title = i18n::wide("settings.error");
    MessageBoxW(hwnd, PCWSTR(msg.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | MB_ICONERROR);
    if !edit.0.is_null() {
        let _ = SetFocus(edit);
        SendMessageW(edit, EM_SETSEL, WPARAM(0), LPARAM(-1));
    }
}

/// The lock-screen countdown to store for `minutes`, in seconds: at least
/// the minimum, and `None` beyond what Settings accepts (never wrapped).
fn lock_screen_timeout_secs(minutes: u32) -> Option<i32> {
    if minutes > MAX_LOCK_SCREEN_TIMEOUT_MINS {
        return None;
    }
    i32::try_from(minutes).ok()?.max(crate::database::MIN_LOCK_SCREEN_TIMEOUT_MINS).checked_mul(60)
}

/// A Discord channel or user ID as typed: a number other than 0 (serenity's
/// ID types panic on 0, and this build aborts on a panic).
fn parse_discord_id(text: &str) -> Option<std::num::NonZeroU64> {
    text.trim().parse().ok()
}

/// Size the Settings window to its content and keep it on the monitor.
///
/// If the content fits the work area (the usual case at 1080p and 100%),
/// the window is simply sized to the content and centred in the work area.
/// If not (e.g. 1366x768, or 1080p at 125%), the window is made as tall as
/// the work area, gets a vertical scroll bar (and is widened by the bar's
/// width so no control is covered), and the content scrolls - so Save is
/// always reachable.
unsafe fn fit_settings_dialog(dlg: HWND) {
    let content_height = SETTINGS_CONTENT_HEIGHT;
    if content_height <= 0 {
        return; // WM_CREATE didn't finish; leave the window as created.
    }

    let mut client: RECT = zeroed();
    let mut window: RECT = zeroed();
    if GetClientRect(dlg, &mut client).is_err() || GetWindowRect(dlg, &mut window).is_err() {
        return;
    }
    // Non-client size (caption, borders) as Windows actually drew it.
    let frame_width = (window.right - window.left) - client.right;
    let frame_height = (window.bottom - window.top) - client.bottom;

    let mut monitor_info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..zeroed() };
    let monitor = MonitorFromWindow(dlg, MONITOR_DEFAULTTOPRIMARY);
    if !GetMonitorInfoW(monitor, &mut monitor_info).as_bool() {
        return;
    }
    let work = monitor_info.rcWork;
    let work_width = work.right - work.left;
    let work_height = work.bottom - work.top;

    let layout = settings_window_layout(client.right, content_height, frame_width, frame_height, work_height, GetSystemMetrics(SM_CXVSCROLL));
    let x = work.left + ((work_width - layout.width) / 2).max(0);
    let y = work.top + ((work_height - layout.height) / 2).max(0);

    if layout.scroll {
        SETTINGS_SCROLLABLE = true;
        let style = GetWindowLongPtrW(dlg, GWL_STYLE);
        SetWindowLongPtrW(dlg, GWL_STYLE, style | WS_VSCROLL.0 as isize);
    }
    let _ = SetWindowPos(dlg, None, x, y, layout.width, layout.height, SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);

    if layout.scroll {
        let mut client_after: RECT = zeroed();
        let _ = GetClientRect(dlg, &mut client_after);
        let info = SCROLLINFO {
            cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
            fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
            nMin: 0,
            nMax: content_height - 1,
            nPage: client_after.bottom.max(1) as u32,
            nPos: 0,
            nTrackPos: 0,
        };
        SetScrollInfo(dlg, SB_VERT, &info, true);
    }
}

/// Outer size of the Settings window, and whether it scrolls.
#[derive(Debug, PartialEq, Eq)]
struct SettingsWindowLayout {
    width: i32,
    height: i32,
    scroll: bool,
}

/// Pure part of `fit_settings_dialog`: size the window to `content_height`
/// of client area, or, when that won't fit in `work_height`, to the work
/// area's height with a scroll bar (widening the window by its width).
fn settings_window_layout(
    client_width: i32,
    content_height: i32,
    frame_width: i32,
    frame_height: i32,
    work_height: i32,
    scrollbar_width: i32,
) -> SettingsWindowLayout {
    let wanted_height = content_height + frame_height;
    if wanted_height <= work_height {
        SettingsWindowLayout { width: client_width + frame_width, height: wanted_height, scroll: false }
    } else {
        SettingsWindowLayout {
            width: client_width + frame_width + scrollbar_width,
            height: work_height,
            scroll: true,
        }
    }
}

/// Scroll the Settings content so that `target` (clamped) is at the top.
unsafe fn settings_scroll_to(hwnd: HWND, target: i32) {
    if !SETTINGS_SCROLLABLE {
        return;
    }
    let mut client: RECT = zeroed();
    let _ = GetClientRect(hwnd, &mut client);
    let max_pos = (SETTINGS_CONTENT_HEIGHT - client.bottom).max(0);
    let new_pos = target.clamp(0, max_pos);
    let dy = SETTINGS_SCROLL_POS - new_pos;
    if dy == 0 {
        return;
    }
    SETTINGS_SCROLL_POS = new_pos;
    ScrollWindowEx(hwnd, 0, dy, None, None, None, None, SW_SCROLLCHILDREN | SW_INVALIDATE | SW_ERASE);
    let info = SCROLLINFO {
        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_POS,
        nPos: new_pos,
        ..zeroed()
    };
    SetScrollInfo(hwnd, SB_VERT, &info, true);
}

// ============================================================================
// Setup wizards on small or highly scaled screens
// ============================================================================

/// A setup wizard that didn't fit on the screen and scrolls instead (see
/// `fit_wizard_dialog`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WizardScroll {
    /// Client height the wizard's pages and buttons are laid out for.
    content_height: i32,
    /// How far the content is scrolled up, in pixels.
    pos: i32,
    /// The Cancel/Back/Next buttons, kept at the bottom of the window
    /// whatever the scroll position.
    buttons: [i32; 3],
}

thread_local! {
    /// Scrolling wizards, by window.
    static WIZARD_SCROLL: RefCell<Vec<(usize, WizardScroll)>> = const { RefCell::new(Vec::new()) };
}

fn wizard_scroll(hwnd: HWND) -> Option<WizardScroll> {
    WIZARD_SCROLL.with(|s| s.borrow().iter().find(|(h, _)| *h == hwnd.0 as usize).map(|(_, state)| *state))
}

fn set_wizard_scroll(hwnd: HWND, state: Option<WizardScroll>) {
    WIZARD_SCROLL.with(|s| {
        let mut s = s.borrow_mut();
        s.retain(|(h, _)| *h != hwnd.0 as usize);
        if let Some(state) = state {
            s.push((hwnd.0 as usize, state));
        }
    });
}

/// How far a wizard's content is scrolled up (0 if it doesn't scroll), for
/// placing a control created while it is scrolled.
fn wizard_scroll_pos(hwnd: HWND) -> i32 {
    wizard_scroll(hwnd).map_or(0, |s| s.pos)
}

/// Start painting a wizard page: if the wizard scrolls, draw in content
/// coordinates (shifted by the scroll position) on a client rectangle as
/// tall as the content, so the pages paint exactly as when it fits.
unsafe fn begin_wizard_paint(hwnd: HWND, hdc: HDC, rect: &mut RECT) {
    if let Some(state) = wizard_scroll(hwnd) {
        rect.bottom = state.content_height;
        let _ = SetViewportOrgEx(hdc, 0, -state.pos, None);
    }
}

/// Keep a setup wizard on the monitor's work area, so its buttons can
/// always be reached.
///
/// The wizards are created at a fixed size (scaled with the DPI) centred on
/// the whole screen, taskbar included. At 175-200% on a 1080p or 1200p
/// display that is taller than the work area, and the Back/Next/Finish row
/// at the bottom ended up behind the taskbar or off the screen. As Settings
/// does (`fit_settings_dialog`, same arithmetic in `settings_window_layout`):
/// a wizard that fits keeps its size and is only moved if it hangs off the
/// work area; one that doesn't is made as tall as the work area, widened by
/// a vertical scroll bar, and its page (laid out for the original height)
/// scrolls, while its row of `buttons` stays at the bottom of the window,
/// where it is when the page is scrolled to the end.
unsafe fn fit_wizard_dialog(dlg: HWND, buttons: [i32; 3]) {
    let mut client: RECT = zeroed();
    let mut window: RECT = zeroed();
    if GetClientRect(dlg, &mut client).is_err() || GetWindowRect(dlg, &mut window).is_err() {
        return;
    }
    let width = window.right - window.left;
    let height = window.bottom - window.top;
    let frame_width = width - client.right;
    let frame_height = height - client.bottom;
    let content_height = client.bottom;

    let mut monitor_info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..zeroed() };
    let monitor = MonitorFromWindow(dlg, MONITOR_DEFAULTTOPRIMARY);
    if !GetMonitorInfoW(monitor, &mut monitor_info).as_bool() {
        return;
    }
    let work = monitor_info.rcWork;

    let layout = settings_window_layout(
        client.right,
        content_height,
        frame_width,
        frame_height,
        work.bottom - work.top,
        GetSystemMetrics(SM_CXVSCROLL),
    );
    if !layout.scroll {
        let (x, y) = position_within(window.left, window.top, width, height, &work);
        if (x, y) != (window.left, window.top) {
            let _ = SetWindowPos(dlg, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
        return;
    }

    let state = WizardScroll { content_height, pos: 0, buttons };
    set_wizard_scroll(dlg, Some(state));
    let style = GetWindowLongPtrW(dlg, GWL_STYLE);
    SetWindowLongPtrW(dlg, GWL_STYLE, style | WS_VSCROLL.0 as isize);
    let x = work.left + ((work.right - work.left - layout.width) / 2).max(0);
    let _ = SetWindowPos(dlg, None, x, work.top, layout.width, layout.height, SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
    pin_wizard_buttons(dlg, &state);

    let mut client_after: RECT = zeroed();
    let _ = GetClientRect(dlg, &mut client_after);
    let info = SCROLLINFO {
        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
        nMin: 0,
        nMax: content_height - 1,
        nPage: client_after.bottom.max(1) as u32,
        nPos: 0,
        nTrackPos: 0,
    };
    SetScrollInfo(dlg, SB_VERT, &info, true);
}

/// Put a scrolling wizard's buttons at the bottom of its window, the same
/// distance from the bottom edge as in the layout (`create_wizard_buttons`).
unsafe fn pin_wizard_buttons(hwnd: HWND, state: &WizardScroll) {
    let mut client: RECT = zeroed();
    if GetClientRect(hwnd, &mut client).is_err() {
        return;
    }
    for id in state.buttons {
        let Ok(button) = GetDlgItem(hwnd, id) else { continue };
        let mut r: RECT = zeroed();
        if GetWindowRect(button, &mut r).is_err() {
            continue;
        }
        let mut corner = POINT { x: r.left, y: r.top };
        let _ = ScreenToClient(hwnd, &mut corner);
        let _ = SetWindowPos(button, None, corner.x, client.bottom - scale(60), 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// Top-left corner for a `width` x `height` window at (`left`, `top`),
/// moved as little as needed to lie within `work` (to its top-left if it
/// is bigger than `work`).
fn position_within(left: i32, top: i32, width: i32, height: i32, work: &RECT) -> (i32, i32) {
    let x = left.min(work.right - width).max(work.left);
    let y = top.min(work.bottom - height).max(work.top);
    (x, y)
}

/// WM_VSCROLL and WM_MOUSEWHEEL for a setup wizard. `false` if the wizard
/// doesn't scroll (the message is then left to DefWindowProc).
unsafe fn handle_wizard_scroll(hwnd: HWND, msg: u32, wparam: WPARAM) -> bool {
    let Some(state) = wizard_scroll(hwnd) else {
        return false;
    };
    let target = if msg == WM_VSCROLL {
        let mut info = SCROLLINFO {
            cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
            fMask: SIF_ALL,
            ..zeroed()
        };
        let _ = GetScrollInfo(hwnd, SB_VERT, &mut info);
        let page = info.nPage as i32;
        match SCROLLBAR_COMMAND((wparam.0 & 0xFFFF) as i32) {
            SB_LINEUP => state.pos - scale(24),
            SB_LINEDOWN => state.pos + scale(24),
            SB_PAGEUP => state.pos - page,
            SB_PAGEDOWN => state.pos + page,
            SB_THUMBTRACK | SB_THUMBPOSITION => info.nTrackPos,
            SB_TOP => 0,
            SB_BOTTOM => i32::MAX,
            _ => state.pos,
        }
    } else {
        let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
        state.pos - delta * scale(72) / 120
    };

    let mut client: RECT = zeroed();
    let _ = GetClientRect(hwnd, &mut client);
    let new_pos = target.clamp(0, (state.content_height - client.bottom).max(0));
    let dy = state.pos - new_pos;
    if dy != 0 {
        let state = WizardScroll { pos: new_pos, ..state };
        set_wizard_scroll(hwnd, Some(state));
        ScrollWindowEx(hwnd, 0, dy, None, None, None, None, SW_SCROLLCHILDREN | SW_INVALIDATE | SW_ERASE);
        pin_wizard_buttons(hwnd, &state);
        let info = SCROLLINFO {
            cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
            fMask: SIF_POS,
            nPos: new_pos,
            ..zeroed()
        };
        SetScrollInfo(hwnd, SB_VERT, &info, true);
    }
    true
}

/// Show today's stats dialog
pub unsafe fn show_stats_dialog(parent_hwnd: HWND) {
    let dialog_class = w!("ScreenTimeStatsDialog");
    let hinstance = GetModuleHandleW(None).expect("Failed to get module handle");

    static mut STATS_DIALOG_OPEN: bool = false;

    if STATS_DIALOG_OPEN {
        return;
    }
    STATS_DIALOG_OPEN = true;

    unsafe extern "system" fn stats_dialog_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        use crate::blocking::REMAINING_SECONDS;
        use crate::database::{get_current_weekday, get_daily_limit};
        use std::sync::atomic::Ordering;

        const ID_RESET_TIMER: i32 = 3001;
        const ID_CLOSE: i32 = 3002;

        match msg {
            WM_CREATE => {
                let hinstance = GetModuleHandleW(None).unwrap();

                let btn_font = track_gdi(hwnd, CreateFontW(
                    scale(14), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"),
                ));

                // Reset Timer button
                let reset_btn_text = i18n::wide("button.reset_timer");
                let reset_btn = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("BUTTON"), PCWSTR(reset_btn_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                    scale(50), scale(310), scale(120), scale(35), hwnd, HMENU(ID_RESET_TIMER as _), hinstance, None,
                );
                if let Ok(h) = reset_btn { SendMessageW(h, WM_SETFONT, WPARAM(btn_font.0 as usize), LPARAM(1)); }

                // Close button
                let close_btn_text = i18n::wide("button.close");
                let close_btn = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("BUTTON"), PCWSTR(close_btn_text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                    scale(190), scale(310), scale(100), scale(35), hwnd, HMENU(ID_CLOSE as _), hinstance, None,
                );
                if let Ok(h) = close_btn { SendMessageW(h, WM_SETFONT, WPARAM(btn_font.0 as usize), LPARAM(1)); }

                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);

                let mut rect: RECT = zeroed();
                GetClientRect(hwnd, &mut rect).ok();

                let bg_brush = CreateSolidBrush(COLORREF(0x00F5F5F5));
                FillRect(hdc, &rect, bg_brush);
                let _ = DeleteObject(bg_brush);

                // Get stats
                let weekday = get_current_weekday();
                let daily_limit_minutes = get_daily_limit(weekday);
                let remaining_seconds = REMAINING_SECONDS.load(Ordering::SeqCst);
                // The time actually counted today - not limit minus
                // remaining, which any extension, reduction or reset threw off
                let used_seconds = crate::mini_overlay::SESSION_ACTIVE_SECONDS.load(Ordering::SeqCst);

                // Get pause stats
                let pause_enabled = is_pause_enabled();
                let pause_config = get_pause_config();
                let pause_used_seconds = get_pause_used_today();
                let pause_budget_seconds = (pause_config.daily_budget_minutes * 60) as i32;
                let pause_remaining_seconds = (pause_budget_seconds - pause_used_seconds).max(0);
                let pause_log = get_pause_log_today();

                // Format time helper
                fn format_duration(seconds: i32) -> String {
                    if seconds < 0 {
                        return String::from("--");
                    }
                    let hours = seconds / 3600;
                    let minutes = (seconds % 3600) / 60;
                    let secs = seconds % 60;
                    if hours > 0 {
                        format!("{}h {}m {}s", hours, minutes, secs)
                    } else if minutes > 0 {
                        format!("{}m {}s", minutes, secs)
                    } else {
                        format!("{}s", secs)
                    }
                }

                // Get weekday name
                let weekday_name = i18n::weekday(weekday as usize);

                // Title font (DPI scaled, ClearType quality = 5)
                let title_font = CreateFontW(
                    scale(20), 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"),
                );
                let section_font = CreateFontW(
                    scale(14), 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"),
                );
                let label_font = CreateFontW(
                    scale(13), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"),
                );
                let value_font = CreateFontW(
                    scale(14), 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"),
                );
                let small_font = CreateFontW(
                    scale(12), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"),
                );

                let old_font = SelectObject(hdc, title_font);
                SetTextColor(hdc, COLORREF(0x00333333));
                SetBkMode(hdc, TRANSPARENT);

                // Title
                let mut title_rect = RECT { left: 0, top: scale(15), right: rect.right, bottom: scale(42) };
                let stats_title: Vec<u16> = i18n::t("stats.title").encode_utf16().collect();
                DrawTextW(
                    hdc,
                    &mut stats_title.clone(),
                    &mut title_rect,
                    DT_CENTER | DT_SINGLELINE,
                );

                // Stats
                let mut y = scale(50);
                let left_margin = scale(25);
                let value_x = scale(160);

                // Day
                SelectObject(hdc, label_font);
                SetTextColor(hdc, COLORREF(0x00666666));
                let mut label_rect = RECT { left: left_margin, top: y, right: value_x, bottom: y + scale(22) };
                let day_label: Vec<u16> = i18n::t("stats.day").encode_utf16().collect();
                DrawTextW(hdc, &mut day_label.clone(), &mut label_rect, DT_SINGLELINE);

                SelectObject(hdc, value_font);
                SetTextColor(hdc, COLORREF(0x00333333));
                let day_str = weekday_name.to_string();
                let mut value_rect = RECT { left: value_x, top: y, right: rect.right - scale(15), bottom: y + scale(22) };
                DrawTextW(hdc, &mut day_str.encode_utf16().collect::<Vec<_>>(), &mut value_rect, DT_SINGLELINE);
                y += scale(24);

                // Daily limit
                SelectObject(hdc, label_font);
                SetTextColor(hdc, COLORREF(0x00666666));
                let mut label_rect = RECT { left: left_margin, top: y, right: value_x, bottom: y + scale(22) };
                DrawTextW(hdc, &mut i18n::t("stats.daily_limit").encode_utf16().collect::<Vec<_>>(), &mut label_rect, DT_SINGLELINE);

                SelectObject(hdc, value_font);
                SetTextColor(hdc, COLORREF(0x00333333));
                let limit_str = format!("{} {}", daily_limit_minutes, i18n::t("stats.minutes"));
                let mut value_rect = RECT { left: value_x, top: y, right: rect.right - scale(15), bottom: y + scale(22) };
                DrawTextW(hdc, &mut limit_str.encode_utf16().collect::<Vec<_>>(), &mut value_rect, DT_SINGLELINE);
                y += scale(24);

                // Time used
                SelectObject(hdc, label_font);
                SetTextColor(hdc, COLORREF(0x00666666));
                let mut label_rect = RECT { left: left_margin, top: y, right: value_x, bottom: y + scale(22) };
                DrawTextW(hdc, &mut i18n::t("stats.time_used").encode_utf16().collect::<Vec<_>>(), &mut label_rect, DT_SINGLELINE);

                SelectObject(hdc, value_font);
                SetTextColor(hdc, COLORREF(0x00333333));
                let used_str = format_duration(used_seconds.max(0));
                let mut value_rect = RECT { left: value_x, top: y, right: rect.right - scale(15), bottom: y + scale(22) };
                DrawTextW(hdc, &mut used_str.encode_utf16().collect::<Vec<_>>(), &mut value_rect, DT_SINGLELINE);
                y += scale(24);

                // Time remaining
                SelectObject(hdc, label_font);
                SetTextColor(hdc, COLORREF(0x00666666));
                let mut label_rect = RECT { left: left_margin, top: y, right: value_x, bottom: y + scale(22) };
                DrawTextW(hdc, &mut i18n::t("stats.time_remaining").encode_utf16().collect::<Vec<_>>(), &mut label_rect, DT_SINGLELINE);

                SelectObject(hdc, value_font);
                // Color based on remaining time
                if remaining_seconds <= 60 {
                    SetTextColor(hdc, COLORREF(COLOR_ERROR));
                } else if remaining_seconds <= 300 {
                    SetTextColor(hdc, COLORREF(COLOR_ACCENT));
                } else {
                    SetTextColor(hdc, COLORREF(0x00008800)); // Green
                }
                let remaining_str = format_duration(remaining_seconds);
                let mut value_rect = RECT { left: value_x, top: y, right: rect.right - scale(15), bottom: y + scale(22) };
                DrawTextW(hdc, &mut remaining_str.encode_utf16().collect::<Vec<_>>(), &mut value_rect, DT_SINGLELINE);
                y += scale(32);

                // ===== Pause Section =====
                SelectObject(hdc, section_font);
                SetTextColor(hdc, COLORREF(0x00333333));
                let mut section_rect = RECT { left: left_margin, top: y, right: rect.right - scale(15), bottom: y + scale(20) };
                DrawTextW(hdc, &mut i18n::t("stats.pause_mode").encode_utf16().collect::<Vec<_>>(), &mut section_rect, DT_SINGLELINE);
                y += scale(22);

                if pause_enabled {
                    // Pause budget used
                    SelectObject(hdc, label_font);
                    SetTextColor(hdc, COLORREF(0x00666666));
                    let mut label_rect = RECT { left: left_margin, top: y, right: value_x, bottom: y + scale(22) };
                    DrawTextW(hdc, &mut i18n::t("stats.pause_used").encode_utf16().collect::<Vec<_>>(), &mut label_rect, DT_SINGLELINE);

                    SelectObject(hdc, value_font);
                    SetTextColor(hdc, COLORREF(0x00333333));
                    let pause_used_str = format!("{} / {} {}", pause_used_seconds / 60, pause_config.daily_budget_minutes, i18n::t("stats.minutes"));
                    let mut value_rect = RECT { left: value_x, top: y, right: rect.right - scale(15), bottom: y + scale(22) };
                    DrawTextW(hdc, &mut pause_used_str.encode_utf16().collect::<Vec<_>>(), &mut value_rect, DT_SINGLELINE);
                    y += scale(24);

                    // Pause remaining
                    SelectObject(hdc, label_font);
                    SetTextColor(hdc, COLORREF(0x00666666));
                    let mut label_rect = RECT { left: left_margin, top: y, right: value_x, bottom: y + scale(22) };
                    DrawTextW(hdc, &mut i18n::t("stats.pause_remaining").encode_utf16().collect::<Vec<_>>(), &mut label_rect, DT_SINGLELINE);

                    SelectObject(hdc, value_font);
                    if pause_remaining_seconds <= 0 {
                        SetTextColor(hdc, COLORREF(COLOR_ERROR));
                    } else if pause_remaining_seconds <= 300 {
                        SetTextColor(hdc, COLORREF(COLOR_ACCENT));
                    } else {
                        SetTextColor(hdc, COLORREF(0x00008800)); // Green
                    }
                    let pause_remaining_str = format_duration(pause_remaining_seconds);
                    let mut value_rect = RECT { left: value_x, top: y, right: rect.right - scale(15), bottom: y + scale(22) };
                    DrawTextW(hdc, &mut pause_remaining_str.encode_utf16().collect::<Vec<_>>(), &mut value_rect, DT_SINGLELINE);
                    y += scale(24);

                    // Pause count
                    SelectObject(hdc, label_font);
                    SetTextColor(hdc, COLORREF(0x00666666));
                    let mut label_rect = RECT { left: left_margin, top: y, right: value_x, bottom: y + scale(22) };
                    DrawTextW(hdc, &mut i18n::t("stats.pauses_today").encode_utf16().collect::<Vec<_>>(), &mut label_rect, DT_SINGLELINE);

                    SelectObject(hdc, value_font);
                    SetTextColor(hdc, COLORREF(0x00333333));
                    let pause_count_str = format!("{}", pause_log.len());
                    let mut value_rect = RECT { left: value_x, top: y, right: rect.right - scale(15), bottom: y + scale(22) };
                    DrawTextW(hdc, &mut pause_count_str.encode_utf16().collect::<Vec<_>>(), &mut value_rect, DT_SINGLELINE);
                    y += scale(24);

                    // Pause log (if any)
                    if !pause_log.is_empty() {
                        SelectObject(hdc, small_font);
                        SetTextColor(hdc, COLORREF(0x00888888));
                        let log_str = format!("{} {}", i18n::t("stats.log"), pause_log.join(", "));
                        let mut log_rect = RECT { left: left_margin, top: y, right: rect.right - scale(15), bottom: y + scale(18) };
                        DrawTextW(hdc, &mut log_str.encode_utf16().collect::<Vec<_>>(), &mut log_rect, DT_SINGLELINE);
                    }
                } else {
                    SelectObject(hdc, label_font);
                    SetTextColor(hdc, COLORREF(0x00888888));
                    let mut disabled_rect = RECT { left: left_margin, top: y, right: rect.right - scale(15), bottom: y + scale(22) };
                    DrawTextW(hdc, &mut i18n::t("stats.pause_disabled").encode_utf16().collect::<Vec<_>>(), &mut disabled_rect, DT_SINGLELINE);
                }

                SelectObject(hdc, old_font);
                let _ = DeleteObject(title_font);
                let _ = DeleteObject(section_font);
                let _ = DeleteObject(label_font);
                let _ = DeleteObject(value_font);
                let _ = DeleteObject(small_font);

                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;

                if id == ID_RESET_TIMER {
                    // Reset to daily limit (saved at once), as the bot's
                    // reset does: the lock screen goes (the reset also ends
                    // a manual lock), and the admin is told
                    let remaining = crate::blocking::reset_to_daily_limit();
                    if remaining > 0 && crate::blocking::is_blocking_overlay_visible() {
                        crate::blocking::hide_blocking_overlay();
                    }
                    crate::blocking::notify_local_reset("reset.source.stats", remaining);

                    let msg = i18n::wide("stats.timer_reset");
                    let title = i18n::wide("stats.timer_reset_title");
                    MessageBoxW(hwnd, PCWSTR(msg.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | MB_ICONINFORMATION);
                    let _ = InvalidateRect(hwnd, None, true);
                } else if id == ID_CLOSE {
                    DestroyWindow(hwnd).ok();
                }

                LRESULT(0)
            }
            WM_CLOSE => {
                DestroyWindow(hwnd).ok();
                LRESULT(0)
            }
            WM_DESTROY => {
                STATS_DIALOG_OPEN = false;
                LRESULT(0)
            }
            WM_NCDESTROY => {
                release_gdi(hwnd);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    register_dialog_class(dialog_class, Some(stats_dialog_proc), 0x00F5F5F5);

    let screen_width = GetSystemMetrics(SM_CXSCREEN);
    let screen_height = GetSystemMetrics(SM_CYSCREEN);
    let dialog_width = scale(340);
    let dialog_height = scale(390);

    let window_title = i18n::wide("window.stats");
    let dialog_hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_DLGMODALFRAME,
        dialog_class,
        PCWSTR(window_title.as_ptr()),
        WS_POPUP | WS_CAPTION | WS_SYSMENU,
        (screen_width - dialog_width) / 2,
        (screen_height - dialog_height) / 2,
        dialog_width,
        dialog_height,
        parent_hwnd,
        HMENU::default(),
        hinstance,
        None,
    );

    if let Ok(dlg) = dialog_hwnd {
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetForegroundWindow(dlg);
        run_modal_loop(dlg);
    }

    STATS_DIALOG_OPEN = false;
}

// ============================================================================
// Telegram Setup Wizard
// ============================================================================

// Wizard control IDs
const ID_WIZARD_NEXT: i32 = 3001;
const ID_WIZARD_BACK: i32 = 3002;
const ID_WIZARD_CANCEL: i32 = 3003;
const ID_WIZARD_TOKEN_EDIT: i32 = 3004;

// Wizard state
static mut WIZARD_STEP: i32 = 1;
static mut WIZARD_TOKEN: Option<String> = None;
static mut WIZARD_POLLING: bool = false;
/// Whether this run of the wizard saved a configuration.
static mut WIZARD_SAVED: bool = false;
/// The wizard window, read by the polling thread to request a repaint.
static WIZARD_HWND: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
/// The chat the bot most recently heard from (written by the polling
/// thread, read on the UI thread).
static WIZARD_CHAT: Mutex<Option<DetectedChat>> = Mutex::new(None);
/// Bumped each time detection (re)starts, so a reply to an older poll can't
/// overwrite a newer detection.
static WIZARD_POLL_GENERATION: AtomicU32 = AtomicU32::new(0);
const WIZARD_TOTAL_STEPS: i32 = 5;
const TIMER_POLL_TELEGRAM: usize = 100;

/// A Telegram chat found by the wizard, with a name so the parent can see
/// whose chat is about to become the admin chat.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DetectedChat {
    id: i64,
    name: String,
}

fn wizard_chat() -> Option<DetectedChat> {
    WIZARD_CHAT.lock().ok().and_then(|c| c.clone())
}

fn set_wizard_chat(chat: Option<DetectedChat>) {
    if let Ok(mut c) = WIZARD_CHAT.lock() {
        *c = chat;
    }
}

/// Show the Telegram setup wizard. Returns whether it saved a configuration
/// (it does so on reaching its last step, whether or not Finish is clicked).
pub unsafe fn show_telegram_wizard(parent_hwnd: HWND) -> bool {
    // One at a time: the wizard's state is shared (statics below).
    let open = HWND(WIZARD_HWND.load(AtomicOrdering::SeqCst));
    if !open.0.is_null() && IsWindow(open).as_bool() {
        let _ = SetForegroundWindow(open);
        return false;
    }

    // Reset wizard state
    WIZARD_STEP = 1;
    WIZARD_TOKEN = None;
    set_wizard_chat(None);
    WIZARD_POLLING = false;
    WIZARD_SAVED = false;
    WIZARD_PAUSED_BOT = false;

    let hinstance = GetModuleHandleW(None).unwrap();
    let wizard_class = w!("ScreenTimeTelegramWizard");

    // Register wizard window class
    register_dialog_class(wizard_class, Some(wizard_proc), 0x00FFFFFF);

    let screen_width = GetSystemMetrics(SM_CXSCREEN);
    let screen_height = GetSystemMetrics(SM_CYSCREEN);
    let dialog_width = scale(480);
    let dialog_height = scale(520);

    let window_title = i18n::wide("wizard.title");
    let wizard_hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_DLGMODALFRAME,
        wizard_class,
        PCWSTR(window_title.as_ptr()),
        WS_POPUP | WS_CAPTION | WS_SYSMENU,
        (screen_width - dialog_width) / 2,
        (screen_height - dialog_height) / 2,
        dialog_width,
        dialog_height,
        parent_hwnd,
        HMENU::default(),
        hinstance,
        None,
    );

    if let Ok(hwnd) = wizard_hwnd {
        WIZARD_HWND.store(hwnd.0, AtomicOrdering::SeqCst);
        fit_wizard_dialog(hwnd, [ID_WIZARD_CANCEL, ID_WIZARD_BACK, ID_WIZARD_NEXT]);
        run_owned_modal(hwnd, parent_hwnd);
    }

    WIZARD_HWND.store(std::ptr::null_mut(), AtomicOrdering::SeqCst);
    // Start the bot the wizard paused again, unless Settings will (it
    // restarts it after a save) or is gone (the app is quitting).
    if WIZARD_PAUSED_BOT && !WIZARD_SAVED && IsWindow(parent_hwnd).as_bool() {
        crate::session::restart_remote_bots(true, false);
    }
    WIZARD_PAUSED_BOT = false;
    WIZARD_SAVED
}

/// Wizard window procedure
unsafe extern "system" fn wizard_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            create_wizard_buttons(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            paint_wizard(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            handle_wizard_command(hwnd, id);
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_POLL_TELEGRAM {
                poll_telegram_for_chatid(hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // Close only this wizard (see run_modal_loop).
            close_modal(hwnd);
            LRESULT(0)
        }
        WM_VSCROLL | WM_MOUSEWHEEL if handle_wizard_scroll(hwnd, msg, wparam) => LRESULT(0),
        WM_DESTROY => {
            // Also when Settings, its owner, is destroyed.
            end_modal(hwnd);
            KillTimer(hwnd, TIMER_POLL_TELEGRAM).ok();
            WIZARD_POLLING = false;
            LRESULT(0)
        }
        WM_NCDESTROY => {
            set_wizard_scroll(hwnd, None);
            release_gdi(hwnd);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Create wizard navigation buttons
unsafe fn create_wizard_buttons(hwnd: HWND) {
    let hinstance = GetModuleHandleW(None).unwrap();

    let mut rect: RECT = zeroed();
    GetClientRect(hwnd, &mut rect).ok();
    let width = rect.right;
    let height = rect.bottom;

    let btn_width = scale(100);
    let btn_height = scale(36);
    let btn_y = height - scale(60);
    let margin = scale(20);

    // Button font
    let btn_font = track_gdi(hwnd, CreateFontW(
        scale(15), 0, 0, 0,
        FW_NORMAL.0 as i32,
        0, 0, 0, 0, 0, 0, 5, 0,
        w!("Segoe UI"),
    ));

    // Cancel button (left)
    let cancel_text = i18n::wide("wizard.cancel");
    let cancel_btn = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        PCWSTR(cancel_text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
        margin,
        btn_y,
        btn_width,
        btn_height,
        hwnd,
        HMENU(ID_WIZARD_CANCEL as _),
        hinstance,
        None,
    );
    if let Ok(h) = cancel_btn {
        SendMessageW(h, WM_SETFONT, WPARAM(btn_font.0 as usize), LPARAM(1));
    }

    // Back button
    let back_text = i18n::wide("wizard.back");
    let back_btn = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        PCWSTR(back_text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
        width - margin - btn_width * 2 - scale(10),
        btn_y,
        btn_width,
        btn_height,
        hwnd,
        HMENU(ID_WIZARD_BACK as _),
        hinstance,
        None,
    );
    if let Ok(h) = back_btn {
        SendMessageW(h, WM_SETFONT, WPARAM(btn_font.0 as usize), LPARAM(1));
    }

    // Next/Finish button
    let next_text = i18n::wide("wizard.next");
    let next_btn = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        PCWSTR(next_text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
        width - margin - btn_width,
        btn_y,
        btn_width,
        btn_height,
        hwnd,
        HMENU(ID_WIZARD_NEXT as _),
        hinstance,
        None,
    );
    if let Ok(h) = next_btn {
        SendMessageW(h, WM_SETFONT, WPARAM(btn_font.0 as usize), LPARAM(1));
    }
}

/// Paint the wizard content based on current step
unsafe fn paint_wizard(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut rect: RECT = zeroed();
    GetClientRect(hwnd, &mut rect).ok();
    begin_wizard_paint(hwnd, hdc, &mut rect);
    let width = rect.right;

    // Background
    let bg_brush = CreateSolidBrush(COLORREF(0x00FFFFFF));
    FillRect(hdc, &rect, bg_brush);
    let _ = DeleteObject(bg_brush);

    // Header area with gradient-like color
    let header_rect = RECT { left: 0, top: 0, right: width, bottom: scale(100) };
    let header_brush = CreateSolidBrush(COLORREF(0x00F5E6D3)); // Warm beige
    FillRect(hdc, &header_rect, header_brush);
    let _ = DeleteObject(header_brush);

    SetBkMode(hdc, TRANSPARENT);

    // Draw step indicator circles
    draw_step_indicators(hdc, width, WIZARD_STEP);

    // Content area
    let content_top = scale(110);
    let content_rect = RECT {
        left: scale(30),
        top: content_top,
        right: width - scale(30),
        bottom: rect.bottom - scale(80),
    };

    match WIZARD_STEP {
        1 => paint_step_welcome(hdc, &content_rect),
        2 => paint_step_botfather(hdc, &content_rect),
        3 => paint_step_token(hdc, hwnd, &content_rect),
        4 => paint_step_connect(hdc, &content_rect),
        5 => paint_step_success(hdc, &content_rect),
        _ => {}
    }

    // Update button states
    update_wizard_buttons(hwnd);

    let _ = EndPaint(hwnd, &ps);
}

/// Draw step indicator circles at the top
unsafe fn draw_step_indicators(hdc: HDC, width: i32, current_step: i32) {
    let center_y = scale(50);
    let circle_radius = scale(16);
    let spacing = scale(70);
    let start_x = (width - (spacing * 4)) / 2;

    // Font for step numbers
    let num_font = CreateFontW(
        scale(14), 0, 0, 0,
        FW_BOLD.0 as i32,
        0, 0, 0, 0, 0, 0, 5, 0,
        w!("Segoe UI"),
    );
    let old_font = SelectObject(hdc, num_font);

    for i in 1..=5 {
        let cx = start_x + (i - 1) * spacing;

        // Draw connecting line (except for first circle)
        if i > 1 {
            let line_color = if i <= current_step {
                COLORREF(0x004CAF50) // Green
            } else {
                COLORREF(0x00CCCCCC) // Gray
            };
            let pen = CreatePen(PS_SOLID, scale(2), line_color);
            let old_pen = SelectObject(hdc, pen);
            let _ = MoveToEx(hdc, cx - spacing + circle_radius, center_y, None);
            let _ = LineTo(hdc, cx - circle_radius, center_y);
            SelectObject(hdc, old_pen);
            let _ = DeleteObject(pen);
        }

        // Circle color based on state
        let (fill_color, text_color) = if i < current_step {
            (COLORREF(0x004CAF50), COLORREF(0x00FFFFFF)) // Completed: green
        } else if i == current_step {
            (COLORREF(0x002196F3), COLORREF(0x00FFFFFF)) // Current: blue
        } else {
            (COLORREF(0x00EEEEEE), COLORREF(0x00666666)) // Future: gray
        };

        // Draw circle
        let brush = CreateSolidBrush(fill_color);
        let pen = CreatePen(PS_SOLID, 1, fill_color);
        let old_brush = SelectObject(hdc, brush);
        let old_pen = SelectObject(hdc, pen);
        let _ = Ellipse(hdc, cx - circle_radius, center_y - circle_radius,
                cx + circle_radius, center_y + circle_radius);
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(brush);
        let _ = DeleteObject(pen);

        // Draw number or checkmark
        SetTextColor(hdc, text_color);
        let text = if i < current_step {
            "✓".to_string()
        } else {
            i.to_string()
        };
        let mut text_rect = RECT {
            left: cx - circle_radius,
            top: center_y - circle_radius,
            right: cx + circle_radius,
            bottom: center_y + circle_radius,
        };
        DrawTextW(hdc, &mut text.encode_utf16().collect::<Vec<_>>(), &mut text_rect,
                  DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }

    SelectObject(hdc, old_font);
    let _ = DeleteObject(num_font);
}

/// Step 1: Welcome screen
unsafe fn paint_step_welcome(hdc: HDC, rect: &RECT) {
    let title_font = CreateFontW(scale(28), 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let desc_font = CreateFontW(scale(16), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let icon_font = CreateFontW(scale(48), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI Emoji"));

    let mut y = rect.top;

    // Phone icon
    SelectObject(hdc, icon_font);
    SetTextColor(hdc, COLORREF(0x002196F3));
    let mut icon_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(60) };
    DrawTextW(hdc, &mut "📱".encode_utf16().collect::<Vec<_>>(), &mut icon_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(70);

    // Title
    SelectObject(hdc, title_font);
    SetTextColor(hdc, COLORREF(0x00333333));
    let mut title_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(35) };
    DrawTextW(hdc, &mut i18n::t("wizard.welcome.title").encode_utf16().collect::<Vec<_>>(), &mut title_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(45);

    // Description
    SelectObject(hdc, desc_font);
    SetTextColor(hdc, COLORREF(0x00666666));
    let mut desc_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(25) };
    DrawTextW(hdc, &mut i18n::t("wizard.welcome.desc1").encode_utf16().collect::<Vec<_>>(), &mut desc_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(40);

    // Features with icons
    let features = [
        ("✓", i18n::t("wizard.welcome.feature1")),
        ("✓", i18n::t("wizard.welcome.feature2")),
        ("✓", i18n::t("wizard.welcome.feature3")),
        ("✓", i18n::t("wizard.welcome.feature4")),
    ];

    SetTextColor(hdc, COLORREF(0x004CAF50));
    for (icon, text) in features.iter() {
        let feature_text = format!("  {}  {}", icon, text);
        let mut feature_rect = RECT { left: rect.left + scale(40), top: y, right: rect.right, bottom: y + scale(28) };
        DrawTextW(hdc, &mut feature_text.encode_utf16().collect::<Vec<_>>(), &mut feature_rect, DT_SINGLELINE);
        y += scale(30);
    }

    y += scale(20);

    // Ready message
    SetTextColor(hdc, COLORREF(0x00333333));
    let mut ready_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(25) };
    DrawTextW(hdc, &mut i18n::t("wizard.welcome.ready").encode_utf16().collect::<Vec<_>>(), &mut ready_rect, DT_CENTER | DT_SINGLELINE);

    deselect_font(hdc);
    let _ = DeleteObject(title_font);
    let _ = DeleteObject(desc_font);
    let _ = DeleteObject(icon_font);
}

/// Step 2: BotFather instructions
unsafe fn paint_step_botfather(hdc: HDC, rect: &RECT) {
    let title_font = CreateFontW(scale(24), 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let step_font = CreateFontW(scale(15), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let icon_font = CreateFontW(scale(40), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI Emoji"));
    let hint_font = CreateFontW(scale(13), 0, 0, 0, FW_NORMAL.0 as i32, 1, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));

    let mut y = rect.top;

    // Robot icon
    SelectObject(hdc, icon_font);
    SetTextColor(hdc, COLORREF(0x002196F3));
    let mut icon_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(50) };
    DrawTextW(hdc, &mut "🤖".encode_utf16().collect::<Vec<_>>(), &mut icon_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(55);

    // Title
    SelectObject(hdc, title_font);
    SetTextColor(hdc, COLORREF(0x00333333));
    let mut title_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(30) };
    DrawTextW(hdc, &mut i18n::t("wizard.bot.title").encode_utf16().collect::<Vec<_>>(), &mut title_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(45);

    // Steps
    SelectObject(hdc, step_font);
    let steps = [
        i18n::t("wizard.bot.step1"),
        i18n::t("wizard.bot.step2"),
        i18n::t("wizard.bot.step3"),
        i18n::t("wizard.bot.step4"),
        i18n::t("wizard.bot.step5"),
        i18n::t("wizard.bot.step6"),
    ];

    for step in steps.iter() {
        SetTextColor(hdc, COLORREF(0x00444444));
        let mut step_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(24) };
        DrawTextW(hdc, &mut step.encode_utf16().collect::<Vec<_>>(), &mut step_rect, DT_SINGLELINE);
        y += scale(28);
    }

    y += scale(15);

    // Hint box
    let hint_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(40) };
    let hint_brush = CreateSolidBrush(COLORREF(0x00FFF3E0)); // Light orange
    FillRect(hdc, &hint_rect, hint_brush);
    let _ = DeleteObject(hint_brush);

    SelectObject(hdc, hint_font);
    SetTextColor(hdc, COLORREF(0x00E65100));
    let mut hint_text_rect = RECT { left: rect.left + scale(10), top: y + scale(10), right: rect.right - scale(10), bottom: y + scale(35) };
    DrawTextW(hdc, &mut i18n::t("wizard.bot.hint").encode_utf16().collect::<Vec<_>>(), &mut hint_text_rect, DT_CENTER | DT_SINGLELINE);

    deselect_font(hdc);
    let _ = DeleteObject(title_font);
    let _ = DeleteObject(step_font);
    let _ = DeleteObject(icon_font);
    let _ = DeleteObject(hint_font);
}

/// Step 3: Token entry
unsafe fn paint_step_token(hdc: HDC, hwnd: HWND, rect: &RECT) {
    let title_font = CreateFontW(scale(24), 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let label_font = CreateFontW(scale(15), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let icon_font = CreateFontW(scale(40), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI Emoji"));

    let mut y = rect.top;

    // Key icon
    SelectObject(hdc, icon_font);
    SetTextColor(hdc, COLORREF(0x00FF9800));
    let mut icon_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(50) };
    DrawTextW(hdc, &mut "🔑".encode_utf16().collect::<Vec<_>>(), &mut icon_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(55);

    // Title
    SelectObject(hdc, title_font);
    SetTextColor(hdc, COLORREF(0x00333333));
    let mut title_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(30) };
    DrawTextW(hdc, &mut i18n::t("wizard.token.title").encode_utf16().collect::<Vec<_>>(), &mut title_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(50);

    // Label
    SelectObject(hdc, label_font);
    SetTextColor(hdc, COLORREF(0x00666666));
    let mut label_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(22) };
    DrawTextW(hdc, &mut i18n::t("wizard.token.label").encode_utf16().collect::<Vec<_>>(), &mut label_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(30);

    // Create/update edit control for token
    let edit_hwnd = GetDlgItem(hwnd, ID_WIZARD_TOKEN_EDIT).unwrap_or_default();
    if edit_hwnd.0.is_null() {
        let hinstance = GetModuleHandleW(None).unwrap();
        let edit_width = scale(350);
        let edit_x = (rect.right - rect.left - edit_width) / 2 + rect.left;

        let _ = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_CENTER as u32 | ES_AUTOHSCROLL as u32),
            edit_x,
            y - wizard_scroll_pos(hwnd),
            edit_width,
            scale(35),
            hwnd,
            HMENU(ID_WIZARD_TOKEN_EDIT as _),
            hinstance,
            None,
        );

        let edit_font = track_gdi(hwnd, CreateFontW(scale(14), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Consolas")));
        let new_edit = GetDlgItem(hwnd, ID_WIZARD_TOKEN_EDIT).unwrap_or_default();
        if !new_edit.0.is_null() {
            SendMessageW(new_edit, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));

            // Set placeholder text if we have a saved token
            if let Some(ref token) = WIZARD_TOKEN {
                let wide: Vec<u16> = token.encode_utf16().chain(std::iter::once(0)).collect();
                SetWindowTextW(new_edit, PCWSTR(wide.as_ptr())).ok();
            }
        }
    }

    y += scale(55);

    // Validation status
    let wizard_token_ref = std::ptr::addr_of!(WIZARD_TOKEN);
    let token_valid = (*wizard_token_ref).as_ref().map(|t| is_valid_token_format(t)).unwrap_or(false);
    if (*wizard_token_ref).is_some() {
        let (status_text, status_color) = if token_valid {
            (format!("✓ {}", i18n::t("wizard.token.valid")), COLORREF(0x004CAF50))
        } else {
            (format!("✗ {}", i18n::t("wizard.token.invalid")), COLORREF(0x00F44336))
        };
        SetTextColor(hdc, status_color);
        let mut status_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(22) };
        DrawTextW(hdc, &mut status_text.encode_utf16().collect::<Vec<_>>(), &mut status_rect, DT_CENTER | DT_SINGLELINE);
    }

    deselect_font(hdc);
    let _ = DeleteObject(title_font);
    let _ = DeleteObject(label_font);
    let _ = DeleteObject(icon_font);
}

/// Step 4: Connect to bot
unsafe fn paint_step_connect(hdc: HDC, rect: &RECT) {
    let title_font = CreateFontW(scale(24), 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let step_font = CreateFontW(scale(15), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let icon_font = CreateFontW(scale(40), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI Emoji"));
    let status_font = CreateFontW(scale(18), 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));

    let mut y = rect.top;

    // Link icon
    SelectObject(hdc, icon_font);
    SetTextColor(hdc, COLORREF(0x009C27B0));
    let mut icon_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(50) };
    DrawTextW(hdc, &mut "🔗".encode_utf16().collect::<Vec<_>>(), &mut icon_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(55);

    // Title
    SelectObject(hdc, title_font);
    SetTextColor(hdc, COLORREF(0x00333333));
    let mut title_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(30) };
    DrawTextW(hdc, &mut i18n::t("wizard.connect.title").encode_utf16().collect::<Vec<_>>(), &mut title_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(50);

    // Steps
    SelectObject(hdc, step_font);
    let steps = [
        i18n::t("wizard.connect.step1"),
        i18n::t("wizard.connect.step2"),
        i18n::t("wizard.connect.step3"),
    ];

    for step in steps.iter() {
        SetTextColor(hdc, COLORREF(0x00444444));
        let mut step_rect = RECT { left: rect.left + scale(30), top: y, right: rect.right, bottom: y + scale(24) };
        DrawTextW(hdc, &mut step.encode_utf16().collect::<Vec<_>>(), &mut step_rect, DT_SINGLELINE);
        y += scale(32);
    }

    y += scale(30);

    // Status
    SelectObject(hdc, status_font);
    if let Some(chat) = wizard_chat() {
        // Connected!
        SetTextColor(hdc, COLORREF(0x004CAF50));
        let detected = format!("✓ {}", i18n::t("wizard.connect.detected"));
        let mut status_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(25) };
        DrawTextW(hdc, &mut detected.encode_utf16().collect::<Vec<_>>(), &mut status_rect, DT_CENTER | DT_SINGLELINE);
        y += scale(30);

        // Whose chat it is, so the parent can tell it's their own before
        // it becomes the admin chat.
        SelectObject(hdc, step_font);
        SetTextColor(hdc, COLORREF(0x00333333));
        let chat_text = format!("{} {}  ({} {})", i18n::t("wizard.connect.chat"), chat.name, i18n::t("wizard.connect.id"), chat.id);
        let mut chat_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(22) };
        DrawTextW(hdc, &mut chat_text.encode_utf16().collect::<Vec<_>>(), &mut chat_rect, DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        y += scale(26);

        let hint_font = CreateFontW(scale(13), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
        SelectObject(hdc, hint_font);
        SetTextColor(hdc, COLORREF(0x00666666));
        let mut hint_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(20) };
        DrawTextW(hdc, &mut i18n::t("wizard.connect.not_you_again").encode_utf16().collect::<Vec<_>>(), &mut hint_rect, DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        SelectObject(hdc, status_font);
        let _ = DeleteObject(hint_font);
    } else if let Some(error) = WIZARD_POLL_STATUS.lock().ok().and_then(|s| s.visible_error().cloned()) {
        // Telegram refuses, or can't be reached: say so (detection keeps
        // trying), instead of the waiting message.
        let mut text = i18n::t(poll_error_key(&error)).to_string();
        if let Some(detail) = poll_error_detail(&error) {
            text.push('\n');
            text.push_str(&detail);
        }
        let error_font = CreateFontW(scale(13), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
        SelectObject(hdc, error_font);
        SetTextColor(hdc, COLORREF(COLOR_ERROR));
        // Room down to just above the buttons (they start 60 px above the
        // bottom; this page's status line is at 341 of 488 at 100%).
        let mut error_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(84) };
        DrawTextW(hdc, &mut text.encode_utf16().collect::<Vec<_>>(), &mut error_rect, DT_CENTER | DT_WORDBREAK);
        SelectObject(hdc, status_font);
        let _ = DeleteObject(error_font);
    } else {
        // Waiting...
        SetTextColor(hdc, COLORREF(0x00FF9800));
        let waiting_anim = match (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() % 4) as usize {
            0 => "⏳",
            1 => "⌛",
            2 => "⏳",
            _ => "⌛",
        };
        let waiting_text = format!("{} {}", waiting_anim, i18n::t("wizard.connect.waiting"));
        let mut status_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(25) };
        DrawTextW(hdc, &mut waiting_text.encode_utf16().collect::<Vec<_>>(), &mut status_rect, DT_CENTER | DT_SINGLELINE);
    }

    deselect_font(hdc);
    let _ = DeleteObject(title_font);
    let _ = DeleteObject(step_font);
    let _ = DeleteObject(icon_font);
    let _ = DeleteObject(status_font);
}

/// Step 5: Success
unsafe fn paint_step_success(hdc: HDC, rect: &RECT) {
    let title_font = CreateFontW(scale(28), 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let desc_font = CreateFontW(scale(15), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let cmd_font = CreateFontW(scale(13), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Consolas"));
    let icon_font = CreateFontW(scale(56), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI Emoji"));

    let mut y = rect.top;

    // Checkmark icon
    SelectObject(hdc, icon_font);
    SetTextColor(hdc, COLORREF(0x004CAF50));
    let mut icon_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(65) };
    DrawTextW(hdc, &mut "✅".encode_utf16().collect::<Vec<_>>(), &mut icon_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(75);

    // Title
    SelectObject(hdc, title_font);
    SetTextColor(hdc, COLORREF(0x004CAF50));
    let mut title_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(35) };
    DrawTextW(hdc, &mut i18n::t("wizard.success.title").encode_utf16().collect::<Vec<_>>(), &mut title_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(45);

    // Description
    SelectObject(hdc, desc_font);
    SetTextColor(hdc, COLORREF(0x00666666));
    let mut desc_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(22) };
    DrawTextW(hdc, &mut i18n::t("wizard.success.desc").encode_utf16().collect::<Vec<_>>(), &mut desc_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(25);

    let mut test_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(22) };
    DrawTextW(hdc, &mut i18n::t("wizard.success.test").encode_utf16().collect::<Vec<_>>(), &mut test_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(40);

    // Commands section
    SetTextColor(hdc, COLORREF(0x00333333));
    let mut cmd_title_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(22) };
    DrawTextW(hdc, &mut i18n::t("wizard.success.commands").encode_utf16().collect::<Vec<_>>(), &mut cmd_title_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(30);

    // Command examples with background
    SelectObject(hdc, cmd_font);
    let commands = [
        i18n::t("wizard.success.cmd1"),
        i18n::t("wizard.success.cmd2"),
        i18n::t("wizard.success.cmd3"),
        i18n::t("wizard.success.cmd4"),
    ];

    let cmd_bg = CreateSolidBrush(COLORREF(0x00F5F5F5));
    for cmd in commands.iter() {
        let cmd_rect = RECT { left: rect.left + scale(20), top: y, right: rect.right - scale(20), bottom: y + scale(22) };
        FillRect(hdc, &cmd_rect, cmd_bg);
        SetTextColor(hdc, COLORREF(0x00333333));
        let mut text_rect = RECT { left: rect.left + scale(30), top: y + scale(2), right: rect.right - scale(30), bottom: y + scale(20) };
        DrawTextW(hdc, &mut cmd.encode_utf16().collect::<Vec<_>>(), &mut text_rect, DT_SINGLELINE);
        y += scale(26);
    }
    let _ = DeleteObject(cmd_bg);

    deselect_font(hdc);
    let _ = DeleteObject(title_font);
    let _ = DeleteObject(desc_font);
    let _ = DeleteObject(cmd_font);
    let _ = DeleteObject(icon_font);
}

/// Check if token format is valid (basic check)
fn is_valid_token_format(token: &str) -> bool {
    // Telegram bot tokens are like: 123456789:ABCdefGHI-jklMNO_pqr
    let parts: Vec<&str> = token.split(':').collect();
    if parts.len() != 2 {
        return false;
    }
    // First part should be numeric (bot ID)
    if parts[0].parse::<i64>().is_err() {
        return false;
    }
    // Second part should be alphanumeric with some special chars
    parts[1].len() >= 30 && parts[1].chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// Update wizard button states based on current step
unsafe fn update_wizard_buttons(hwnd: HWND) {
    let back_btn = GetDlgItem(hwnd, ID_WIZARD_BACK).unwrap_or_default();
    let next_btn = GetDlgItem(hwnd, ID_WIZARD_NEXT).unwrap_or_default();

    // Back button: disabled on step 1
    if !back_btn.0.is_null() {
        let _ = EnableWindow(back_btn, WIZARD_STEP > 1);
    }

    // Next button text and state
    if !next_btn.0.is_null() {
        let next_text = if WIZARD_STEP == WIZARD_TOTAL_STEPS {
            i18n::wide("wizard.finish")
        } else {
            i18n::wide("wizard.next")
        };
        SetWindowTextW(next_btn, PCWSTR(next_text.as_ptr())).ok();

        // Disable next on step 3 if token invalid, step 4 if not connected
        let wizard_token_ref = std::ptr::addr_of!(WIZARD_TOKEN);
        let can_proceed = match WIZARD_STEP {
            3 => (*wizard_token_ref).as_ref().map(|t| is_valid_token_format(t)).unwrap_or(false),
            4 => wizard_chat().is_some(),
            _ => true,
        };
        let _ = EnableWindow(next_btn, can_proceed);
    }
}

/// Handle wizard command (button clicks)
unsafe fn handle_wizard_command(hwnd: HWND, id: i32) {
    match id {
        ID_WIZARD_CANCEL => {
            close_modal(hwnd);
        }
        ID_WIZARD_BACK => {
            if WIZARD_STEP > 1 {
                // Stop polling if we're leaving step 4
                if WIZARD_STEP == 4 {
                    KillTimer(hwnd, TIMER_POLL_TELEGRAM).ok();
                    WIZARD_POLLING = false;
                }
                WIZARD_STEP -= 1;
                // Destroy token edit if going back from step 3
                if WIZARD_STEP == 2 {
                    let edit = GetDlgItem(hwnd, ID_WIZARD_TOKEN_EDIT).unwrap_or_default();
                    if !edit.0.is_null() {
                        DestroyWindow(edit).ok();
                    }
                }
                let _ = InvalidateRect(hwnd, None, true);
            }
        }
        ID_WIZARD_NEXT => {
            // Save token from edit control on step 3
            if WIZARD_STEP == 3 {
                let edit = GetDlgItem(hwnd, ID_WIZARD_TOKEN_EDIT).unwrap_or_default();
                if !edit.0.is_null() {
                    let mut buffer = [0u16; 256];
                    let len = GetWindowTextW(edit, &mut buffer);
                    let token = String::from_utf16_lossy(&buffer[..len as usize]);
                    WIZARD_TOKEN = Some(token.trim().to_string());
                }
            }

            if WIZARD_STEP < WIZARD_TOTAL_STEPS {
                WIZARD_STEP += 1;

                // Start polling on step 4, detecting afresh each time the
                // step is entered: only a message sent to the bot after
                // this point counts (see wizard_poll_once), so after "Back"
                // a wrong chat can be replaced by messaging from the right one.
                if WIZARD_STEP == 4 {
                    set_wizard_chat(None);
                    WIZARD_POLL_GENERATION.fetch_add(1, AtomicOrdering::SeqCst);
                    if let Ok(mut status) = WIZARD_POLL_STATUS.lock() {
                        *status = PollStatus::new();
                    }
                    let wizard_token_ref = std::ptr::addr_of!(WIZARD_TOKEN);
                    if let Some(token) = (*wizard_token_ref).as_deref() {
                        pause_own_bot_for(token);
                    }
                }
                if WIZARD_STEP == 4 && !WIZARD_POLLING {
                    WIZARD_POLLING = true;
                    SetTimer(hwnd, TIMER_POLL_TELEGRAM, 2000, None);
                }
                if WIZARD_STEP == 4 {
                    // Note where the updates stand right away, not at the
                    // first timer tick.
                    poll_telegram_for_chatid(hwnd);
                }

                // Destroy token edit when leaving step 3
                if WIZARD_STEP == 4 {
                    let edit = GetDlgItem(hwnd, ID_WIZARD_TOKEN_EDIT).unwrap_or_default();
                    if !edit.0.is_null() {
                        DestroyWindow(edit).ok();
                    }
                }

                // On step 5, save config and send test message
                if WIZARD_STEP == 5 {
                    KillTimer(hwnd, TIMER_POLL_TELEGRAM).ok();
                    WIZARD_POLLING = false;
                    save_wizard_config();
                }

                let _ = InvalidateRect(hwnd, None, true);
            } else {
                // Finish
                close_modal(hwnd);
            }
        }
        ID_WIZARD_TOKEN_EDIT => {
            // Token edit changed - update validation
            let edit = GetDlgItem(hwnd, ID_WIZARD_TOKEN_EDIT).unwrap_or_default();
            if !edit.0.is_null() {
                let mut buffer = [0u16; 256];
                let len = GetWindowTextW(edit, &mut buffer);
                let token = String::from_utf16_lossy(&buffer[..len as usize]);
                WIZARD_TOKEN = Some(token.trim().to_string());
                let _ = InvalidateRect(hwnd, None, true);
            }
        }
        _ => {}
    }
}

/// Poll Telegram for incoming messages to detect chat ID
unsafe fn poll_telegram_for_chatid(hwnd: HWND) {
    if !WIZARD_POLLING || wizard_chat().is_some() {
        return;
    }

    let wizard_token_ref = std::ptr::addr_of!(WIZARD_TOKEN);
    if let Some(token) = (*wizard_token_ref).clone() {
        // One request at a time, on a thread (the API call blocks).
        if !WIZARD_POLL_IN_FLIGHT.swap(true, AtomicOrdering::SeqCst) {
            let generation = WIZARD_POLL_GENERATION.load(AtomicOrdering::SeqCst);
            std::thread::spawn(move || {
                wizard_poll_once(&token, generation);
                WIZARD_POLL_IN_FLIGHT.store(false, AtomicOrdering::SeqCst);
            });
        }
    }

    // Trigger repaint for animation
    let _ = InvalidateRect(hwnd, None, true);
}

/// A poll request is under way (see `poll_telegram_for_chatid`).
static WIZARD_POLL_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// Where the bot's updates stood when detection (re)started, for the
/// detection generation it was taken in: the newest update id then (`None`
/// if there were none). Only updates after it are taken as the parent's.
static WIZARD_BASELINE: Mutex<Option<(u32, Option<i64>)>> = Mutex::new(None);

/// One step of chat detection for detection generation `generation`: the
/// first successful request only notes where the bot's updates stand (so a
/// message that was already waiting - from anyone, or the wrong chat picked
/// up before "Back" - is never taken); later ones look for a newer one.
fn wizard_poll_once(token: &str, generation: u32) {
    let current = || WIZARD_POLL_GENERATION.load(AtomicOrdering::SeqCst) == generation;
    let baseline = WIZARD_BASELINE
        .lock()
        .ok()
        .and_then(|b| *b)
        .filter(|(g, _)| *g == generation)
        .map(|(_, newest)| newest);
    let result = match baseline {
        None => fetch_newest_update_id(token).map(|newest| {
            if current() {
                if let Ok(mut b) = WIZARD_BASELINE.lock() {
                    *b = Some((generation, newest));
                }
            }
        }),
        Some(after) => fetch_chat_after(token, after).map(|chat| {
            if let Some(chat) = chat {
                if current() {
                    set_wizard_chat(Some(chat));
                }
            }
        }),
    };
    if current() {
        if let Ok(mut status) = WIZARD_POLL_STATUS.lock() {
            status.record(result);
        }
        let wizard_hwnd = WIZARD_HWND.load(AtomicOrdering::SeqCst);
        if !wizard_hwnd.is_null() {
            let _ = unsafe { InvalidateRect(HWND(wizard_hwnd), None, true) };
        }
    }
}

/// Why a request to Telegram for the wizard failed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PollError {
    /// Telegram answered with an error, e.g. 401 for a revoked or mistyped
    /// token, 409 when something else is reading the bot's messages.
    Api { code: u16, description: String },
    /// No answer (offline, timeout), or not one Telegram would give.
    Network,
}

/// How the wizard's requests to Telegram are going, for the "connect"
/// page. The wizard used to show only "waiting for your message" whatever
/// happened, so a revoked or mistyped token, or a second reader of the
/// bot's messages, meant waiting forever with no clue why.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PollStatus {
    /// Requests in a row that failed (any success resets it).
    failures: u32,
    /// The latest failure.
    last_error: Option<PollError>,
}

impl PollStatus {
    const fn new() -> Self {
        Self { failures: 0, last_error: None }
    }

    fn record(&mut self, result: Result<(), PollError>) {
        match result {
            Ok(()) => *self = Self::new(),
            Err(e) => {
                self.failures = self.failures.saturating_add(1);
                self.last_error = Some(e);
            }
        }
    }

    /// The error to show, if any. A token Telegram refuses (401, 404) is
    /// shown at once - it won't fix itself; anything else only once it has
    /// failed three times in a row (about 6 seconds), so a single dropped
    /// request, or the moment this PC's own bot takes to stop, doesn't flash
    /// an error.
    fn visible_error(&self) -> Option<&PollError> {
        let error = self.last_error.as_ref()?;
        let definite = matches!(error, PollError::Api { code: 401 | 404, .. });
        (definite || self.failures >= 3).then_some(error)
    }
}

static WIZARD_POLL_STATUS: Mutex<PollStatus> = Mutex::new(PollStatus::new());

/// The i18n key of the advice shown for `error`.
fn poll_error_key(error: &PollError) -> &'static str {
    match error {
        PollError::Api { code: 401 | 404, .. } => "wizard.connect.error.token",
        PollError::Api { code: 409, .. } => "wizard.connect.error.conflict",
        PollError::Api { .. } => "wizard.connect.error.other",
        PollError::Network => "wizard.connect.error.network",
    }
}

/// What Telegram said, for showing under the advice: "Telegram: 401
/// Unauthorized", shortened to fit. `None` without an answer from Telegram.
fn poll_error_detail(error: &PollError) -> Option<String> {
    const MAX_CHARS: usize = 90;
    let PollError::Api { code, description } = error else {
        return None;
    };
    let full = format!("Telegram: {code} {}", description.trim());
    let full = full.trim_end();
    if full.chars().count() <= MAX_CHARS {
        return Some(full.to_string());
    }
    let cut: String = full.chars().take(MAX_CHARS - 3).collect();
    Some(format!("{}...", cut.trim_end()))
}

/// Most updates one `getUpdates` call returns (Telegram's default limit).
const TELEGRAM_UPDATES_PAGE: usize = 100;

fn telegram_get_updates(token: &str, query: &str) -> Result<Vec<TelegramUpdate>, PollError> {
    let url = format!("https://api.telegram.org/bot{token}/getUpdates?timeout=0{query}");
    match ureq::get(&url).timeout(std::time::Duration::from_secs(3)).call() {
        Ok(response) => {
            let status = response.status();
            let body = response.into_string().map_err(|_| PollError::Network)?;
            parse_updates_response(status, &body)
        }
        // Telegram's error replies (4xx) carry its explanation in the body.
        Err(ureq::Error::Status(status, response)) => {
            parse_updates_response(status, &response.into_string().unwrap_or_default())
        }
        Err(ureq::Error::Transport(_)) => Err(PollError::Network),
    }
}

/// The newest update id waiting for the bot (`None` if none). Read without
/// an offset, which leaves the updates for whoever else reads them; only if
/// a full page is waiting - nobody is reading them - is the newest one
/// asked for directly, which drops the rest.
fn fetch_newest_update_id(token: &str) -> Result<Option<i64>, PollError> {
    let updates = telegram_get_updates(token, "")?;
    if updates.len() < TELEGRAM_UPDATES_PAGE {
        return Ok(newest_update_id(&updates));
    }
    Ok(newest_update_id(&telegram_get_updates(token, "&offset=-1&limit=1")?))
}

/// The chat of the newest update after `after` (any update if `None`), if
/// there is one yet. Read without an offset like `fetch_newest_update_id`;
/// only if a full page of older updates hides newer ones are those older
/// ones (all waiting since before detection started) skipped with an offset.
fn fetch_chat_after(token: &str, after: Option<i64>) -> Result<Option<DetectedChat>, PollError> {
    let updates = telegram_get_updates(token, "")?;
    if let Some(chat) = latest_chat_after(&updates, after) {
        return Ok(Some(chat));
    }
    let Some(after_id) = after else {
        return Ok(None);
    };
    if updates.len() < TELEGRAM_UPDATES_PAGE {
        return Ok(None);
    }
    let newer = telegram_get_updates(token, &format!("&offset={}", after_id.saturating_add(1)))?;
    Ok(latest_chat_after(&newer, after))
}

/// One update from a `getUpdates` response: its id and, if it came from a
/// chat, that chat.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TelegramUpdate {
    id: i64,
    chat: Option<DetectedChat>,
}

/// The updates in a `getUpdates` response; `None` if it isn't one (an
/// error reply, or not JSON).
#[cfg(test)]
fn parse_updates(body: &str) -> Option<Vec<TelegramUpdate>> {
    parse_updates_response(200, body).ok()
}

/// The updates in a `getUpdates` response with HTTP `status`, or why there
/// are none: Telegram's own error (`"ok": false`, its code and
/// description), or, for anything that isn't a Telegram reply, the HTTP
/// status (or `Network` if that was a success).
fn parse_updates_response(status: u16, body: &str) -> Result<Vec<TelegramUpdate>, PollError> {
    let json: Option<serde_json::Value> = serde_json::from_str(body).ok();
    if let Some(json) = &json {
        let ok = json.get("ok").and_then(serde_json::Value::as_bool);
        if ok != Some(false) {
            if let Some(updates) = json.get("result").and_then(serde_json::Value::as_array) {
                return Ok(updates
                    .iter()
                    .filter_map(|u| Some(TelegramUpdate { id: u.get("update_id")?.as_i64()?, chat: parse_update_chat(u) }))
                    .collect());
            }
        }
        if ok == Some(false) {
            let code = json
                .get("error_code")
                .and_then(serde_json::Value::as_u64)
                .and_then(|c| u16::try_from(c).ok())
                .unwrap_or(status);
            let description = json.get("description").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
            return Err(PollError::Api { code, description });
        }
    }
    if status >= 400 {
        Err(PollError::Api { code: status, description: String::new() })
    } else {
        Err(PollError::Network)
    }
}

fn newest_update_id(updates: &[TelegramUpdate]) -> Option<i64> {
    updates.iter().map(|u| u.id).max()
}

/// The chat of the newest update with an id greater than `after` (any id
/// if `None`) that came from a chat.
fn latest_chat_after(updates: &[TelegramUpdate], after: Option<i64>) -> Option<DetectedChat> {
    updates
        .iter()
        .filter(|u| after.is_none_or(|a| u.id > a))
        .filter(|u| u.chat.is_some())
        .max_by_key(|u| u.id)
        .and_then(|u| u.chat.clone())
}

/// The chat an update came from, with a display name.
fn parse_update_chat(update: &serde_json::Value) -> Option<DetectedChat> {
    let chat = ["message", "edited_message", "channel_post", "edited_channel_post", "my_chat_member"]
        .iter()
        .find_map(|kind| update.get(kind)?.get("chat"))?;
    let id = chat.get("id")?.as_i64()?;

    let text = |key: &str| chat.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
    let mut name = match text("title") {
        Some(title) => title.to_string(),
        None => [text("first_name"), text("last_name")].into_iter().flatten().collect::<Vec<_>>().join(" "),
    };
    if let Some(username) = text("username") {
        name = if name.is_empty() { format!("@{username}") } else { format!("{name} (@{username})") };
    }
    if name.is_empty() {
        name = "?".to_string();
    }
    Some(DetectedChat { id, name })
}

/// Whether this run of the wizard paused this PC's Telegram bot (see
/// `pause_own_bot_for`).
static mut WIZARD_PAUSED_BOT: bool = false;

/// Whether the wizard, about to look for the parent's message with `token`,
/// must first pause the bot this app runs with `config`: the same bot,
/// switched on. Telegram gives a bot's messages to one reader at a time.
fn must_pause_own_bot(config: &crate::database::TelegramConfig, token: &str) -> bool {
    config.enabled && config.bot_token.as_deref().map(str::trim) == Some(token.trim())
}

/// Re-running the wizard with the bot this app is already running used to
/// wait forever: the running bot took the parent's message (and answered it
/// as a command), and the two readers' requests cut each other off with
/// "409 Conflict". So that bot is paused while the wizard listens, and
/// started again when the wizard closes - by Settings, with what the wizard
/// saved, or by `show_telegram_wizard` if it saved nothing. Remote control
/// is off only while the wizard is open; the bot sends its usual "started"
/// message when it comes back.
unsafe fn pause_own_bot_for(token: &str) {
    if !WIZARD_PAUSED_BOT && must_pause_own_bot(&get_telegram_config(), token) {
        crate::telegram::stop_bot_thread();
        WIZARD_PAUSED_BOT = true;
    }
}

/// Save wizard configuration to database
unsafe fn save_wizard_config() {
    let wizard_token_ref = std::ptr::addr_of!(WIZARD_TOKEN);
    let chat_id = wizard_chat().map(|c| c.id);
    if let (Some(ref token), Some(chat_id)) = (&*wizard_token_ref, chat_id) {
        // Save to database using the existing function signature
        crate::database::set_telegram_config(token, &chat_id.to_string(), true);
        WIZARD_SAVED = true;

        // Send test message
        let token = token.clone();
        std::thread::spawn(move || {
            let _ = send_test_message(&token, chat_id);
        });
    }
}

/// Send a test message to confirm setup
fn send_test_message(token: &str, chat_id: i64) -> Result<(), ()> {
    let message = format!("✅ {} - Screen Time Manager", crate::i18n::t("wizard.success.title"));
    let url = format!(
        "https://api.telegram.org/bot{}/sendMessage?chat_id={}&text={}",
        token,
        chat_id,
        urlencoding::encode(&message)
    );

    let _ = ureq::get(&url).timeout(std::time::Duration::from_secs(5)).call();
    Ok(())
}

// ============================================================================
// Discord Setup Wizard
// ============================================================================

// Wizard control IDs
const ID_DC_WIZARD_NEXT: i32 = 3101;
const ID_DC_WIZARD_BACK: i32 = 3102;
const ID_DC_WIZARD_CANCEL: i32 = 3103;
const ID_DC_WIZARD_TOKEN_EDIT: i32 = 3104;
const ID_DC_WIZARD_CHANNEL_EDIT: i32 = 3105;
const ID_DC_WIZARD_USER_EDIT: i32 = 3106;

// Wizard state
static mut DC_WIZARD_STEP: i32 = 1;
static mut DC_WIZARD_TOKEN: Option<String> = None;
static mut DC_WIZARD_CHANNEL_ID: Option<String> = None;
static mut DC_WIZARD_USER_ID: Option<String> = None;
/// Whether this run of the wizard saved a configuration.
static mut DC_WIZARD_SAVED: bool = false;
/// The Discord wizard window while it is open.
static DC_WIZARD_HWND: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
const DC_WIZARD_TOTAL_STEPS: i32 = 5;

/// Show the Discord setup wizard. Returns whether it saved a configuration
/// (it does so on reaching its last step, whether or not Finish is clicked).
pub unsafe fn show_discord_wizard(parent_hwnd: HWND) -> bool {
    // One at a time: the wizard's state is shared (statics above).
    let open = HWND(DC_WIZARD_HWND.load(AtomicOrdering::SeqCst));
    if !open.0.is_null() && IsWindow(open).as_bool() {
        let _ = SetForegroundWindow(open);
        return false;
    }

    // Reset wizard state
    DC_WIZARD_STEP = 1;
    DC_WIZARD_TOKEN = None;
    DC_WIZARD_CHANNEL_ID = None;
    DC_WIZARD_USER_ID = None;
    DC_WIZARD_SAVED = false;

    let hinstance = GetModuleHandleW(None).unwrap();
    let wizard_class = w!("ScreenTimeDiscordWizard");

    // Register wizard window class
    register_dialog_class(wizard_class, Some(dc_wizard_proc), 0x00FFFFFF);

    let screen_width = GetSystemMetrics(SM_CXSCREEN);
    let screen_height = GetSystemMetrics(SM_CYSCREEN);
    let dialog_width = scale(480);
    let dialog_height = scale(580);

    let window_title = i18n::wide("wizard.dc.title");
    let wizard_hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_DLGMODALFRAME,
        wizard_class,
        PCWSTR(window_title.as_ptr()),
        WS_POPUP | WS_CAPTION | WS_SYSMENU,
        (screen_width - dialog_width) / 2,
        (screen_height - dialog_height) / 2,
        dialog_width,
        dialog_height,
        parent_hwnd,
        HMENU::default(),
        hinstance,
        None,
    );

    if let Ok(hwnd) = wizard_hwnd {
        DC_WIZARD_HWND.store(hwnd.0, AtomicOrdering::SeqCst);
        fit_wizard_dialog(hwnd, [ID_DC_WIZARD_CANCEL, ID_DC_WIZARD_BACK, ID_DC_WIZARD_NEXT]);
        run_owned_modal(hwnd, parent_hwnd);
    }
    DC_WIZARD_HWND.store(std::ptr::null_mut(), AtomicOrdering::SeqCst);
    DC_WIZARD_SAVED
}

/// Discord wizard window procedure
unsafe extern "system" fn dc_wizard_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            create_dc_wizard_buttons(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            paint_dc_wizard(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            handle_dc_wizard_command(hwnd, id);
            LRESULT(0)
        }
        WM_CLOSE => {
            // Close only this wizard (see run_modal_loop).
            close_modal(hwnd);
            LRESULT(0)
        }
        WM_VSCROLL | WM_MOUSEWHEEL if handle_wizard_scroll(hwnd, msg, wparam) => LRESULT(0),
        WM_DESTROY => {
            // Also when Settings, its owner, is destroyed.
            end_modal(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            set_wizard_scroll(hwnd, None);
            release_gdi(hwnd);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Create wizard navigation buttons
unsafe fn create_dc_wizard_buttons(hwnd: HWND) {
    let hinstance = GetModuleHandleW(None).unwrap();

    let mut rect: RECT = zeroed();
    GetClientRect(hwnd, &mut rect).ok();
    let width = rect.right;
    let height = rect.bottom;

    let btn_width = scale(100);
    let btn_height = scale(36);
    let btn_y = height - scale(60);
    let margin = scale(20);

    let btn_font = track_gdi(hwnd, CreateFontW(
        scale(15), 0, 0, 0,
        FW_NORMAL.0 as i32,
        0, 0, 0, 0, 0, 0, 5, 0,
        w!("Segoe UI"),
    ));

    // Cancel button (left)
    let cancel_text = i18n::wide("wizard.cancel");
    let cancel_btn = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        PCWSTR(cancel_text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
        margin,
        btn_y,
        btn_width,
        btn_height,
        hwnd,
        HMENU(ID_DC_WIZARD_CANCEL as _),
        hinstance,
        None,
    );
    if let Ok(h) = cancel_btn {
        SendMessageW(h, WM_SETFONT, WPARAM(btn_font.0 as usize), LPARAM(1));
    }

    // Back button
    let back_text = i18n::wide("wizard.back");
    let back_btn = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        PCWSTR(back_text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
        width - margin - btn_width * 2 - scale(10),
        btn_y,
        btn_width,
        btn_height,
        hwnd,
        HMENU(ID_DC_WIZARD_BACK as _),
        hinstance,
        None,
    );
    if let Ok(h) = back_btn {
        SendMessageW(h, WM_SETFONT, WPARAM(btn_font.0 as usize), LPARAM(1));
    }

    // Next/Finish button
    let next_text = i18n::wide("wizard.next");
    let next_btn = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        PCWSTR(next_text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
        width - margin - btn_width,
        btn_y,
        btn_width,
        btn_height,
        hwnd,
        HMENU(ID_DC_WIZARD_NEXT as _),
        hinstance,
        None,
    );
    if let Ok(h) = next_btn {
        SendMessageW(h, WM_SETFONT, WPARAM(btn_font.0 as usize), LPARAM(1));
    }
}

/// Paint the wizard content based on current step
unsafe fn paint_dc_wizard(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut rect: RECT = zeroed();
    GetClientRect(hwnd, &mut rect).ok();
    begin_wizard_paint(hwnd, hdc, &mut rect);
    let width = rect.right;

    // Background
    let bg_brush = CreateSolidBrush(COLORREF(0x00FFFFFF));
    FillRect(hdc, &rect, bg_brush);
    let _ = DeleteObject(bg_brush);

    // Header area
    let header_rect = RECT { left: 0, top: 0, right: width, bottom: scale(100) };
    let header_brush = CreateSolidBrush(COLORREF(0x00E3E6FD)); // Light Discord blurple tint
    FillRect(hdc, &header_rect, header_brush);
    let _ = DeleteObject(header_brush);

    SetBkMode(hdc, TRANSPARENT);

    // Draw step indicator circles (shared with the Telegram wizard)
    draw_step_indicators(hdc, width, DC_WIZARD_STEP);

    // Content area
    let content_top = scale(110);
    let content_rect = RECT {
        left: scale(30),
        top: content_top,
        right: width - scale(30),
        bottom: rect.bottom - scale(80),
    };

    match DC_WIZARD_STEP {
        1 => paint_dc_step_welcome(hdc, &content_rect),
        2 => paint_dc_step_create_bot(hdc, &content_rect),
        3 => paint_dc_step_token(hdc, hwnd, &content_rect),
        4 => paint_dc_step_ids(hdc, hwnd, &content_rect),
        5 => paint_dc_step_success(hdc, &content_rect),
        _ => {}
    }

    update_dc_wizard_buttons(hwnd);

    let _ = EndPaint(hwnd, &ps);
}

/// Step 1: Welcome screen
unsafe fn paint_dc_step_welcome(hdc: HDC, rect: &RECT) {
    let title_font = CreateFontW(scale(28), 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let desc_font = CreateFontW(scale(16), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let icon_font = CreateFontW(scale(48), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI Emoji"));

    let mut y = rect.top;

    // Chat icon
    SelectObject(hdc, icon_font);
    SetTextColor(hdc, COLORREF(0x00F25865)); // Discord blurple (BGR)
    let mut icon_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(60) };
    DrawTextW(hdc, &mut "💬".encode_utf16().collect::<Vec<_>>(), &mut icon_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(70);

    // Title
    SelectObject(hdc, title_font);
    SetTextColor(hdc, COLORREF(0x00333333));
    let mut title_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(35) };
    DrawTextW(hdc, &mut i18n::t("wizard.dc.welcome.title").encode_utf16().collect::<Vec<_>>(), &mut title_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(45);

    // Description
    SelectObject(hdc, desc_font);
    SetTextColor(hdc, COLORREF(0x00666666));
    let mut desc_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(25) };
    DrawTextW(hdc, &mut i18n::t("wizard.dc.welcome.desc1").encode_utf16().collect::<Vec<_>>(), &mut desc_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(40);

    // Features with icons (shared wording with the Telegram wizard)
    let features = [
        ("✓", i18n::t("wizard.welcome.feature1")),
        ("✓", i18n::t("wizard.welcome.feature2")),
        ("✓", i18n::t("wizard.welcome.feature3")),
        ("✓", i18n::t("wizard.welcome.feature4")),
    ];

    SetTextColor(hdc, COLORREF(0x004CAF50));
    for (icon, text) in features.iter() {
        let feature_text = format!("  {}  {}", icon, text);
        let mut feature_rect = RECT { left: rect.left + scale(40), top: y, right: rect.right, bottom: y + scale(28) };
        DrawTextW(hdc, &mut feature_text.encode_utf16().collect::<Vec<_>>(), &mut feature_rect, DT_SINGLELINE);
        y += scale(30);
    }

    y += scale(20);

    // Ready message
    SetTextColor(hdc, COLORREF(0x00333333));
    let mut ready_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(25) };
    DrawTextW(hdc, &mut i18n::t("wizard.dc.welcome.ready").encode_utf16().collect::<Vec<_>>(), &mut ready_rect, DT_CENTER | DT_SINGLELINE);

    deselect_font(hdc);
    let _ = DeleteObject(title_font);
    let _ = DeleteObject(desc_font);
    let _ = DeleteObject(icon_font);
}

/// Step 2: Create bot instructions
unsafe fn paint_dc_step_create_bot(hdc: HDC, rect: &RECT) {
    let title_font = CreateFontW(scale(22), 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let step_font = CreateFontW(scale(14), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let icon_font = CreateFontW(scale(40), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI Emoji"));
    let hint_font = CreateFontW(scale(12), 0, 0, 0, FW_NORMAL.0 as i32, 1, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));

    let mut y = rect.top;

    // Robot icon
    SelectObject(hdc, icon_font);
    SetTextColor(hdc, COLORREF(0x00F25865));
    let mut icon_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(46) };
    DrawTextW(hdc, &mut "🤖".encode_utf16().collect::<Vec<_>>(), &mut icon_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(50);

    // Title
    SelectObject(hdc, title_font);
    SetTextColor(hdc, COLORREF(0x00333333));
    let mut title_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(28) };
    DrawTextW(hdc, &mut i18n::t("wizard.dc.bot.title").encode_utf16().collect::<Vec<_>>(), &mut title_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(34);

    // Steps
    SelectObject(hdc, step_font);
    let steps = [
        i18n::t("wizard.dc.bot.step1"),
        i18n::t("wizard.dc.bot.step2"),
        i18n::t("wizard.dc.bot.step3"),
        i18n::t("wizard.dc.bot.step4"),
        i18n::t("wizard.dc.bot.step5"),
        i18n::t("wizard.dc.bot.step6"),
        i18n::t("wizard.dc.bot.step7"),
    ];

    for step in steps.iter() {
        SetTextColor(hdc, COLORREF(0x00444444));
        // A step that wraps (the long permissions list in step 7) gets the
        // room it needs instead of running into the next line.
        let mut text: Vec<u16> = step.encode_utf16().collect();
        let mut measured = RECT { left: rect.left, top: y, right: rect.right, bottom: y };
        DrawTextW(hdc, &mut text, &mut measured, DT_WORDBREAK | DT_CALCRECT);
        let height = (measured.bottom - measured.top + scale(8)).max(scale(30));
        let mut step_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + height };
        DrawTextW(hdc, &mut text, &mut step_rect, DT_WORDBREAK);
        y += height;
    }

    y += scale(6);

    // Hint box
    let hint_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(34) };
    let hint_brush = CreateSolidBrush(COLORREF(0x00FFF3E0)); // Light orange
    FillRect(hdc, &hint_rect, hint_brush);
    let _ = DeleteObject(hint_brush);

    SelectObject(hdc, hint_font);
    SetTextColor(hdc, COLORREF(0x00E65100));
    let mut hint_text_rect = RECT { left: rect.left + scale(10), top: y + scale(8), right: rect.right - scale(10), bottom: y + scale(30) };
    DrawTextW(hdc, &mut i18n::t("wizard.dc.bot.hint").encode_utf16().collect::<Vec<_>>(), &mut hint_text_rect, DT_CENTER | DT_WORDBREAK);

    deselect_font(hdc);
    let _ = DeleteObject(title_font);
    let _ = DeleteObject(step_font);
    let _ = DeleteObject(icon_font);
    let _ = DeleteObject(hint_font);
}

/// Step 3: Token entry
unsafe fn paint_dc_step_token(hdc: HDC, hwnd: HWND, rect: &RECT) {
    let title_font = CreateFontW(scale(24), 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let label_font = CreateFontW(scale(15), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let icon_font = CreateFontW(scale(40), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI Emoji"));

    let mut y = rect.top;

    // Key icon
    SelectObject(hdc, icon_font);
    SetTextColor(hdc, COLORREF(0x00FF9800));
    let mut icon_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(50) };
    DrawTextW(hdc, &mut "🔑".encode_utf16().collect::<Vec<_>>(), &mut icon_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(55);

    // Title
    SelectObject(hdc, title_font);
    SetTextColor(hdc, COLORREF(0x00333333));
    let mut title_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(30) };
    DrawTextW(hdc, &mut i18n::t("wizard.dc.token.title").encode_utf16().collect::<Vec<_>>(), &mut title_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(50);

    // Label
    SelectObject(hdc, label_font);
    SetTextColor(hdc, COLORREF(0x00666666));
    let mut label_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(22) };
    DrawTextW(hdc, &mut i18n::t("wizard.dc.token.label").encode_utf16().collect::<Vec<_>>(), &mut label_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(30);

    // Create/update edit control for token
    let edit_hwnd = GetDlgItem(hwnd, ID_DC_WIZARD_TOKEN_EDIT).unwrap_or_default();
    if edit_hwnd.0.is_null() {
        let hinstance = GetModuleHandleW(None).unwrap();
        let edit_width = scale(380);
        let edit_x = (rect.right - rect.left - edit_width) / 2 + rect.left;

        let _ = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_CENTER as u32 | ES_AUTOHSCROLL as u32),
            edit_x,
            y - wizard_scroll_pos(hwnd),
            edit_width,
            scale(35),
            hwnd,
            HMENU(ID_DC_WIZARD_TOKEN_EDIT as _),
            hinstance,
            None,
        );

        let edit_font = track_gdi(hwnd, CreateFontW(scale(13), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Consolas")));
        let new_edit = GetDlgItem(hwnd, ID_DC_WIZARD_TOKEN_EDIT).unwrap_or_default();
        if !new_edit.0.is_null() {
            SendMessageW(new_edit, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
            SendMessageW(new_edit, EM_SETLIMITTEXT, WPARAM(256), LPARAM(0));

            let dc_wizard_token_ref = std::ptr::addr_of!(DC_WIZARD_TOKEN);
            if let Some(ref token) = *dc_wizard_token_ref {
                let wide: Vec<u16> = token.encode_utf16().chain(std::iter::once(0)).collect();
                SetWindowTextW(new_edit, PCWSTR(wide.as_ptr())).ok();
            }
        }
    }

    y += scale(55);

    // Validation status
    let dc_wizard_token_ref = std::ptr::addr_of!(DC_WIZARD_TOKEN);
    let token_valid = (*dc_wizard_token_ref).as_ref().map(|t| is_valid_discord_token_format(t)).unwrap_or(false);
    if (*dc_wizard_token_ref).is_some() {
        let (status_text, status_color) = if token_valid {
            (format!("✓ {}", i18n::t("wizard.dc.token.valid")), COLORREF(0x004CAF50))
        } else {
            (format!("✗ {}", i18n::t("wizard.dc.token.invalid")), COLORREF(0x00F44336))
        };
        SetTextColor(hdc, status_color);
        let mut status_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(22) };
        DrawTextW(hdc, &mut status_text.encode_utf16().collect::<Vec<_>>(), &mut status_rect, DT_CENTER | DT_SINGLELINE);
    }

    deselect_font(hdc);
    let _ = DeleteObject(title_font);
    let _ = DeleteObject(label_font);
    let _ = DeleteObject(icon_font);
}

/// Step 4: Channel & User ID entry
unsafe fn paint_dc_step_ids(hdc: HDC, hwnd: HWND, rect: &RECT) {
    let title_font = CreateFontW(scale(22), 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let step_font = CreateFontW(scale(14), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let icon_font = CreateFontW(scale(40), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI Emoji"));
    let label_font = CreateFontW(scale(14), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));

    let mut y = rect.top;

    // Link icon
    SelectObject(hdc, icon_font);
    SetTextColor(hdc, COLORREF(0x009C27B0));
    let mut icon_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(46) };
    DrawTextW(hdc, &mut "🔗".encode_utf16().collect::<Vec<_>>(), &mut icon_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(50);

    // Title
    SelectObject(hdc, title_font);
    SetTextColor(hdc, COLORREF(0x00333333));
    let mut title_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(28) };
    DrawTextW(hdc, &mut i18n::t("wizard.dc.ids.title").encode_utf16().collect::<Vec<_>>(), &mut title_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(36);

    // Steps
    SelectObject(hdc, step_font);
    let steps = [
        i18n::t("wizard.dc.ids.step1"),
        i18n::t("wizard.dc.ids.step2"),
        i18n::t("wizard.dc.ids.step3"),
    ];
    for step in steps.iter() {
        SetTextColor(hdc, COLORREF(0x00444444));
        let mut step_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(30) };
        DrawTextW(hdc, &mut step.encode_utf16().collect::<Vec<_>>(), &mut step_rect, DT_WORDBREAK);
        y += scale(32);
    }

    y += scale(16);

    // Channel ID label + edit
    SelectObject(hdc, label_font);
    SetTextColor(hdc, COLORREF(0x00666666));
    let mut chan_label_rect = RECT { left: rect.left, top: y + scale(4), right: rect.left + scale(110), bottom: y + scale(28) };
    DrawTextW(hdc, &mut i18n::t("wizard.dc.ids.channel_label").encode_utf16().collect::<Vec<_>>(), &mut chan_label_rect, DT_SINGLELINE | DT_VCENTER);

    let channel_edit_hwnd = GetDlgItem(hwnd, ID_DC_WIZARD_CHANNEL_EDIT).unwrap_or_default();
    if channel_edit_hwnd.0.is_null() {
        let hinstance = GetModuleHandleW(None).unwrap();
        let edit_width = scale(200);
        let _ = CreateWindowExW(
            WS_EX_CLIENTEDGE, w!("EDIT"), w!(""),
            WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32 | ES_NUMBER as u32),
            rect.left + scale(120), y - wizard_scroll_pos(hwnd), edit_width, scale(28),
            hwnd, HMENU(ID_DC_WIZARD_CHANNEL_EDIT as _), hinstance, None,
        );
        let edit_font = track_gdi(hwnd, CreateFontW(scale(14), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Consolas")));
        let new_edit = GetDlgItem(hwnd, ID_DC_WIZARD_CHANNEL_EDIT).unwrap_or_default();
        if !new_edit.0.is_null() {
            SendMessageW(new_edit, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
            SendMessageW(new_edit, EM_SETLIMITTEXT, WPARAM(20), LPARAM(0));
            let dc_wizard_channel_ref = std::ptr::addr_of!(DC_WIZARD_CHANNEL_ID);
            if let Some(ref value) = *dc_wizard_channel_ref {
                let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                SetWindowTextW(new_edit, PCWSTR(wide.as_ptr())).ok();
            }
        }
    }
    y += scale(36);

    // User ID label + edit
    SelectObject(hdc, label_font);
    SetTextColor(hdc, COLORREF(0x00666666));
    let mut user_label_rect = RECT { left: rect.left, top: y + scale(4), right: rect.left + scale(110), bottom: y + scale(28) };
    DrawTextW(hdc, &mut i18n::t("wizard.dc.ids.user_label").encode_utf16().collect::<Vec<_>>(), &mut user_label_rect, DT_SINGLELINE | DT_VCENTER);

    let user_edit_hwnd = GetDlgItem(hwnd, ID_DC_WIZARD_USER_EDIT).unwrap_or_default();
    if user_edit_hwnd.0.is_null() {
        let hinstance = GetModuleHandleW(None).unwrap();
        let edit_width = scale(200);
        let _ = CreateWindowExW(
            WS_EX_CLIENTEDGE, w!("EDIT"), w!(""),
            WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32 | ES_NUMBER as u32),
            rect.left + scale(120), y - wizard_scroll_pos(hwnd), edit_width, scale(28),
            hwnd, HMENU(ID_DC_WIZARD_USER_EDIT as _), hinstance, None,
        );
        let edit_font = track_gdi(hwnd, CreateFontW(scale(14), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Consolas")));
        let new_edit = GetDlgItem(hwnd, ID_DC_WIZARD_USER_EDIT).unwrap_or_default();
        if !new_edit.0.is_null() {
            SendMessageW(new_edit, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
            SendMessageW(new_edit, EM_SETLIMITTEXT, WPARAM(20), LPARAM(0));
            let dc_wizard_user_ref = std::ptr::addr_of!(DC_WIZARD_USER_ID);
            if let Some(ref value) = *dc_wizard_user_ref {
                let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                SetWindowTextW(new_edit, PCWSTR(wide.as_ptr())).ok();
            }
        }
    }

    deselect_font(hdc);
    let _ = DeleteObject(title_font);
    let _ = DeleteObject(step_font);
    let _ = DeleteObject(icon_font);
    let _ = DeleteObject(label_font);
}

/// Step 5: Success
unsafe fn paint_dc_step_success(hdc: HDC, rect: &RECT) {
    let title_font = CreateFontW(scale(28), 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let desc_font = CreateFontW(scale(15), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
    let cmd_font = CreateFontW(scale(13), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Consolas"));
    let icon_font = CreateFontW(scale(56), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI Emoji"));

    let mut y = rect.top;

    // Checkmark icon
    SelectObject(hdc, icon_font);
    SetTextColor(hdc, COLORREF(0x004CAF50));
    let mut icon_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(65) };
    DrawTextW(hdc, &mut "✅".encode_utf16().collect::<Vec<_>>(), &mut icon_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(75);

    // Title
    SelectObject(hdc, title_font);
    SetTextColor(hdc, COLORREF(0x004CAF50));
    let mut title_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(35) };
    DrawTextW(hdc, &mut i18n::t("wizard.dc.success.title").encode_utf16().collect::<Vec<_>>(), &mut title_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(45);

    // Description
    SelectObject(hdc, desc_font);
    SetTextColor(hdc, COLORREF(0x00666666));
    let mut desc_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(22) };
    DrawTextW(hdc, &mut i18n::t("wizard.dc.success.desc").encode_utf16().collect::<Vec<_>>(), &mut desc_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(25);

    let mut test_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(22) };
    DrawTextW(hdc, &mut i18n::t("wizard.dc.success.test").encode_utf16().collect::<Vec<_>>(), &mut test_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(40);

    // Commands section
    SetTextColor(hdc, COLORREF(0x00333333));
    let mut cmd_title_rect = RECT { left: rect.left, top: y, right: rect.right, bottom: y + scale(22) };
    DrawTextW(hdc, &mut i18n::t("wizard.dc.success.commands").encode_utf16().collect::<Vec<_>>(), &mut cmd_title_rect, DT_CENTER | DT_SINGLELINE);
    y += scale(30);

    // Command examples with background
    SelectObject(hdc, cmd_font);
    let commands = [
        i18n::t("wizard.dc.success.cmd1"),
        i18n::t("wizard.dc.success.cmd2"),
        i18n::t("wizard.dc.success.cmd3"),
        i18n::t("wizard.dc.success.cmd4"),
    ];

    let cmd_bg = CreateSolidBrush(COLORREF(0x00F5F5F5));
    for cmd in commands.iter() {
        let cmd_rect = RECT { left: rect.left + scale(20), top: y, right: rect.right - scale(20), bottom: y + scale(22) };
        FillRect(hdc, &cmd_rect, cmd_bg);
        SetTextColor(hdc, COLORREF(0x00333333));
        let mut text_rect = RECT { left: rect.left + scale(30), top: y + scale(2), right: rect.right - scale(30), bottom: y + scale(20) };
        DrawTextW(hdc, &mut cmd.encode_utf16().collect::<Vec<_>>(), &mut text_rect, DT_SINGLELINE);
        y += scale(26);
    }
    let _ = DeleteObject(cmd_bg);

    deselect_font(hdc);
    let _ = DeleteObject(title_font);
    let _ = DeleteObject(desc_font);
    let _ = DeleteObject(cmd_font);
    let _ = DeleteObject(icon_font);
}

/// Check if token format is plausible (basic check)
fn is_valid_discord_token_format(token: &str) -> bool {
    // Discord bot tokens are three dot-separated segments (id.timestamp.signature)
    let parts: Vec<&str> = token.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| p.len() >= 6)
}

/// Update wizard button states based on current step
unsafe fn update_dc_wizard_buttons(hwnd: HWND) {
    let back_btn = GetDlgItem(hwnd, ID_DC_WIZARD_BACK).unwrap_or_default();
    let next_btn = GetDlgItem(hwnd, ID_DC_WIZARD_NEXT).unwrap_or_default();

    // Back button: disabled on step 1
    if !back_btn.0.is_null() {
        let _ = EnableWindow(back_btn, DC_WIZARD_STEP > 1);
    }

    // Next button text and state
    if !next_btn.0.is_null() {
        let next_text = if DC_WIZARD_STEP == DC_WIZARD_TOTAL_STEPS {
            i18n::wide("wizard.finish")
        } else {
            i18n::wide("wizard.next")
        };
        SetWindowTextW(next_btn, PCWSTR(next_text.as_ptr())).ok();

        // Disable next on step 3 if token invalid, step 4 until both IDs
        // are filled in (Next then checks them and says what's wrong)
        let dc_wizard_token_ref = std::ptr::addr_of!(DC_WIZARD_TOKEN);
        let dc_wizard_channel_ref = std::ptr::addr_of!(DC_WIZARD_CHANNEL_ID);
        let dc_wizard_user_ref = std::ptr::addr_of!(DC_WIZARD_USER_ID);
        let can_proceed = match DC_WIZARD_STEP {
            3 => (*dc_wizard_token_ref).as_ref().map(|t| is_valid_discord_token_format(t)).unwrap_or(false),
            4 => {
                let filled = |id: &Option<String>| id.as_deref().is_some_and(|s| !s.trim().is_empty());
                filled(&*dc_wizard_channel_ref) && filled(&*dc_wizard_user_ref)
            }
            _ => true,
        };
        let _ = EnableWindow(next_btn, can_proceed);
    }
}

/// Handle wizard command (button clicks)
unsafe fn handle_dc_wizard_command(hwnd: HWND, id: i32) {
    match id {
        ID_DC_WIZARD_CANCEL => {
            close_modal(hwnd);
        }
        ID_DC_WIZARD_BACK => {
            if DC_WIZARD_STEP > 1 {
                if DC_WIZARD_STEP == 3 {
                    let edit = GetDlgItem(hwnd, ID_DC_WIZARD_TOKEN_EDIT).unwrap_or_default();
                    if !edit.0.is_null() {
                        DestroyWindow(edit).ok();
                    }
                }
                if DC_WIZARD_STEP == 4 {
                    let ch_edit = GetDlgItem(hwnd, ID_DC_WIZARD_CHANNEL_EDIT).unwrap_or_default();
                    if !ch_edit.0.is_null() {
                        DestroyWindow(ch_edit).ok();
                    }
                    let us_edit = GetDlgItem(hwnd, ID_DC_WIZARD_USER_EDIT).unwrap_or_default();
                    if !us_edit.0.is_null() {
                        DestroyWindow(us_edit).ok();
                    }
                }
                DC_WIZARD_STEP -= 1;
                let _ = InvalidateRect(hwnd, None, true);
            }
        }
        ID_DC_WIZARD_NEXT => {
            // Save token from edit control on step 3
            if DC_WIZARD_STEP == 3 {
                let edit = GetDlgItem(hwnd, ID_DC_WIZARD_TOKEN_EDIT).unwrap_or_default();
                if !edit.0.is_null() {
                    let mut buffer = [0u16; 256];
                    let len = GetWindowTextW(edit, &mut buffer);
                    let token = String::from_utf16_lossy(&buffer[..len as usize]);
                    DC_WIZARD_TOKEN = Some(token.trim().to_string());
                }
            }

            // Save IDs from edit controls on step 4
            if DC_WIZARD_STEP == 4 {
                let ch_edit = GetDlgItem(hwnd, ID_DC_WIZARD_CHANNEL_EDIT).unwrap_or_default();
                if !ch_edit.0.is_null() {
                    let mut buffer = [0u16; 32];
                    let len = GetWindowTextW(ch_edit, &mut buffer);
                    DC_WIZARD_CHANNEL_ID = Some(String::from_utf16_lossy(&buffer[..len as usize]).trim().to_string());
                }
                let us_edit = GetDlgItem(hwnd, ID_DC_WIZARD_USER_EDIT).unwrap_or_default();
                if !us_edit.0.is_null() {
                    let mut buffer = [0u16; 32];
                    let len = GetWindowTextW(us_edit, &mut buffer);
                    DC_WIZARD_USER_ID = Some(String::from_utf16_lossy(&buffer[..len as usize]).trim().to_string());
                }

                // Both must be real IDs (a number other than 0) before going on.
                let dc_wizard_channel_ref = std::ptr::addr_of!(DC_WIZARD_CHANNEL_ID);
                let dc_wizard_user_ref = std::ptr::addr_of!(DC_WIZARD_USER_ID);
                for (value, edit_id, error_key) in [
                    (&*dc_wizard_channel_ref, ID_DC_WIZARD_CHANNEL_EDIT, "settings.error.discord_channel_id"),
                    (&*dc_wizard_user_ref, ID_DC_WIZARD_USER_EDIT, "settings.error.discord_user_id"),
                ] {
                    if value.as_deref().and_then(parse_discord_id).is_none() {
                        let edit = GetDlgItem(hwnd, edit_id).unwrap_or_default();
                        show_settings_field_error(hwnd, edit, i18n::t(error_key));
                        return;
                    }
                }
            }

            if DC_WIZARD_STEP < DC_WIZARD_TOTAL_STEPS {
                DC_WIZARD_STEP += 1;

                // Destroy token edit when leaving step 3
                if DC_WIZARD_STEP == 4 {
                    let edit = GetDlgItem(hwnd, ID_DC_WIZARD_TOKEN_EDIT).unwrap_or_default();
                    if !edit.0.is_null() {
                        DestroyWindow(edit).ok();
                    }
                }

                // On step 5, destroy ID edits, save config and send test message
                if DC_WIZARD_STEP == 5 {
                    let ch_edit = GetDlgItem(hwnd, ID_DC_WIZARD_CHANNEL_EDIT).unwrap_or_default();
                    if !ch_edit.0.is_null() {
                        DestroyWindow(ch_edit).ok();
                    }
                    let us_edit = GetDlgItem(hwnd, ID_DC_WIZARD_USER_EDIT).unwrap_or_default();
                    if !us_edit.0.is_null() {
                        DestroyWindow(us_edit).ok();
                    }
                    save_dc_wizard_config();
                }

                let _ = InvalidateRect(hwnd, None, true);
            } else {
                // Finish
                close_modal(hwnd);
            }
        }
        ID_DC_WIZARD_TOKEN_EDIT => {
            let edit = GetDlgItem(hwnd, ID_DC_WIZARD_TOKEN_EDIT).unwrap_or_default();
            if !edit.0.is_null() {
                let mut buffer = [0u16; 256];
                let len = GetWindowTextW(edit, &mut buffer);
                let token = String::from_utf16_lossy(&buffer[..len as usize]);
                DC_WIZARD_TOKEN = Some(token.trim().to_string());
                let _ = InvalidateRect(hwnd, None, true);
            }
        }
        ID_DC_WIZARD_CHANNEL_EDIT | ID_DC_WIZARD_USER_EDIT => {
            let ch_edit = GetDlgItem(hwnd, ID_DC_WIZARD_CHANNEL_EDIT).unwrap_or_default();
            if !ch_edit.0.is_null() {
                let mut buffer = [0u16; 32];
                let len = GetWindowTextW(ch_edit, &mut buffer);
                DC_WIZARD_CHANNEL_ID = Some(String::from_utf16_lossy(&buffer[..len as usize]).trim().to_string());
            }
            let us_edit = GetDlgItem(hwnd, ID_DC_WIZARD_USER_EDIT).unwrap_or_default();
            if !us_edit.0.is_null() {
                let mut buffer = [0u16; 32];
                let len = GetWindowTextW(us_edit, &mut buffer);
                DC_WIZARD_USER_ID = Some(String::from_utf16_lossy(&buffer[..len as usize]).trim().to_string());
            }
            let _ = InvalidateRect(hwnd, None, true);
        }
        _ => {}
    }
}

/// Save wizard configuration to database
unsafe fn save_dc_wizard_config() {
    let dc_wizard_token_ref = std::ptr::addr_of!(DC_WIZARD_TOKEN);
    let dc_wizard_channel_ref = std::ptr::addr_of!(DC_WIZARD_CHANNEL_ID);
    let dc_wizard_user_ref = std::ptr::addr_of!(DC_WIZARD_USER_ID);

    if let (Some(ref token), Some(ref channel_id), Some(ref user_id)) =
        (&*dc_wizard_token_ref, &*dc_wizard_channel_ref, &*dc_wizard_user_ref)
    {
        crate::database::set_discord_config(token, channel_id, user_id, true);
        DC_WIZARD_SAVED = true;

        if let Some(channel_id_num) = parse_discord_id(channel_id) {
            let token = token.clone();
            std::thread::spawn(move || {
                send_discord_test_message(&token, channel_id_num);
            });
        }
    }
}

/// Send a test message to confirm setup
fn send_discord_test_message(token: &str, channel_id: std::num::NonZeroU64) {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(_) => return,
    };
    rt.block_on(async {
        let http = serenity::http::Http::new(token);
        let message = format!("✅ {} - Screen Time Manager", crate::i18n::t("wizard.dc.success.title"));
        let _ = serenity::model::id::ChannelId::from(channel_id).say(&http, message).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hidden top-level window (owned by `owner` if given) for the modal
    /// tests; never shown.
    unsafe fn hidden_window(owner: Option<HWND>) -> HWND {
        CreateWindowExW(
            WINDOW_EX_STYLE(0), w!("STATIC"), w!(""), WS_POPUP,
            0, 0, 10, 10, owner.unwrap_or_default(), HMENU::default(), None, None,
        )
        .expect("create test window")
    }

    #[test]
    fn a_wizard_disables_its_owner_until_it_closes() {
        unsafe {
            let settings = hidden_window(None);
            let wizard = hidden_window(Some(settings));
            begin_modal(wizard, settings);
            assert!(!IsWindowEnabled(settings).as_bool(), "Settings must not take clicks behind the wizard");
            close_modal(wizard);
            assert!(IsWindowEnabled(settings).as_bool(), "Finish/Cancel/close give Settings back");
            assert!(!IsWindow(wizard).as_bool());

            // Destroyed some other way (its owner going away): WM_DESTROY's
            // end_modal, or the cleanup after the loop, re-enables; repeating
            // it is harmless.
            let wizard = hidden_window(Some(settings));
            begin_modal(wizard, settings);
            end_modal(wizard);
            end_modal(wizard);
            assert!(IsWindowEnabled(settings).as_bool());
            let _ = DestroyWindow(wizard);

            // An owner already disabled by someone else stays disabled.
            let _ = EnableWindow(settings, false);
            let wizard = hidden_window(Some(settings));
            begin_modal(wizard, settings);
            close_modal(wizard);
            assert!(!IsWindowEnabled(settings).as_bool());
            let _ = DestroyWindow(settings);
        }
    }

    /// The newest chat in a `getUpdates` body, whatever its id.
    fn parse_latest_chat(body: &str) -> Option<DetectedChat> {
        latest_chat_after(&parse_updates(body)?, None)
    }

    #[test]
    fn only_a_message_after_the_step_was_entered_is_taken() {
        let body = r#"{"ok":true,"result":[
            {"update_id":10,"message":{"chat":{"id":1,"first_name":"Stranger","type":"private"}}},
            {"update_id":11,"message":{"chat":{"id":2,"first_name":"Wrong","type":"private"}}}]}"#;
        let updates = parse_updates(body).unwrap();
        // Entering the step notes update 11 as the newest...
        assert_eq!(newest_update_id(&updates), Some(11));
        // ...so neither waiting message is taken, on entry or after "Back".
        assert_eq!(latest_chat_after(&updates, Some(11)), None);
        // The parent's own message afterwards is.
        let later = r#"{"ok":true,"result":[
            {"update_id":10,"message":{"chat":{"id":1,"first_name":"Stranger","type":"private"}}},
            {"update_id":11,"message":{"chat":{"id":2,"first_name":"Wrong","type":"private"}}},
            {"update_id":12,"message":{"chat":{"id":3,"first_name":"Parent","type":"private"}}}]}"#;
        assert_eq!(
            latest_chat_after(&parse_updates(later).unwrap(), Some(11)),
            Some(DetectedChat { id: 3, name: "Parent".into() })
        );
        // Nothing waiting when the step was entered: anything later counts.
        assert_eq!(newest_update_id(&parse_updates(r#"{"ok":true,"result":[]}"#).unwrap()), None);
        assert_eq!(latest_chat_after(&updates, None).map(|c| c.id), Some(2));
        // An error reply is not "no updates".
        assert_eq!(parse_updates(r#"{"ok":false,"error_code":409}"#), None);
    }

    #[test]
    fn telegram_errors_are_reported_not_taken_for_no_messages() {
        // A revoked or mistyped token.
        let revoked = parse_updates_response(401, r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#);
        assert_eq!(revoked, Err(PollError::Api { code: 401, description: "Unauthorized".into() }));
        // A token Telegram can't even parse: 404 with Telegram's body.
        let garbled = parse_updates_response(404, r#"{"ok":false,"error_code":404,"description":"Not Found"}"#);
        assert_eq!(poll_error_key(&garbled.unwrap_err()), "wizard.connect.error.token");
        // Another reader of the bot's messages.
        let conflict = parse_updates_response(
            409,
            r#"{"ok":false,"error_code":409,"description":"Conflict: terminated by other getUpdates request; make sure that only one bot instance is running"}"#,
        )
        .unwrap_err();
        assert_eq!(poll_error_key(&conflict), "wizard.connect.error.conflict");
        assert_eq!(
            poll_error_detail(&conflict).unwrap(),
            "Telegram: 409 Conflict: terminated by other getUpdates request; make sure that only one..."
        );
        assert!(poll_error_detail(&conflict).unwrap().chars().count() <= 90);
        // Not Telegram's reply at all (a proxy's error page, garbage).
        assert_eq!(parse_updates_response(502, "<html>Bad gateway</html>"), Err(PollError::Api { code: 502, description: String::new() }));
        assert_eq!(parse_updates_response(200, "not json"), Err(PollError::Network));
        assert_eq!(poll_error_key(&PollError::Network), "wizard.connect.error.network");
        assert_eq!(poll_error_detail(&PollError::Network), None);
        assert_eq!(poll_error_detail(&PollError::Api { code: 500, description: String::new() }).as_deref(), Some("Telegram: 500"));
        // A real reply still reads as before.
        let ok = parse_updates_response(200, r#"{"ok":true,"result":[{"update_id":3,"message":{"chat":{"id":9,"first_name":"P","type":"private"}}}]}"#);
        assert_eq!(ok.unwrap().len(), 1);
        assert_eq!(parse_updates_response(200, r#"{"ok":true,"result":[]}"#), Ok(vec![]));
    }

    #[test]
    fn a_bad_token_shows_at_once_other_errors_once_they_persist() {
        let mut status = PollStatus::new();
        assert_eq!(status.visible_error(), None);

        // Offline, or the paused bot's last request crossing ours: not
        // shown for one or two failures, shown from the third.
        status.record(Err(PollError::Network));
        status.record(Err(PollError::Api { code: 409, description: "Conflict".into() }));
        assert_eq!(status.visible_error(), None);
        status.record(Err(PollError::Network));
        assert_eq!(status.visible_error(), Some(&PollError::Network));

        // Any success clears it.
        status.record(Ok(()));
        assert_eq!(status, PollStatus::new());

        // A refused token is shown on the first failure.
        status.record(Err(PollError::Api { code: 401, description: "Unauthorized".into() }));
        assert!(status.visible_error().is_some());
    }

    #[test]
    fn the_wizard_pauses_this_pcs_bot_only_when_it_is_the_same_bot() {
        use crate::database::TelegramConfig;
        let running = TelegramConfig { bot_token: Some("123:abc".into()), admin_chat_id: Some(5), enabled: true };
        assert!(must_pause_own_bot(&running, "123:abc"));
        assert!(must_pause_own_bot(&TelegramConfig { bot_token: Some(" 123:abc\n".into()), ..running.clone() }, "123:abc"));
        assert!(!must_pause_own_bot(&running, "999:other"), "a new bot doesn't conflict");
        assert!(!must_pause_own_bot(&TelegramConfig { enabled: false, ..running.clone() }, "123:abc"), "not running");
        assert!(!must_pause_own_bot(&TelegramConfig { bot_token: None, ..running }, "123:abc"));
    }

    #[test]
    fn minutes_fields_take_whole_numbers_in_range() {
        assert_eq!(parse_minutes("1", 1, 1440), Some(1));
        assert_eq!(parse_minutes(" 1440 ", 1, 1440), Some(1440));
        assert_eq!(parse_minutes("0", 0, 9999), Some(0), "0 is a real limit: no time that day");
        assert_eq!(parse_minutes("", 0, 9999), None, "empty used to be stored and read back as 120");
        assert_eq!(parse_minutes("  ", 1, 1440), None);
        assert_eq!(parse_minutes("1441", 1, 1440), None);
        assert_eq!(parse_minutes("0", 1, 1440), None);
        assert_eq!(parse_minutes("40000000", 1, 1440), None, "'never' as a huge number");
        assert_eq!(parse_minutes("99999999999999999999", 0, 9999), None);
        assert_eq!(parse_minutes("-5", 0, 9999), None);
        assert_eq!(parse_minutes("ten", 0, 9999), None);
    }

    /// The Settings fields as a fresh install shows them.
    fn opened_settings() -> SettingsForm {
        SettingsForm {
            daily_limits: std::array::from_fn(|_| "120".to_string()),
            warning1_minutes: "10".into(),
            warning1_message: "10 minutes remaining!".into(),
            warning2_minutes: "5".into(),
            warning2_message: "5 minutes remaining!".into(),
            blocking_message: "Your screen time limit has been reached.".into(),
            lock_screen_timeout: "5".into(),
            idle_timeout: "5".into(),
            pause_budget: "30".into(),
            pause_max: "15".into(),
            pause_cooldown: "30".into(),
            language: 0,
            ..SettingsForm::default()
        }
    }

    fn refused(field: SettingsField, kind: SettingsErrorKind) -> Result<SettingsChanges, SettingsError> {
        Err(SettingsError { field, kind })
    }

    #[test]
    fn saving_unchanged_settings_writes_nothing() {
        // So a limit, message or PIN setting a bot command changed while
        // Settings was open is kept.
        let loaded = opened_settings();
        assert_eq!(plan_settings_save(&loaded, &loaded.clone(), 1), Ok(SettingsChanges::default()));
    }

    #[test]
    fn only_changed_fields_are_written() {
        let loaded = opened_settings();
        let mut now = loaded.clone();
        now.daily_limits[6] = "0".into();
        now.warning2_message = "Almost done".into();
        now.rotating_pin = true;
        now.language = 1;
        let changes = plan_settings_save(&loaded, &now, 1).unwrap();
        let mut limits = [None; 7];
        limits[6] = Some(0);
        assert_eq!(
            changes,
            SettingsChanges {
                daily_limits: limits,
                warning2_message: Some("Almost done".into()),
                rotating_pin: Some(true),
                language: Some(1),
                ..SettingsChanges::default()
            }
        );

        // Numbers are stored as numbers, the lock screen in seconds, an idle
        // time of 0 as 1 minute (as before).
        let mut now = loaded.clone();
        now.daily_limits[0] = " 90".into();
        now.lock_screen_timeout = "10".into();
        now.idle_timeout = "0".into();
        now.pause_budget = "0".into();
        let changes = plan_settings_save(&loaded, &now, 1).unwrap();
        assert_eq!(changes.daily_limits[0], Some(90));
        assert_eq!(changes.lock_screen_timeout_secs, Some(600));
        assert_eq!(changes.idle_timeout, Some(1));
        assert_eq!(changes.pause_budget, Some(0));
    }

    #[test]
    fn emptied_fields_are_refused_not_saved_empty() {
        let loaded = opened_settings();
        let emptied = |set: fn(&mut SettingsForm)| {
            let mut now = loaded.clone();
            set(&mut now);
            plan_settings_save(&loaded, &now, 1)
        };
        let range = |min, max| SettingsErrorKind::Minutes { min, max };
        assert_eq!(emptied(|f| f.daily_limits[6].clear()), refused(SettingsField::DailyLimit(6), range(0, 9999)));
        assert_eq!(emptied(|f| f.warning2_minutes.clear()), refused(SettingsField::Warning2Minutes, range(0, 9999)));
        assert_eq!(emptied(|f| f.lock_screen_timeout.clear()), refused(SettingsField::LockScreenTimeout, range(1, 1440)));
        assert_eq!(emptied(|f| f.idle_timeout.clear()), refused(SettingsField::IdleTimeout, range(0, 999)));
        assert_eq!(emptied(|f| f.pause_budget.clear()), refused(SettingsField::PauseBudget, range(0, 999)));
        assert_eq!(emptied(|f| f.pause_max.clear()), refused(SettingsField::PauseMax, range(1, 999)));
        assert_eq!(emptied(|f| f.pause_cooldown.clear()), refused(SettingsField::PauseCooldown, range(0, 999)));
        let empty = SettingsErrorKind::EmptyMessage;
        assert_eq!(emptied(|f| f.warning1_message.clear()), refused(SettingsField::Warning1Message, empty));
        assert_eq!(emptied(|f| f.warning2_message = "   ".into()), refused(SettingsField::Warning2Message, empty));
        assert_eq!(emptied(|f| f.blocking_message.clear()), refused(SettingsField::BlockingMessage, empty));

        // A field that was already empty when Settings opened (stored that
        // way by an older version) isn't held against an unrelated save.
        let mut old = opened_settings();
        old.blocking_message.clear();
        let mut now = old.clone();
        now.daily_limits[0] = "60".into();
        assert!(plan_settings_save(&old, &now, 1).is_ok());

        // The first bad field top to bottom is the one reported, after the
        // two that must be at least a minute (checked first, as before).
        let mut now = loaded.clone();
        now.blocking_message.clear();
        now.daily_limits[3] = "ten".into();
        now.pause_max = "0".into();
        assert_eq!(plan_settings_save(&loaded, &now, 1), refused(SettingsField::PauseMax, SettingsErrorKind::BelowOneMinute));
        now.pause_max = "15".into();
        assert_eq!(plan_settings_save(&loaded, &now, 1), refused(SettingsField::DailyLimit(3), range(0, 9999)));
    }

    #[test]
    fn bot_fields_are_trimmed_and_ids_checked_as_they_are_read() {
        let loaded = opened_settings();
        let mut now = loaded.clone();
        now.telegram_token = "  123:abc\r\n".into();
        now.telegram_chat_id = " -1001234567890 ".into();
        now.discord_token = "\ttoken.part.three ".into();
        now.discord_channel_id = " 42".into();
        now.discord_admin_user_id = "+0007 ".into();
        now.discord_enabled = true;
        let changes = plan_settings_save(&loaded, &now, 1).unwrap();
        assert_eq!(
            changes.telegram,
            TelegramChanges { token: Some("123:abc".into()), chat_id: Some("-1001234567890".into()), enabled: None }
        );
        assert_eq!(
            changes.discord,
            DiscordChanges {
                token: Some("token.part.three".into()),
                channel_id: Some("42".into()),
                admin_user_id: Some("7".into()),
                enabled: Some(true),
            }
        );

        let bad = |set: fn(&mut SettingsForm)| {
            let mut now = loaded.clone();
            set(&mut now);
            plan_settings_save(&loaded, &now, 1)
        };
        let id = SettingsErrorKind::BadId;
        assert_eq!(bad(|f| f.telegram_chat_id = "0".into()), refused(SettingsField::TelegramChatId, id));
        assert_eq!(bad(|f| f.telegram_chat_id = "12a".into()), refused(SettingsField::TelegramChatId, id));
        assert_eq!(bad(|f| f.telegram_chat_id = "--5".into()), refused(SettingsField::TelegramChatId, id));
        assert_eq!(bad(|f| f.discord_channel_id = "0".into()), refused(SettingsField::DiscordChannelId, id));
        assert_eq!(bad(|f| f.discord_admin_user_id = "-7".into()), refused(SettingsField::DiscordUserId, id));
        // Emptying a bot field clears that setting, as before.
        let cleared = bad(|f| f.telegram_chat_id = " ".into()).unwrap();
        assert_eq!(cleared.telegram.chat_id, Some(String::new()));
    }

    #[test]
    fn telegram_chat_ids_may_be_negative() {
        assert_eq!(parse_telegram_chat_id("123456789"), Some(123_456_789));
        assert_eq!(parse_telegram_chat_id(" -1001234567890 "), Some(-1_001_234_567_890));
        assert_eq!(parse_telegram_chat_id("0"), None);
        assert_eq!(parse_telegram_chat_id(""), None);
        assert_eq!(parse_telegram_chat_id("- 5"), None);
        assert_eq!(parse_telegram_chat_id("99999999999999999999"), None, "beyond i64");
    }

    #[test]
    fn unchanged_bot_settings_keep_their_stored_values() {
        use crate::database::{DiscordConfig, TelegramConfig};
        let telegram = TelegramConfig { bot_token: Some("123:abc".into()), admin_chat_id: Some(-100), enabled: true };
        assert_eq!(merged_telegram(&TelegramChanges::default(), &telegram), None);
        assert_eq!(
            merged_telegram(&TelegramChanges { enabled: Some(false), ..Default::default() }, &telegram),
            Some(("123:abc".to_string(), "-100".to_string(), false))
        );
        let discord = DiscordConfig { bot_token: None, channel_id: Some(5), admin_user_id: None, enabled: false };
        assert_eq!(merged_discord(&DiscordChanges::default(), &discord), None);
        assert_eq!(
            merged_discord(&DiscordChanges { token: Some("t.o.k".into()), ..Default::default() }, &discord),
            Some(("t.o.k".to_string(), "5".to_string(), String::new(), false))
        );
    }

    #[test]
    fn a_new_passcode_is_four_digits() {
        assert!(is_valid_new_passcode("0420"));
        assert!(!is_valid_new_passcode("12a4"), "letters can't be typed into the passcode prompt");
        assert!(!is_valid_new_passcode("abcd"));
        assert!(!is_valid_new_passcode("123"));
        assert!(!is_valid_new_passcode("12345"));
        assert!(!is_valid_new_passcode("١٢٣٤"), "non-ASCII digits");
        assert!(!is_valid_new_passcode(" 123"));
    }

    #[test]
    fn every_refusal_has_a_message() {
        use SettingsErrorKind::*;
        use SettingsField::*;
        for (field, kind) in [
            (LockScreenTimeout, BelowOneMinute),
            (PauseMax, BelowOneMinute),
            (DailyLimit(0), Minutes { min: 0, max: 9999 }),
            (BlockingMessage, EmptyMessage),
            (TelegramChatId, BadId),
            (DiscordChannelId, BadId),
            (DiscordUserId, BadId),
        ] {
            let error = SettingsError { field, kind };
            assert!(!settings_error_text(&error).is_empty(), "{error:?}");
        }
        assert_eq!(
            settings_error_text(&SettingsError { field: PauseBudget, kind: Minutes { min: 0, max: 999 } }),
            "Please enter a whole number of minutes from 0 to 999."
        );
    }

    #[test]
    fn lock_screen_timeout_is_stored_in_seconds_without_overflow() {
        assert_eq!(lock_screen_timeout_secs(10), Some(600));
        assert_eq!(lock_screen_timeout_secs(0), Some(60), "at least a minute");
        assert_eq!(lock_screen_timeout_secs(MAX_LOCK_SCREEN_TIMEOUT_MINS), Some(86_400));
        assert_eq!(lock_screen_timeout_secs(MAX_LOCK_SCREEN_TIMEOUT_MINS + 1), None);
        assert_eq!(lock_screen_timeout_secs(40_000_000), None, "used to wrap to a negative value");
        assert_eq!(lock_screen_timeout_secs(u32::MAX), None);
    }

    #[test]
    fn discord_ids_must_be_numbers_other_than_zero() {
        assert_eq!(parse_discord_id("123456789012345678").map(|n| n.get()), Some(123_456_789_012_345_678));
        assert_eq!(parse_discord_id(" 42 ").map(|n| n.get()), Some(42));
        assert_eq!(parse_discord_id("0"), None);
        assert_eq!(parse_discord_id("000"), None);
        assert_eq!(parse_discord_id(""), None);
        assert_eq!(parse_discord_id("abc"), None);
        assert_eq!(parse_discord_id("-1"), None);
        assert_eq!(parse_discord_id("99999999999999999999"), None, "beyond u64");
    }

    #[test]
    fn newest_update_names_the_chat() {
        let body = r#"{"ok":true,"result":[{"update_id":5,"message":{"message_id":9,
            "from":{"id":42,"is_bot":false,"first_name":"Dan"},
            "chat":{"id":42,"first_name":"Dan","last_name":"H","username":"dan_h","type":"private"},
            "date":1,"text":"/start"}}]}"#;
        assert_eq!(parse_latest_chat(body), Some(DetectedChat { id: 42, name: "Dan H (@dan_h)".into() }));
    }

    #[test]
    fn group_chats_use_their_title_and_the_last_update_wins() {
        let body = r#"{"ok":true,"result":[
            {"update_id":1,"message":{"chat":{"id":1,"first_name":"Old","type":"private"}}},
            {"update_id":2,"my_chat_member":{"chat":{"id":-100123,"title":"Family","type":"group"}}}]}"#;
        assert_eq!(parse_latest_chat(body), Some(DetectedChat { id: -100123, name: "Family".into() }));
    }

    #[test]
    fn no_usable_update_means_no_chat() {
        assert_eq!(parse_latest_chat(r#"{"ok":true,"result":[]}"#), None);
        assert_eq!(parse_latest_chat(r#"{"ok":false,"error_code":401}"#), None);
        assert_eq!(parse_latest_chat("not json"), None);
        let nameless = r#"{"ok":true,"result":[{"update_id":1,"message":{"chat":{"id":7,"type":"private"}}}]}"#;
        assert_eq!(parse_latest_chat(nameless), Some(DetectedChat { id: 7, name: "?".into() }));
    }

    #[test]
    fn settings_window_fits_when_it_can_and_scrolls_when_it_cannot() {
        // 1080p at 100%: work area 1032 (48 px taskbar); content 992 client
        // px (largest case, with the default-passcode warning); ~32 px frame.
        let fits = settings_window_layout(392, 992, 8, 32, 1032, 17);
        assert_eq!(fits, SettingsWindowLayout { width: 400, height: 1024, scroll: false });

        // 1366x768 at 100%: work area 728.
        let small = settings_window_layout(392, 992, 8, 32, 728, 17);
        assert_eq!(small, SettingsWindowLayout { width: 417, height: 728, scroll: true });

        // 1080p at 125%: everything scaled by 1.25, work area 1020.
        let scaled = settings_window_layout(490, 1240, 10, 39, 1020, 21);
        assert_eq!(scaled, SettingsWindowLayout { width: 521, height: 1020, scroll: true });
    }

    /// The wizards' outer size is scale(480) x scale(520) (Telegram) and
    /// scale(580) (Discord), centred on the whole screen. Frame taken as
    /// 8 x 32 px at 100% scaled with the DPI (14 x 56 at 175%, 16 x 64 at
    /// 200%), the scroll bar as 17 px scaled (30 / 34), and a 48 px taskbar
    /// scaled (84 / 96) at the bottom.
    #[test]
    fn wizards_fit_the_work_area_at_high_scaling() {
        let work = |w: i32, h: i32| RECT { left: 0, top: 0, right: w, bottom: h };
        let centred = |screen: i32, size: i32| (screen - size) / 2;

        // 1080p at 175%: work area 1080 - 84 = 996.
        // Telegram 840 x 910 (client 826 x 854) fits, and its screen-centred
        // position (y 85, bottom 995) is already inside: left as it was.
        assert_eq!(
            settings_window_layout(826, 854, 14, 56, 996, 30),
            SettingsWindowLayout { width: 840, height: 910, scroll: false }
        );
        assert_eq!(position_within(540, centred(1080, 910), 840, 910, &work(1920, 996)), (540, 85));
        // Discord 840 x 1015 (client 959) doesn't: 996 tall with a scroll
        // bar (client 940, 19 px to scroll), 870 wide.
        assert_eq!(
            settings_window_layout(826, 959, 14, 56, 996, 30),
            SettingsWindowLayout { width: 870, height: 996, scroll: true }
        );

        // 1080p at 200%: work area 984. Telegram 960 x 1040 (client 976) and
        // Discord 960 x 1160 (client 1096) both scroll, 994 wide.
        assert_eq!(
            settings_window_layout(944, 976, 16, 64, 984, 34),
            SettingsWindowLayout { width: 994, height: 984, scroll: true }
        );
        assert_eq!(
            settings_window_layout(944, 1096, 16, 64, 984, 34),
            SettingsWindowLayout { width: 994, height: 984, scroll: true }
        );

        // 1200p at 200%: work area 1104. Telegram (1040) fits, but centred on
        // the screen (y 80) its bottom (1120) was 16 px behind the taskbar:
        // moved up to 64. Discord (1160) scrolls.
        assert_eq!(
            settings_window_layout(944, 976, 16, 64, 1104, 34),
            SettingsWindowLayout { width: 960, height: 1040, scroll: false }
        );
        assert_eq!(position_within(480, centred(1200, 1040), 960, 1040, &work(1920, 1104)), (480, 64));
        assert!(settings_window_layout(944, 1096, 16, 64, 1104, 34).scroll);

        // 1200p at 175%: work area 1116; both fit where they are created.
        assert!(!settings_window_layout(826, 959, 14, 56, 1116, 30).scroll);
        assert_eq!(position_within(540, centred(1200, 1015), 840, 1015, &work(1920, 1116)), (540, 92));

        // 100% on 1080p: unchanged.
        assert!(!settings_window_layout(472, 548, 8, 32, 1032, 17).scroll);
        assert_eq!(position_within(720, 250, 480, 580, &work(1920, 1032)), (720, 250));

        // A taskbar at the top or left: moved down / right onto the work area.
        let top_bar = RECT { left: 0, top: 96, right: 1920, bottom: 1200 };
        assert_eq!(position_within(480, 80, 960, 1040, &top_bar), (480, 96));
        let left_bar = RECT { left: 96, top: 0, right: 1920, bottom: 1200 };
        assert_eq!(position_within(40, 80, 960, 1040, &left_bar), (96, 80));
    }
}
