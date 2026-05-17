#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod pty;
mod sim_commands;
mod vcd_viewer;

use pty::{PtyManager, SessionId};
use sim_commands::{
    sim_driver_at, sim_end, sim_eval, sim_query_active, sim_seek, sim_start, sim_step,
    DebuggerSessions,
};
use vcd_viewer::{vcd_close, vcd_find_edge, vcd_open, vcd_query, VcdSessionHolder};
use serde::Serialize;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::menu::{Menu, SubmenuBuilder};
use tauri::{Emitter, Manager, State};

#[derive(Debug)]
struct ActionLogger {
    path: Mutex<PathBuf>,
}

impl ActionLogger {
    fn append(&self, entry: &ActionLogEntry) -> Result<(), String> {
        let path = self.path.lock().map_err(|e| e.to_string())?.clone();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        let line =
            serde_json::to_string(entry).map_err(|e| format!("serialize log entry: {e}"))?;
        file.write_all(line.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .map_err(|e| e.to_string())
    }
}

#[derive(Debug, Serialize)]
struct ActionLogEntry {
    ts_unix_ms: u128,
    event: String,
    meta: Value,
}

#[tauri::command]
async fn log_action(
    logger: State<'_, ActionLogger>,
    event: String,
    meta: Option<Value>,
) -> Result<(), String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?;
    let entry = ActionLogEntry {
        ts_unix_ms: now.as_millis(),
        event,
        meta: meta.unwrap_or(Value::Null),
    };
    logger.append(&entry)
}

#[tauri::command]
async fn parse_file(path: String) -> Result<verilog_core::ParseResult, String> {
    let p = Path::new(&path);
    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_ascii_lowercase();
        if ext != "v" && ext != "sv" {
            return Ok(verilog_core::ParseResult {
                modules: vec![],
                diagnostics: vec![],
            });
        }
    } else {
        return Ok(verilog_core::ParseResult {
            modules: vec![],
            diagnostics: vec![],
        });
    }

    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    Ok(verilog_core::parse_file(path, &content))
}

#[tauri::command]
async fn index_project(root: String) -> Result<verilog_core::ProjectIndex, String> {
    let path = Path::new(&root);
    verilog_core::index_project(path).map_err(|e| e.to_string())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompilerInfo {
    verilog_core_version: &'static str,
}

/// Embeds the `verilog-core` revision linked into this app (for debugging stale builds).
#[tauri::command]
fn compiler_info() -> CompilerInfo {
    CompilerInfo {
        verilog_core_version: verilog_core::PACKAGE_VERSION,
    }
}

/// **File → Generate VCD** — IEEE 1364 Verilog → VCD via the same path as `csverilog` (`run_csverilog_pipeline`), in-process.
#[tauri::command]
fn simulate_vcd(
    root: String,
    cycles: Option<usize>,
    output_file: Option<String>,
) -> Result<String, String> {
    let root_path = Path::new(&root);
    let name = output_file.unwrap_or_else(|| "circuit_scope.vcd".into());
    if name.contains('/') || name.contains('\\') || name.is_empty() {
        return Err("output_file must be a file name only, no path separators".into());
    }

    let verilog_paths = verilog_core::list_verilog_source_paths(root_path).map_err(|e| e.to_string())?;
    if verilog_paths.is_empty() {
        return Err("No Verilog sources (.v or .sv) found under this folder.".into());
    }

    let out_path = root_path.join(&name);
    let vcd = verilog_core::run_csverilog_pipeline(
        &verilog_paths,
        &out_path,
        "simulate_vcd (in-process, Circuit Scope)",
        verilog_core::CsVerilogOptions {
            num_cycles: cycles,
            ..Default::default()
        },
    )?;
    std::fs::write(&out_path, &vcd).map_err(|e| e.to_string())?;
    Ok(out_path.to_string_lossy().to_string())
}

#[tauri::command]
async fn create_pty(
    app: tauri::AppHandle,
    manager: State<'_, PtyManager>,
    shell: Option<String>,
    cwd: Option<String>,
) -> Result<u64, String> {
    manager
        .create_session(&app, shell, cwd)
        .map(|id| id.0)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn write_pty(manager: State<'_, PtyManager>, session_id: u64, data: String) -> Result<(), String> {
    manager
        .write(SessionId(session_id), &data)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn resize_pty(
    manager: State<'_, PtyManager>,
    session_id: u64,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    manager
        .resize(SessionId(session_id), cols, rows)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn close_pty(manager: State<'_, PtyManager>, session_id: u64) -> Result<(), String> {
    manager
        .close(SessionId(session_id))
        .map_err(|e| e.to_string())
}

/// Create an empty file at parent_dir/name. Name must not contain path separators.
#[tauri::command]
async fn create_file(parent_dir: String, name: String) -> Result<(), String> {
    if name.contains('/') || name.contains('\\') || name.is_empty() {
        return Err("Invalid file name".to_string());
    }
    let path = Path::new(&parent_dir).join(&name);
    if path.exists() {
        return Err(format!("Already exists: {}", name));
    }
    std::fs::File::create(&path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Create a directory at parent_dir/name. Name must not contain path separators.
#[tauri::command]
async fn create_dir(parent_dir: String, name: String) -> Result<(), String> {
    if name.contains('/') || name.contains('\\') || name.is_empty() {
        return Err("Invalid folder name".to_string());
    }
    let path = Path::new(&parent_dir).join(&name);
    if path.exists() {
        return Err(format!("Already exists: {}", name));
    }
    std::fs::create_dir(&path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Move or rename a file/folder from `from` to `to`. Works for same filesystem.
#[tauri::command]
async fn move_path(from: String, to: String) -> Result<(), String> {
    let from_path = Path::new(&from);
    let to_path = Path::new(&to);
    if !from_path.exists() {
        return Err("Source does not exist".to_string());
    }
    if to_path.exists() && to_path.is_dir() {
        let name = from_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("Invalid source name")?;
        let dest = to_path.join(name);
        if dest.exists() {
            return Err(format!("Destination already exists: {}", name));
        }
        std::fs::rename(from_path, &dest).map_err(|e| e.to_string())?;
    } else if to_path.exists() {
        return Err("Destination already exists".to_string());
    } else {
        std::fs::rename(from_path, to_path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Delete a file or directory (recursively for directories).
#[tauri::command]
async fn delete_path(path: String) -> Result<(), String> {
    let p = Path::new(&path);
    if !p.exists() {
        return Ok(());
    }
    let meta = std::fs::metadata(p).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        std::fs::remove_dir_all(p).map_err(|e| e.to_string())?;
    } else {
        std::fs::remove_file(p).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .setup(|app| {
            // Set up action logger in app data directory.
            let app_data_dir = app
                .path()
                .app_data_dir()
                .map_err(|e| format!("app_data_dir: {e}"))?;
            std::fs::create_dir_all(&app_data_dir).ok();
            let log_path = app_data_dir.join("actions.log");
            app.manage(ActionLogger {
                path: Mutex::new(log_path),
            });
            app.manage(PtyManager::new());
            app.manage(VcdSessionHolder::default());
            app.manage(DebuggerSessions::default());

            let handle = app.handle().clone();
            // On macOS the first submenu becomes the app-name menu; add it first so "File" is separate.
            let app_submenu = SubmenuBuilder::new(&handle, "Circuit Scope")
                .text("about", "About Circuit Scope")
                .separator()
                .quit()
                .build()?;
            let file_submenu = SubmenuBuilder::new(&handle, "File")
                .text("open_folder", "Open Folder...")
                .text("open_file", "Open File...")
                .text("save", "Save")
                .separator()
                .text("generate_vcd", "Generate VCD in Project Folder")
                .build()?;

            // Standard Edit actions wire OS clipboard shortcuts (Cmd+C/V/X, Ctrl+C/V/X) into WKWebView
            // on macOS; without this menu, copy/paste keyboard commands often do nothing.
            let edit_submenu = SubmenuBuilder::new(&handle, "Edit")
                .cut()
                .copy()
                .paste()
                .separator()
                .select_all()
                .build()?;

            let terminal_submenu = SubmenuBuilder::new(&handle, "Terminal")
                .text("open_new_terminal", "Open New Terminal")
                .text("close_terminal", "Close Terminal")
                .build()?;
            let view_submenu = SubmenuBuilder::new(&handle, "View")
                .text("close_waveform", "Close Waveform Viewer")
                .build()?;
            let menu = Menu::new(&handle)?;
            menu.append(&app_submenu)?;
            menu.append(&file_submenu)?;
            menu.append(&edit_submenu)?;
            menu.append(&view_submenu)?;
            menu.append(&terminal_submenu)?;
            let _ = app.set_menu(menu);

            app.on_menu_event(move |app, event| {
                let id = event.id().as_ref();
                match id {
                    "open_folder" => {
                        let _ = app.emit("menu-open-folder", ());
                    }
                    "open_file" => {
                        let _ = app.emit("menu-open-file", ());
                    }
                    "save" => {
                        let _ = app.emit("menu-save", ());
                    }
                    "generate_vcd" => {
                        let _ = app.emit("menu-generate-vcd", ());
                    }
                    "open_new_terminal" => {
                        let _ = app.emit("menu-open-new-terminal", ());
                    }
                    "close_terminal" => {
                        let _ = app.emit("menu-close-terminal", ());
                    }
                    "close_waveform" => {
                        let _ = app.emit("menu-close-waveform", ());
                    }
                    _ => {}
                };
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            compiler_info,
            parse_file,
            index_project,
            simulate_vcd,
            vcd_open,
            vcd_query,
            vcd_find_edge,
            vcd_close,
            log_action,
            create_pty,
            write_pty,
            resize_pty,
            close_pty,
            create_file,
            create_dir,
            move_path,
            delete_path,
            sim_start,
            sim_step,
            sim_query_active,
            sim_eval,
            sim_seek,
            sim_end,
            sim_driver_at
        ])
        .run(tauri::generate_context!())
        .expect("error while running Circuit Scope application");
}

