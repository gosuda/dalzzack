//! Native key presses for `keyboard --native`.
//!
//! A `KeyboardEvent` made in JavaScript reaches the page with
//! `isTrusted: false`. It does not move focus on Tab, and it cannot get into a
//! cross-origin iframe inside a closed shadow root. A Cloudflare Turnstile
//! checkbox is built that way.
//!
//! - macOS (WKWebView): an `NSEvent` key down and key up, built inside the
//!   daemon and handed to the WebView's own window with `[window sendEvent:]`
//!   after making the WKWebView the first responder.
//! - Windows (WebView2): CDP `Input.dispatchKeyEvent` over WebView2's DevTools
//!   channel, the same two calls the Chrome engine sends for a key.
//!
//! Either way the page sees `isTrusted: true` and focus crosses frame and
//! shadow boundaries; Tab and then Space tick a Turnstile checkbox (measured
//! on macOS). The events stay inside the process, so no Accessibility
//! permission is needed. The mouse does not move, and the window is not
//! brought to the front.

/// A key that `keyboard --native` can press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeKey {
    /// The name the result reports (`Tab`, `Space`, …).
    pub name: &'static str,
    /// macOS virtual key code (`kVK_*` in HIToolbox `Events.h`).
    pub key_code: u16,
    /// What `characters` carries. Keys that type nothing use the AppKit
    /// function-key code points (`NSUpArrowFunctionKey` = U+F700, …).
    pub chars: &'static str,
}

const NATIVE_KEYS: &[NativeKey] = &[
    NativeKey {
        name: "Enter",
        key_code: 0x24,
        chars: "\r",
    },
    NativeKey {
        name: "Tab",
        key_code: 0x30,
        chars: "\t",
    },
    NativeKey {
        name: "Space",
        key_code: 0x31,
        chars: " ",
    },
    NativeKey {
        name: "Backspace",
        key_code: 0x33,
        chars: "\u{7f}",
    },
    NativeKey {
        name: "Escape",
        key_code: 0x35,
        chars: "\u{1b}",
    },
    NativeKey {
        name: "Insert",
        key_code: 0x72,
        chars: "\u{F727}",
    },
    NativeKey {
        name: "Home",
        key_code: 0x73,
        chars: "\u{F729}",
    },
    NativeKey {
        name: "PageUp",
        key_code: 0x74,
        chars: "\u{F72C}",
    },
    NativeKey {
        name: "Delete",
        key_code: 0x75,
        chars: "\u{F728}",
    },
    NativeKey {
        name: "End",
        key_code: 0x77,
        chars: "\u{F72B}",
    },
    NativeKey {
        name: "PageDown",
        key_code: 0x79,
        chars: "\u{F72D}",
    },
    NativeKey {
        name: "ArrowLeft",
        key_code: 0x7B,
        chars: "\u{F702}",
    },
    NativeKey {
        name: "ArrowRight",
        key_code: 0x7C,
        chars: "\u{F703}",
    },
    NativeKey {
        name: "ArrowDown",
        key_code: 0x7D,
        chars: "\u{F701}",
    },
    NativeKey {
        name: "ArrowUp",
        key_code: 0x7E,
        chars: "\u{F700}",
    },
];

/// Look up a named key, ignoring case. `" "` is Space.
pub fn native_key(name: &str) -> Option<NativeKey> {
    if name == " " {
        return native_key("Space");
    }
    NATIVE_KEYS
        .iter()
        .find(|k| k.name.eq_ignore_ascii_case(name))
        .copied()
}

/// The names `native_key` accepts, for an `UNKNOWN_KEY` message.
pub fn native_key_names() -> Vec<&'static str> {
    NATIVE_KEYS.iter().map(|k| k.name).collect()
}

/// `NSEventModifierFlags` for a comma-separated modifier list, with the names
/// the Chrome engine accepts (`ctrl`, `shift`, `alt`, `meta`, and the aliases
/// `control`, `cmd`, `command`). Unknown names add nothing.
pub fn native_modifier_flags(modifiers: &str) -> usize {
    const SHIFT: usize = 1 << 17;
    const CONTROL: usize = 1 << 18;
    const OPTION: usize = 1 << 19;
    const COMMAND: usize = 1 << 20;
    modifiers
        .split(',')
        .map(|m| match m.trim().to_ascii_lowercase().as_str() {
            "shift" => SHIFT,
            "ctrl" | "control" => CONTROL,
            "alt" => OPTION,
            "meta" | "cmd" | "command" => COMMAND,
            _ => 0,
        })
        .fold(0, |flags, bit| flags | bit)
}

/// How a native key press was delivered, for the tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeKeyPress {
    /// `nsevent` (macOS) or `cdp` (Windows).
    pub transport: &'static str,
    /// macOS: the class of the window's first responder when the key was
    /// sent. It should be the WKWebView; when it is not, the key went
    /// somewhere else. `None` on Windows.
    pub first_responder: Option<String>,
}

/// Press `key` once (key down, then key up) on the main WebView.
pub async fn native_key_press(key: NativeKey, modifiers: &str) -> Result<NativeKeyPress, String> {
    #[cfg(target_os = "macos")]
    {
        let first_responder = native_key_press_macos(key, native_modifier_flags(modifiers)).await?;
        Ok(NativeKeyPress {
            transport: "nsevent",
            first_responder: Some(first_responder),
        })
    }
    #[cfg(target_os = "windows")]
    {
        native_key_press_windows(key, modifiers).await?;
        Ok(NativeKeyPress {
            transport: "cdp",
            first_responder: None,
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (key, modifiers);
        Err("keyboard --native works on macOS and Windows only".to_string())
    }
}

/// Windows: the Chrome engine's `Input.dispatchKeyEvent` pair, sent over
/// WebView2's CDP channel. CDP input is trusted input to Blink.
#[cfg(target_os = "windows")]
async fn native_key_press_windows(key: NativeKey, modifiers: &str) -> Result<(), String> {
    use crate::chrome::tools::{key_event_params, key_info, modifier_mask};

    let info = key_info(key.name).ok_or_else(|| format!("no CDP key for '{}'", key.name))?;
    let window = crate::state::get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND")?;
    for params in key_event_params(&info, modifier_mask(modifiers)) {
        crate::cdp::call(
            &window,
            "Input.dispatchKeyEvent",
            &params.to_string(),
            crate::cdp::DEFAULT_TIMEOUT,
        )
        .await?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
async fn native_key_press_macos(key: NativeKey, modifier_flags: usize) -> Result<String, String> {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::{NSPoint, NSString};

    const NS_EVENT_TYPE_KEY_DOWN: usize = 10;
    const NS_EVENT_TYPE_KEY_UP: usize = 11;

    let window = crate::state::get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND")?;
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<String, String>>();

    window
        .with_webview(move |wv| unsafe {
            let result = (|| {
                let wk: *mut AnyObject = wv.inner().cast();
                if wk.is_null() {
                    return Err("WKWebView is null".to_string());
                }
                let win: *mut AnyObject = msg_send![wk, window];
                if win.is_null() {
                    return Err("WKWebView has no window".to_string());
                }
                // Within the window only: this does not make the window key,
                // so it takes no focus away from other apps.
                let _: bool = msg_send![win, makeFirstResponder: wk];
                let responder: *mut AnyObject = msg_send![win, firstResponder];
                let responder = if responder.is_null() {
                    "null".to_string()
                } else {
                    format!("{}", (*responder).class())
                };

                let window_number: isize = msg_send![win, windowNumber];
                let process: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
                // A real key event carries the time since boot; WebKit may drop
                // one with a zero timestamp.
                let uptime: f64 = msg_send![process, systemUptime];
                let chars = NSString::from_str(key.chars);

                for event_type in [NS_EVENT_TYPE_KEY_DOWN, NS_EVENT_TYPE_KEY_UP] {
                    let event: *mut AnyObject = msg_send![
                        class!(NSEvent),
                        keyEventWithType: event_type,
                        location: NSPoint::new(0.0, 0.0),
                        modifierFlags: modifier_flags,
                        timestamp: uptime,
                        windowNumber: window_number,
                        context: std::ptr::null_mut::<AnyObject>(),
                        characters: &*chars,
                        charactersIgnoringModifiers: &*chars,
                        isARepeat: false,
                        keyCode: key.key_code
                    ];
                    if event.is_null() {
                        return Err("NSEvent keyEventWithType returned nil".to_string());
                    }
                    // Straight to the window, past key-window routing.
                    let _: () = msg_send![win, sendEvent: event];
                }
                Ok(responder)
            })();
            let _ = tx.send(result);
        })
        .map_err(|e| format!("with_webview error: {e:?}"))?;

    match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("main thread dropped the key press".to_string()),
        Err(_) => Err("key press did not run on the main thread within 5s".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_and_space_use_the_apple_virtual_key_codes() {
        // kVK_Tab = 0x30 and kVK_Space = 0x31 in HIToolbox Events.h. These two
        // keys tick a Turnstile checkbox, so a wrong code here fails silently.
        assert_eq!(
            native_key("Tab").map(|k| (k.key_code, k.chars)),
            Some((0x30, "\t"))
        );
        assert_eq!(native_key("tab"), native_key("Tab"));
        assert_eq!(
            native_key("Space").map(|k| (k.key_code, k.chars)),
            Some((0x31, " "))
        );
        assert_eq!(native_key(" "), native_key("Space"));
        assert_eq!(native_key("space"), native_key("Space"));
    }

    #[test]
    fn every_key_has_its_own_code_and_characters() {
        for (i, a) in NATIVE_KEYS.iter().enumerate() {
            assert!(!a.chars.is_empty(), "{} carries no characters", a.name);
            for b in &NATIVE_KEYS[i + 1..] {
                assert_ne!(
                    a.key_code, b.key_code,
                    "{} and {} share a key code",
                    a.name, b.name
                );
                assert_ne!(
                    a.chars, b.chars,
                    "{} and {} share characters",
                    a.name, b.name
                );
            }
        }
    }

    #[test]
    fn a_key_outside_the_table_is_refused() {
        assert_eq!(native_key("a"), None);
        assert_eq!(native_key("F5"), None);
        assert_eq!(native_key(""), None);
    }

    #[test]
    fn modifiers_map_to_nsevent_flags() {
        assert_eq!(native_modifier_flags(""), 0);
        assert_eq!(native_modifier_flags("shift"), 1 << 17);
        assert_eq!(native_modifier_flags("ctrl, shift"), (1 << 18) | (1 << 17));
        assert_eq!(
            native_modifier_flags("ctrl,shift,alt,meta"),
            (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20)
        );
        assert_eq!(native_modifier_flags("hyper"), 0);
    }

    #[test]
    fn modifiers_accept_the_chrome_engine_aliases() {
        assert_eq!(
            native_modifier_flags("control,cmd"),
            native_modifier_flags("ctrl,meta")
        );
        assert_eq!(
            native_modifier_flags("Command"),
            native_modifier_flags("meta")
        );
    }

    #[test]
    fn every_native_key_has_a_cdp_key_for_windows() {
        // Windows presses a native key through the Chrome engine's table, so a
        // key missing there would pass the name check and then fail.
        for key in NATIVE_KEYS {
            assert!(
                crate::chrome::tools::key_info(key.name).is_some(),
                "{} has no CDP key",
                key.name
            );
        }
        // Blink matches the DOM key exactly: "Tab" moves focus and " "
        // activates a checkbox.
        let tab = crate::chrome::tools::key_event_params(
            &crate::chrome::tools::key_info("Tab").expect("Tab"),
            0,
        );
        assert_eq!(tab[0]["key"], "Tab");
        assert_eq!(tab[0]["windowsVirtualKeyCode"], 9);
        let space = crate::chrome::tools::key_event_params(
            &crate::chrome::tools::key_info("Space").expect("Space"),
            0,
        );
        assert_eq!(space[0]["key"], " ");
        assert_eq!(space[0]["windowsVirtualKeyCode"], 32);
        assert_eq!(space[1]["type"], "keyUp");
    }
}
