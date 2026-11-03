//! Native mouse simulation — OS-level cursor move + click.
//!
//! macOS: CGEvent (Quartz Event Services)
//! Windows: SendInput (Win32)
//! Linux: stub (not supported)
//!
//! Simulates real mouse input to bypass the limits of JavaScript synthetic events.
//! Works across cross-origin iframes, dropdowns, framework synthetic events, and more.

use crate::state::get_main_window;

/// Perform an OS-level mouse click at coordinates relative to the WebView.
///
/// # Arguments
/// * `viewport_x` - X coordinate inside the WebView viewport (CSS pixels)
/// * `viewport_y` - Y coordinate inside the WebView viewport (CSS pixels)
/// * `click_type` - "left", "right", "double"
pub async fn native_mouse_click(
    viewport_x: f64,
    viewport_y: f64,
    click_type: &str,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        native_mouse_click_macos(viewport_x, viewport_y, click_type).await
    }
    #[cfg(target_os = "windows")]
    {
        native_mouse_click_windows(viewport_x, viewport_y, click_type).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err("Native mouse click not supported on this platform".to_string())
    }
}

/// Move the OS cursor to the given viewport coordinates without clicking.
///
/// Used by `hover` to trigger the browser's CSS `:hover` pseudo-class, which
/// JavaScript synthetic `mouseover` events alone cannot do. macOS only for now;
/// Windows/Linux callers should fall back to JS-only hover with a note that
/// `:hover` styles will not apply.
pub async fn native_mouse_move(viewport_x: f64, viewport_y: f64) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        native_mouse_move_macos(viewport_x, viewport_y).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (viewport_x, viewport_y);
        Err("Native mouse move not supported on this platform".to_string())
    }
}

#[cfg(target_os = "macos")]
async fn native_mouse_move_macos(viewport_x: f64, viewport_y: f64) -> Result<(), String> {
    use core_graphics::event::{CGEvent, CGEventType, CGMouseButton};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;

    let window = get_main_window().await.ok_or("WEBVIEW_NOT_FOUND")?;
    let _ = window.set_focus();

    let scale = window.scale_factor().map_err(|e| format!("scale: {}", e))?;
    let outer_pos = window
        .outer_position()
        .map_err(|e| format!("outer_pos: {}", e))?;
    let inner_pos = window
        .inner_position()
        .map_err(|e| format!("inner_pos: {}", e))?;

    let title_bar_phys = (inner_pos.y - outer_pos.y) as f64;
    let title_bar_logical = if title_bar_phys > 1.0 {
        title_bar_phys / scale
    } else {
        28.0
    };

    let screen_x = (outer_pos.x as f64 / scale) + viewport_x;
    let screen_y = (outer_pos.y as f64 / scale) + title_bar_logical + viewport_y;

    tokio::task::spawn_blocking(move || {
        use core_graphics::event::CGEventTapLocation;
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| "Failed to create event source".to_string())?;
        let point = CGPoint::new(screen_x, screen_y);
        let ev =
            CGEvent::new_mouse_event(source, CGEventType::MouseMoved, point, CGMouseButton::Left)
                .map_err(|_| "Failed to create mouse move".to_string())?;
        ev.post(CGEventTapLocation::Session);
        Ok(())
    })
    .await
    .map_err(|e| format!("spawn_blocking: {}", e))?
}

/// macOS: real mouse click via CGEvent.
#[cfg(target_os = "macos")]
async fn native_mouse_click_macos(
    viewport_x: f64,
    viewport_y: f64,
    click_type: &str,
) -> Result<(), String> {
    use core_graphics::event::{CGEvent, CGEventType, CGMouseButton};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;

    let window = get_main_window().await.ok_or("WEBVIEW_NOT_FOUND")?;

    // Bring the window to the foreground (CGEvent needs this to reach the right window)
    let _ = window.set_focus();

    let scale = window.scale_factor().map_err(|e| format!("scale: {}", e))?;
    let outer_pos = window
        .outer_position()
        .map_err(|e| format!("outer_pos: {}", e))?;
    let inner_pos = window
        .inner_position()
        .map_err(|e| format!("inner_pos: {}", e))?;

    // outer_position / inner_position return physical pixels (2x on Retina)
    // A CGEvent CGPoint uses logical pixels (Quartz coordinates)
    // Viewport coordinates are CSS pixels (= logical)
    //
    // Title bar height = (inner.y - outer.y) / scale (logical pixels)
    // Tauri can return the same value for inner_position and outer_position,
    // so when they are equal, use the default macOS title bar height (28px)
    let title_bar_phys = (inner_pos.y - outer_pos.y) as f64;
    let title_bar_logical = if title_bar_phys > 1.0 {
        title_bar_phys / scale
    } else {
        // inner_position == outer_position (Tauri bug)
        // Use the default macOS title bar height
        28.0
    };

    let screen_x = (outer_pos.x as f64 / scale) + viewport_x;
    let screen_y = (outer_pos.y as f64 / scale) + title_bar_logical + viewport_y;

    tracing::info!(
        "[NativeClick] viewport({}, {}) → screen({:.0}, {:.0}), scale={}, outer=({}, {}), inner=({}, {}), titlebar={:.0}px",
        viewport_x, viewport_y, screen_x, screen_y, scale,
        outer_pos.x, outer_pos.y, inner_pos.x, inner_pos.y, title_bar_logical
    );

    let point = CGPoint::new(screen_x, screen_y);
    let click_type_owned = click_type.to_string();

    // Wait for the window to activate
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // CGEvent is not Send, so run it in spawn_blocking
    tokio::task::spawn_blocking(move || {
        use core_graphics::event::CGEventTapLocation;

        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| "Failed to create event source".to_string())?;

        // Step 0: MouseMoved, which moves the cursor to the target point
        // WKWebView needs it to see the hover state and pick the click target
        let move_event = CGEvent::new_mouse_event(
            source.clone(),
            CGEventType::MouseMoved,
            point,
            CGMouseButton::Left,
        )
        .map_err(|_| "Failed to create mouse move".to_string())?;
        move_event.post(CGEventTapLocation::Session);
        std::thread::sleep(std::time::Duration::from_millis(50));

        match click_type_owned.as_str() {
            "right" => {
                let down = CGEvent::new_mouse_event(
                    source.clone(),
                    CGEventType::RightMouseDown,
                    point,
                    CGMouseButton::Right,
                )
                .map_err(|_| "Failed to create right mouse down".to_string())?;
                down.post(CGEventTapLocation::Session);

                std::thread::sleep(std::time::Duration::from_millis(50));

                let up = CGEvent::new_mouse_event(
                    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                        .map_err(|_| "source".to_string())?,
                    CGEventType::RightMouseUp,
                    point,
                    CGMouseButton::Right,
                )
                .map_err(|_| "Failed to create right mouse up".to_string())?;
                up.post(CGEventTapLocation::Session);
            }
            "double" => {
                for click_count in 1..=2u64 {
                    let down = CGEvent::new_mouse_event(
                        source.clone(),
                        CGEventType::LeftMouseDown,
                        point,
                        CGMouseButton::Left,
                    )
                    .map_err(|_| "Failed to create mouse down".to_string())?;
                    down.set_integer_value_field(
                        core_graphics::event::EventField::MOUSE_EVENT_CLICK_STATE,
                        click_count as i64,
                    );
                    down.post(CGEventTapLocation::Session);

                    std::thread::sleep(std::time::Duration::from_millis(30));

                    let up = CGEvent::new_mouse_event(
                        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                            .map_err(|_| "source".to_string())?,
                        CGEventType::LeftMouseUp,
                        point,
                        CGMouseButton::Left,
                    )
                    .map_err(|_| "Failed to create mouse up".to_string())?;
                    up.set_integer_value_field(
                        core_graphics::event::EventField::MOUSE_EVENT_CLICK_STATE,
                        click_count as i64,
                    );
                    up.post(CGEventTapLocation::Session);

                    if click_count == 1 {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            }
            _ => {
                // Left click
                let down = CGEvent::new_mouse_event(
                    source.clone(),
                    CGEventType::LeftMouseDown,
                    point,
                    CGMouseButton::Left,
                )
                .map_err(|_| "Failed to create mouse down".to_string())?;
                down.post(CGEventTapLocation::Session);

                std::thread::sleep(std::time::Duration::from_millis(50));

                let up = CGEvent::new_mouse_event(
                    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                        .map_err(|_| "source".to_string())?,
                    CGEventType::LeftMouseUp,
                    point,
                    CGMouseButton::Left,
                )
                .map_err(|_| "Failed to create mouse up".to_string())?;
                up.post(CGEventTapLocation::Session);
            }
        }

        Ok(())
    })
    .await
    .map_err(|e| format!("spawn_blocking: {}", e))?
}

/// Windows: real mouse click via SendInput.
#[cfg(target_os = "windows")]
async fn native_mouse_click_windows(
    viewport_x: f64,
    viewport_y: f64,
    click_type: &str,
) -> Result<(), String> {
    let window = get_main_window().await.ok_or("WEBVIEW_NOT_FOUND")?;

    let scale = window.scale_factor().map_err(|e| format!("scale: {}", e))?;
    let inner_pos = window
        .inner_position()
        .map_err(|e| format!("inner_pos: {}", e))?;

    let screen_x = inner_pos.x as f64 + (viewport_x * scale);
    let screen_y = inner_pos.y as f64 + (viewport_y * scale);

    let click_type_owned = click_type.to_string();

    tokio::task::spawn_blocking(move || {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
            MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
            MOUSEINPUT,
        };
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

        unsafe {
            let cx = GetSystemMetrics(SM_CXSCREEN) as f64;
            let cy = GetSystemMetrics(SM_CYSCREEN) as f64;

            // SendInput absolute coordinates: 0-65535 range
            let abs_x = ((screen_x / cx) * 65535.0) as i32;
            let abs_y = ((screen_y / cy) * 65535.0) as i32;

            let (down_flag, up_flag) = match click_type_owned.as_str() {
                "right" => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
                _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            };

            let inputs = [
                INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: abs_x,
                            dy: abs_y,
                            dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | down_flag,
                            ..Default::default()
                        },
                    },
                },
                INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: abs_x,
                            dy: abs_y,
                            dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | up_flag,
                            ..Default::default()
                        },
                    },
                },
            ];

            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }

        Ok(())
    })
    .await
    .map_err(|e| format!("spawn_blocking: {}", e))?
}
