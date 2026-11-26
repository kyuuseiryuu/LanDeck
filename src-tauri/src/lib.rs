mod actions;
mod apps;
pub mod audio;
mod certs;
mod models;
mod server;
mod state;

use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

use sysinfo::{Networks, System};
#[cfg(target_os = "macos")]
use tauri::menu::CheckMenuItem;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, State, WindowEvent,
};
use uuid::Uuid;

use crate::actions::execute_action;
use crate::apps::scan_local_apps;
use crate::models::{LayoutConfig, LocalAppInfo, ScreenRatioPreset, ServerInfo, SlotAction, SystemStats};
use crate::state::AppState;

fn pick_port(preferred: u16) -> u16 {
    if TcpListener::bind(("0.0.0.0", preferred)).is_ok() {
        return preferred;
    }
    TcpListener::bind(("0.0.0.0", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(preferred)
}

fn load_pairing_code(data_dir: &PathBuf) -> (String, bool) {
    let _ = fs::create_dir_all(data_dir);
    let code_path = data_dir.join("pairing_code.txt");
    if let Ok(existing) = fs::read_to_string(&code_path) {
        let trimmed = existing.trim().to_string();
        if !trimmed.is_empty() {
            return (trimmed, true);
        }
    }
    let num = (Uuid::new_v4().as_u128() % 9000) + 1000;
    (format!("{:04}", num), false)
}

pub fn detect_available_ips() -> Vec<crate::models::NetworkInterfaceInfo> {
    let mut result = Vec::new();
    if let Ok(ifas) = local_ip_address::list_afinet_netifas() {
        for (name, ip) in ifas {
            if let std::net::IpAddr::V4(ipv4) = ip {
                let ip_str = ipv4.to_string();
                if !ipv4.is_loopback() && !ipv4.is_link_local() {
                    if !result
                        .iter()
                        .any(|item: &crate::models::NetworkInterfaceInfo| item.ip == ip_str)
                    {
                        result.push(crate::models::NetworkInterfaceInfo { name, ip: ip_str });
                    }
                }
            }
        }
    }
    result.sort_by(|a, b| {
        let score = |ip: &str| {
            if ip.starts_with("192.168.") {
                1
            } else if ip.starts_with("10.") {
                2
            } else if ip.starts_with("172.") {
                3
            } else {
                4
            }
        };
        score(&a.ip).cmp(&score(&b.ip)).then_with(|| a.name.cmp(&b.name))
    });
    if result.is_empty() {
        let default_ip = local_ip_address::local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string());
        result.push(crate::models::NetworkInterfaceInfo {
            name: "默认网卡".to_string(),
            ip: default_ip,
        });
    }
    result
}

#[tauri::command]
async fn get_server_info(state: State<'_, AppState>) -> Result<ServerInfo, String> {
    Ok(state.server_info().await)
}

#[tauri::command]
async fn set_lan_ip(
    state: State<'_, AppState>,
    ip: String,
) -> Result<ServerInfo, String> {
    state.set_lan_ip(ip).await
}

#[tauri::command]
async fn set_pairing_code(
    state: State<'_, AppState>,
    code: String,
) -> Result<ServerInfo, String> {
    state.set_pairing_code(code).await
}

#[tauri::command]
async fn set_hide_dock_icon(
    state: State<'_, AppState>,
    hide: bool,
) -> Result<ServerInfo, String> {
    state.set_hide_dock_icon(hide).await
}

#[tauri::command]
async fn set_https_enabled(
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<ServerInfo, String> {
    state.set_https_enabled(enabled).await
}

#[tauri::command]
async fn get_online_count(state: State<'_, AppState>) -> Result<usize, String> {
    Ok(state.online_count().await)
}

#[tauri::command]
async fn list_layouts(state: State<'_, AppState>) -> Result<Vec<LayoutConfig>, String> {
    Ok(state.list_layouts().await)
}

#[tauri::command]
async fn save_layout(
    state: State<'_, AppState>,
    layout: LayoutConfig,
) -> Result<LayoutConfig, String> {
    let saved = state.save_layout(layout).await;
    let _ = state.app_handle.emit("layouts-updated", ());
    Ok(saved)
}

#[tauri::command]
async fn delete_layout(state: State<'_, AppState>, layout_id: String) -> Result<(), String> {
    state.delete_layout(&layout_id).await;
    let _ = state.app_handle.emit("layouts-updated", ());
    Ok(())
}

#[tauri::command]
async fn list_screen_ratios(state: State<'_, AppState>) -> Result<Vec<ScreenRatioPreset>, String> {
    Ok(state.list_screen_ratios().await)
}

#[tauri::command]
async fn delete_screen_ratio(
    state: State<'_, AppState>,
    ratio_key: String,
) -> Result<Vec<ScreenRatioPreset>, String> {
    let updated = state.delete_screen_ratio(&ratio_key).await;
    let _ = state.app_handle.emit("screen-ratios-updated", ());
    Ok(updated)
}

#[tauri::command]
async fn list_local_apps(state: State<'_, AppState>) -> Result<Vec<LocalAppInfo>, String> {
    let icon_cache_dir = {
        let guard = state.inner.read().await;
        guard.icon_cache_dir.clone()
    };
    tokio::task::spawn_blocking(move || scan_local_apps(&icon_cache_dir))
        .await
        .map_err(|e| format!("Failed to scan apps: {}", e))
}

#[tauri::command]
async fn save_custom_image(
    state: State<'_, AppState>,
    bytes: Vec<u8>,
    extension: String,
) -> Result<String, String> {
    let ext = extension
        .trim_start_matches('.')
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    let safe_ext = if ext.is_empty() { "png".to_string() } else { ext };
    let filename = format!("custom_{}.{}", Uuid::new_v4().simple(), safe_ext);

    let dest = {
        let guard = state.inner.read().await;
        guard.icon_cache_dir.join(&filename)
    };

    tokio::fs::write(&dest, &bytes)
        .await
        .map_err(|e| format!("Failed to save image: {}", e))?;

    Ok(format!("/icons/{}", filename))
}

#[tauri::command]
async fn trigger_slot_action(app: tauri::AppHandle, action: SlotAction) -> Result<(), String> {
    tokio::task::spawn_blocking(move || execute_action(&app, &action))
        .await
        .map_err(|e| format!("Action task error: {}", e))?
}

#[tauri::command]
fn check_accessibility_permission(prompt: bool) -> bool {
    crate::actions::is_accessibility_trusted(prompt)
}

#[tauri::command]
fn open_accessibility_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .spawn()
            .map_err(|e| format!("Failed to open System Settings: {}", e))?;
    }
    #[cfg(target_os = "windows")]
    {
        let _ = crate::actions::open_windows_path_or_url("ms-settings:easeofaccess-keyboard");
    }
    Ok(())
}

#[tauri::command]
fn open_external_url(url: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&url)
            .spawn()
            .map_err(|e| format!("Failed to open URL: {}", e))?;
    }
    #[cfg(target_os = "windows")]
    {
        crate::actions::open_windows_path_or_url(&url)?;
    }
    Ok(())
}

#[tauri::command]
async fn fetch_url_text(url: String) -> Result<String, String> {
    let clean = url.trim().to_string();
    if !(clean.starts_with("http://") || clean.starts_with("https://")) {
        return Err("请输入以 http:// 或 https:// 开头的有效 URL".into());
    }

    tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new("curl");
        cmd.arg("-sSL")
            .arg("-f")
            .arg("--max-time")
            .arg("15")
            .arg("--max-filesize")
            .arg("5242880")
            .arg(&clean);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        let out = cmd.output().map_err(|e| format!("请求失败: {}", e))?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let msg = stderr.trim();
            return Err(if msg.is_empty() {
                "无法获取该 URL 内容，请检查链接是否可访问".into()
            } else {
                format!("获取失败: {}", msg)
            });
        }

        String::from_utf8(out.stdout).map_err(|_| "返回内容不是有效的 UTF-8 文本".into())
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))?
}

#[tauri::command]
async fn list_audio_devices() -> Result<Vec<crate::audio::AudioDeviceInfo>, String> {
    tokio::task::spawn_blocking(crate::audio::list_audio_devices)
        .await
        .map_err(|e| format!("Task error: {}", e))?
}

#[tauri::command]
async fn set_default_audio_device(
    state: State<'_, AppState>,
    device_id: String,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || crate::audio::set_default_audio_device(&device_id))
        .await
        .map_err(|e| format!("Task error: {}", e))??;

    if let Ok(devices) = crate::audio::list_audio_devices() {
        state.broadcast_audio_devices(&devices).await;
        let _ = state.app_handle.emit("audio-devices-updated", &devices);
    }
    Ok(())
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();

    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        show_main_window(app);
    }));

    builder
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| PathBuf::from(".landeck-data"));
            let available_ips = detect_available_ips();
            let lan_ip = available_ips
                .first()
                .map(|ifa| ifa.ip.clone())
                .unwrap_or_else(|| "127.0.0.1".to_string());
            let port = pick_port(19820);
            let (pairing_code, pairing_configured) = load_pairing_code(&data_dir);

            let state = AppState::new(
                app.handle().clone(),
                data_dir,
                lan_ip,
                port,
                pairing_code,
                pairing_configured,
            );

            #[cfg(target_os = "macos")]
            {
                let hide = tauri::async_runtime::block_on(async {
                    state.inner.read().await.hide_dock_icon
                });
                if hide {
                    let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                }
            }

            app.manage(state.clone());

            let server_state = state.clone();
            tauri::async_runtime::spawn(async move {
                server::start_server(server_state, port).await;
            });

            let https_state = state.clone();
            tauri::async_runtime::spawn(async move {
                let enabled = https_state.inner.read().await.https_enabled;
                if enabled {
                    let _ = https_state.set_https_enabled(true).await;
                }
            });

            let stats_state = state;
            tauri::async_runtime::spawn(async move {
                let mut sys = System::new_all();
                let mut networks = Networks::new_with_refreshed_list();
                loop {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    sys.refresh_cpu_usage();
                    sys.refresh_memory();
                    networks.refresh();

                    let cpu = sys.global_cpu_usage();
                    let total_mem = sys.total_memory() as f64;
                    let used_mem = sys.used_memory() as f64;
                    let mem_percent = if total_mem > 0.0 {
                        ((used_mem / total_mem) * 100.0) as f32
                    } else {
                        0.0
                    };
                    let mem_used_gb = (used_mem / 1_073_741_824.0) as f32;
                    let mem_total_gb = (total_mem / 1_073_741_824.0) as f32;

                    let mut rx_bytes: u64 = 0;
                    let mut tx_bytes: u64 = 0;
                    for (_name, net) in &networks {
                        rx_bytes = rx_bytes.saturating_add(net.received());
                        tx_bytes = tx_bytes.saturating_add(net.transmitted());
                    }
                    let net_rx_kbps = (rx_bytes as f64) / 1024.0 / 1.5;
                    let net_tx_kbps = (tx_bytes as f64) / 1024.0 / 1.5;

                    let stats = SystemStats {
                        cpu,
                        mem_percent,
                        mem_used_gb,
                        mem_total_gb,
                        net_rx_kbps,
                        net_tx_kbps,
                    };

                    stats_state.broadcast_system_stats(&stats).await;
                    let _ = stats_state.app_handle.emit("system-stats", &stats);
                }
            });

            let show_i = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "退出 LanDeck", true, None::<&str>)?;

            #[cfg(target_os = "macos")]
            let menu = {
                let initial_hide = tauri::async_runtime::block_on(async {
                    app.state::<AppState>().inner.read().await.hide_dock_icon
                });
                let dock_i = CheckMenuItem::with_id(
                    app,
                    "toggle_dock",
                    "隐藏 Dock 图标",
                    true,
                    initial_hide,
                    None::<&str>,
                )?;
                tauri::async_runtime::block_on(async {
                    app.state::<AppState>().inner.write().await.dock_menu_item =
                        Some(dock_i.clone());
                });
                Menu::with_items(app, &[&show_i, &dock_i, &quit_i])?
            };

            #[cfg(not(target_os = "macos"))]
            let menu = Menu::with_items(app, &[&show_i, &quit_i])?;

            let mut tray_builder = TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("LanDeck 正在后台运行")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        show_main_window(app);
                    }
                    #[cfg(target_os = "macos")]
                    "toggle_dock" => {
                        let app_handle = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let state = app_handle.state::<AppState>();
                            let current = state.inner.read().await.hide_dock_icon;
                            let _ = state.set_hide_dock_icon(!current).await;
                        });
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                });

            if let Some(icon) = app.default_window_icon() {
                tray_builder = tray_builder.icon(icon.clone());
            }
            tray_builder.build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_server_info,
            set_lan_ip,
            set_pairing_code,
            set_hide_dock_icon,
            set_https_enabled,
            get_online_count,
            list_layouts,
            save_layout,
            delete_layout,
            list_screen_ratios,
            delete_screen_ratio,
            list_local_apps,
            save_custom_image,
            trigger_slot_action,
            check_accessibility_permission,
            open_accessibility_settings,
            open_external_url,
            fetch_url_text,
            list_audio_devices,
            set_default_audio_device,
        ])
        .build(tauri::generate_context!())
        .expect("error while building LanDeck")
        .run(|_app_handle, _event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = _event {
                show_main_window(_app_handle);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_available_ips() {
        let ips = detect_available_ips();
        println!("Available IPs: {:?}", ips);
        assert!(!ips.is_empty(), "Should detect at least one IP");
        for ifa in &ips {
            assert!(!ifa.ip.starts_with("169.254."), "Should not include link-local");
        }
    }
}
