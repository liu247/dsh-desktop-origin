//! DeepSeek Harness desktop shell: window, tray, and the dsh web service
//! lifecycle.
//!
//! The shell renders the exact SPA and plugin registry the browser edition
//! serves: it spawns `dsh --profile web` (dev: the checkout's CLI from source;
//! release: the bundled runtime), waits for the readiness line, and navigates
//! the window to the printed URL. Every harness feature — host and client
//! plugins, sessions, the API gateway — runs unchanged inside that service.
//!
//! Window close hides to the tray instead of quitting; the tray menu owns
//! show and quit. Quitting stops the service (SIGTERM, then SIGKILL). An
//! unexpected service exit restarts the service once and reloads the window.

mod service;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Emitter, Listener, LogicalSize, Manager, WebviewUrl, WebviewWindowBuilder};

use service::{ServiceError, ServiceHandle};

/// Restart backoff between an unexpected exit and the respawn.
const RESTART_DELAY: Duration = Duration::from_secs(1);

/// Desktop-environment adapter injected after every page load. The WebView's
/// content area already sits below the native title bar (innerHeight excludes
/// it), so plugins that pin controls to the viewport top — like the
/// better-sidebar toggle cluster — align exactly as they do in the browser,
/// with the plugin's own tab bar. The shell's one job here is to open the
/// left sidebar on first paint when the narrow-viewport rail is showing.
const DESKTOP_ADAPTER_JS: &str = r#"
(() => {
  const log = (...args) => console.log('[dsh-desktop]', ...args)
  log('innerWidth=' + window.innerWidth + ' height=' + window.innerHeight)
  // Expand the left sidebar on first paint when the rail is showing.
  const tryExpandSidebar = () => {
    const toggle = document.querySelector('[data-slot="sidebar"] button[aria-label="展开侧边栏"]')
    if (toggle !== null) {
      toggle.click()
      log('clicked sidebar expand toggle')
    } else {
      log('no collapsed-sidebar toggle found (sidebar likely expanded)')
    }
  }
  window.setTimeout(tryExpandSidebar, 1800)
  // aionui-panel's side card starts collapsed on first run: collapse it once
  // when no persistable preference exists yet (its toggle writes
  // project-panel-collapse:<root>), then leave the user's own choice alone.
  const collapseAionuiPanelOnce = () => {
    const hasPref = Object.keys(localStorage).some(k => k.startsWith('project-panel-collapse:'))
    if (hasPref) return
    const btn = [...document.querySelectorAll('button[aria-label="收起面板"]')][0]
    if (btn !== undefined) {
      btn.click()
      log('collapsed aionui side card (first run)')
    } else {
      window.setTimeout(collapseAionuiPanelOnce, 1200)
    }
  }
  window.setTimeout(collapseAionuiPanelOnce, 2500)
  // The aionui-panel floating expand button (shown when its side card is
  // collapsed) docks at the top-right corner and would overlap the
  // better-sidebar toggle cluster (both pin near top:3-31px). Nudge it below
  // the cluster; !important wins over the plugin's inline top.
  const style = document.createElement('style')
  style.textContent = '.aionui-floating-expand { top: 44px !important }'
  document.head.appendChild(style)
})()
"#;

/// The settings file the harness persists user preferences to.
const DSH_SETTINGS_FILE: &str = "settings.yaml";

/// One-time desktop preference defaults: the better-sidebar side card starts
/// collapsed (openByDefault). The native title bar needs no strip handling —
/// the WebView content area already sits below it — so titleBarCompat stays
/// off, keeping the plugin's pinned controls aligned with its own tab bar
/// exactly as in the browser. Written only when the namespace is absent, so
/// the user's own settings always win.
const DESKTOP_SETTINGS_BLOCK: &str = "\ndsh-better-sidebar:\n  openByDefault: false\n";

/// Ensure the desktop prefs defaults exist in `~/.dsh/settings.yaml`. A no-op
/// when the namespace is already present or the file is unreadable.
fn ensure_desktop_settings(home: PathBuf) {
    let path = home.join(".dsh").join(DSH_SETTINGS_FILE);
    let Ok(content) = fs::read_to_string(&path) else { return };
    if content.contains("dsh-better-sidebar:") { return }
    let mut next = content.trim_end().to_string();
    next.push('\n');
    next.push_str(DESKTOP_SETTINGS_BLOCK);
    if fs::write(&path, next).is_ok() {
        println!("dsh-desktop: wrote desktop prefs defaults to {path:?}");
    }
}

/// Shared shell state: the running service pid/url, and whether the shell is
/// quitting (an exit then is expected and not restarted).
struct ShellState {
    service: Mutex<Option<ServiceHandle>>,
    quitting: AtomicBool,
}

/// Spawn the service for the current mode (dev from source, release from
/// bundled resources) and return its handle plus the exit-code receiver.
fn launch_service(app: &AppHandle) -> Result<(ServiceHandle, Receiver<Option<i32>>), ServiceError> {
    let command = if tauri::is_dev() {
        service::dev_service_command()?
    } else {
        let resource_dir = app.path().resource_dir().expect("resource dir resolves");
        service::bundled_service_command(&resource_dir)?
    };
    service::spawn_service(command)
}

/// Navigate the main window to the service URL.
fn navigate_main(app: &AppHandle, url: &str) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.navigate(url.parse().expect("service URL parses"));
    }
}

/// Restart the service and re-point the window; used after an unexpected exit.
fn restart_service(app: &AppHandle) {
    let state = app.state::<ShellState>();
    // Clear the stale handle so a quit during the backoff window does not try
    // to signal a dead pid.
    *state.service.lock().expect("service mutex") = None;
    thread::sleep(RESTART_DELAY);
    if state.quitting.load(Ordering::SeqCst) {
        return;
    }
    match launch_service(app) {
        Ok((handle, exit_rx)) => {
            if let Some(url) = handle.url.clone() {
                navigate_main(app, &url);
            }
            *state.service.lock().expect("service mutex") = Some(handle);
            watch_service(app.clone(), exit_rx);
        }
        Err(error) => {
            let _ = app.emit("service-error", error.to_string());
        }
    }
}

/// Watch a spawned service's exit channel; on an unexpected exit, restart.
fn watch_service(app: AppHandle, exit_rx: Receiver<Option<i32>>) {
    thread::spawn(move || {
        let _code = exit_rx.recv();
        if app.state::<ShellState>().quitting.load(Ordering::SeqCst) {
            return;
        }
        // The service died under us; bring it back and reload the window.
        restart_service(&app);
    });
}

/// Build the tray icon and menu: Show/Hide the main window and Quit.
fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&show, &separator, &quit])?;

    TrayIconBuilder::with_id("main-tray")
        .icon(app.default_window_icon().cloned().expect("window icon is bundled"))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "quit" => quit_shell(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })
        .build(app)?;
    Ok(())
}

/// Stop the service and exit the process.
fn quit_shell(app: &AppHandle) {
    let state = app.state::<ShellState>();
    state.quitting.store(true, Ordering::SeqCst);
    let handle = state.service.lock().expect("service mutex").take();
    if let Some(handle) = handle {
        service::stop_service(handle.pid);
    }
    app.exit(0);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            app.manage(ShellState {
                service: Mutex::new(None),
                quitting: AtomicBool::new(false),
            });

            // The window opens on the bundled loading page; the service
            // readiness listener navigates it to the real GUI.
            let window = WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::App("index.html".into()),
            )
            .title("DeepSeek Harness")
            .inner_size(1360.0, 860.0)
            .min_inner_size(880.0, 560.0)
            .on_page_load(|window, payload| {
                if matches!(payload.event(), PageLoadEvent::Finished) {
                    let _ = window.eval(DESKTOP_ADAPTER_JS);
                }
            })
            .build()?;
            // macOS may restore a previously saved window frame (possibly a
            // small one) over the requested size. The web shell auto-collapses
            // the sidebar below SIDEBAR_AUTO_COLLAPSE (1024px), so force the
            // default size once at startup to guarantee the expanded layout.
            let _ = window.set_size(LogicalSize::new(1360.0, 860.0));

            // One-time desktop prefs defaults (better-sidebar collapsed by
            // default, title-bar compatible toggle). Runs before the service
            // boots so the freshly spawned server reads them.
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            ensure_desktop_settings(home);

            let handle = app.handle().clone();

            // Hide to tray on close; the tray owns the real quit.
            window.on_window_event({
                let handle = handle.clone();
                move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        if let Some(window) = handle.get_webview_window("main") {
                            let _ = window.hide();
                        }
                    }
                }
            });

            // Navigate once the service reports ready.
            handle.listen("service-ready", {
                let handle = handle.clone();
                move |event| {
                    let url = event.payload();
                    if !url.is_empty() {
                        navigate_main(&handle, url);
                    }
                }
            });

            // Launch the service and watch it.
            match launch_service(&handle) {
                Ok((service_handle, exit_rx)) => {
                    if let Some(url) = service_handle.url.clone() {
                        navigate_main(&handle, &url);
                    }
                    *handle.state::<ShellState>().service.lock().expect("service mutex") =
                        Some(service_handle);
                    watch_service(handle.clone(), exit_rx);
                }
                Err(error) => {
                    let _ = handle.emit("service-error", error.to_string());
                }
            }

            build_tray(&handle)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the desktop shell");
}
