use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HistoryRetention {
    FiveMinutes,
    TenMinutes,
    ThirtyMinutes,
    OneDay,
    OneWeek,
    OneMonth,
    SixMonths,
    OneYear,
    Forever,
}

impl HistoryRetention {
    pub fn from_id(id: &str) -> Self {
        match id {
            "5m" => Self::FiveMinutes,
            "10m" => Self::TenMinutes,
            "30m" => Self::ThirtyMinutes,
            "1d" => Self::OneDay,
            "1w" => Self::OneWeek,
            "1M" => Self::OneMonth,
            "6M" => Self::SixMonths,
            "1Y" => Self::OneYear,
            "forever" => Self::Forever,
            _ => Self::ThirtyMinutes,
        }
    }

    pub fn keep_ms(self) -> u64 {
        const MINUTE: u64 = 60_000;
        const HOUR: u64 = 60 * MINUTE;
        const DAY: u64 = 24 * HOUR;
        match self {
            Self::FiveMinutes => 5 * MINUTE,
            Self::TenMinutes => 10 * MINUTE,
            Self::ThirtyMinutes => 30 * MINUTE,
            Self::OneDay => DAY,
            Self::OneWeek => 7 * DAY,
            Self::OneMonth => 30 * DAY,
            Self::SixMonths => 6 * 30 * DAY,
            Self::OneYear => 365 * DAY,
            Self::Forever => u64::MAX,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryTurn {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: String,
    pub saved_at_ms: u64,
    pub action_id: String,
    #[serde(default)]
    pub action_name: String,
    #[serde(default)]
    pub action_kind: String,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub selection_text: Option<String>,
    pub content: String,
    #[serde(default)]
    pub turns: Vec<HistoryTurn>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct HistoryFile {
    #[serde(default)]
    entries: Vec<HistoryEntry>,
}

fn history_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    Ok(dir.join("history.json"))
}

fn load_entries(app: &AppHandle) -> Vec<HistoryEntry> {
    let Ok(path) = history_path(app) else {
        return Vec::new();
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str::<HistoryFile>(&text)
        .map(|file| file.entries)
        .unwrap_or_default()
}

fn save_entries(app: &AppHandle, entries: &[HistoryEntry]) -> Result<(), String> {
    let path = history_path(app)?;
    let file = HistoryFile {
        entries: entries.to_vec(),
    };
    let json = serde_json::to_string_pretty(&file).map_err(|error| error.to_string())?;
    fs::write(&path, json).map_err(|error| error.to_string())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[tauri::command]
pub fn save_history_entry(app: AppHandle, entry: HistoryEntry) -> Result<(), String> {
    if entry.content.trim().is_empty() {
        return Ok(());
    }
    let mut entries = load_entries(&app);
    entries.retain(|existing| existing.id != entry.id);
    entries.push(entry);
    save_entries(&app, &entries)?;
    let _ = app.emit("textlens:history-changed", ());
    Ok(())
}

#[tauri::command]
pub fn list_history(app: AppHandle, retention: String) -> Result<Vec<HistoryEntry>, String> {
    let keep_ms = HistoryRetention::from_id(&retention).keep_ms();
    let cutoff = now_ms().saturating_sub(keep_ms);
    let mut entries: Vec<HistoryEntry> = load_entries(&app)
        .into_iter()
        .filter(|entry| entry.saved_at_ms >= cutoff)
        .collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.saved_at_ms));
    let _ = save_entries(&app, &entries);
    Ok(entries)
}

#[tauri::command]
pub fn delete_history(app: AppHandle, ids: Vec<String>) -> Result<(), String> {
    let id_set: HashSet<String> = ids.into_iter().collect();
    let entries: Vec<HistoryEntry> = load_entries(&app)
        .into_iter()
        .filter(|entry| !id_set.contains(&entry.id))
        .collect();
    save_entries(&app, &entries)
}

#[tauri::command]
pub fn clear_history(app: AppHandle) -> Result<(), String> {
    save_entries(&app, &[])
}

fn format_saved_time(ms: u64) -> String {
    let days = ms / 86_400_000;
    let secs_of_day = (ms % 86_400_000) / 1000;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    // civil_from_days (Howard Hinnant algorithm)
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

fn export_markdown(entries: &[HistoryEntry]) -> String {
    let mut out = String::from("# TextLens 历史导出\n\n");
    for entry in entries {
        out.push_str(&format!(
            "## {} · {}\n",
            if entry.action_name.is_empty() { "回答".to_owned() } else { entry.action_name.clone() },
            format_saved_time(entry.saved_at_ms)
        ));
        let mut meta: Vec<String> = Vec::new();
        if let Some(provider) = &entry.provider_id {
            meta.push(format!("服务商：{provider}"));
        }
        if let Some(model) = &entry.model_id {
            meta.push(format!("模型：{model}"));
        }
        if let Some(selection) = &entry.selection_text {
            let selection = selection.trim();
            if !selection.is_empty() {
                meta.push(format!("选中文本：{selection}"));
            }
        }
        if !meta.is_empty() {
            out.push_str(&meta.join("，"));
            out.push('\n');
        }
        out.push('\n');
        if entry.turns.is_empty() {
            out.push_str(&entry.content);
            out.push_str("\n\n");
        } else {
            for turn in &entry.turns {
                let role = if turn.role == "user" { "用户" } else { "AI" };
                out.push_str(&format!("**{role}：**\n{}\n\n", turn.content));
            }
        }
        out.push_str("---\n\n");
    }
    out
}

fn export_txt(entries: &[HistoryEntry]) -> String {
    let mut out = String::from("TextLens 历史导出\n\n");
    for entry in entries {
        out.push_str(&format!(
            "{} · {}\n",
            if entry.action_name.is_empty() { "回答".to_owned() } else { entry.action_name.clone() },
            format_saved_time(entry.saved_at_ms)
        ));
        if let Some(provider) = &entry.provider_id {
            out.push_str(&format!("服务商：{provider}\n"));
        }
        if let Some(model) = &entry.model_id {
            out.push_str(&format!("模型：{model}\n"));
        }
        if let Some(selection) = &entry.selection_text {
            let selection = selection.trim();
            if !selection.is_empty() {
                out.push_str(&format!("选中文本：{selection}\n"));
            }
        }
        out.push('\n');
        if entry.turns.is_empty() {
            out.push_str(&entry.content);
            out.push_str("\n\n");
        } else {
            for turn in &entry.turns {
                let role = if turn.role == "user" { "用户" } else { "AI" };
                out.push_str(&format!("{role}：{}\n\n", turn.content));
            }
        }
        out.push_str("----------------------------------------\n\n");
    }
    out
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn export_docx(entries: &[HistoryEntry]) -> Result<Vec<u8>, String> {
    use std::io::Write;
    let mut document = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body>",
    );
    let paragraph = |text: &str, bold: bool| -> String {
        let runs = if bold {
            format!("<w:r><w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">{}</w:t></w:r>", escape_xml(text))
        } else {
            format!("<w:r><w:t xml:space=\"preserve\">{}</w:t></w:r>", escape_xml(text))
        };
        format!("<w:p>{runs}</w:p>")
    };
    document.push_str(&paragraph("TextLens 历史导出", true));
    for entry in entries {
        let title = format!(
            "{} · {}",
            if entry.action_name.is_empty() { "回答".to_owned() } else { entry.action_name.clone() },
            format_saved_time(entry.saved_at_ms)
        );
        document.push_str(&paragraph(&title, true));
        if let Some(provider) = &entry.provider_id {
            document.push_str(&paragraph(&format!("服务商：{provider}"), false));
        }
        if let Some(model) = &entry.model_id {
            document.push_str(&paragraph(&format!("模型：{model}"), false));
        }
        if let Some(selection) = &entry.selection_text {
            let selection = selection.trim();
            if !selection.is_empty() {
                document.push_str(&paragraph(&format!("选中文本：{selection}"), false));
            }
        }
        if entry.turns.is_empty() {
            document.push_str(&paragraph(&entry.content, false));
        } else {
            for turn in &entry.turns {
                let role = if turn.role == "user" { "用户" } else { "AI" };
                document.push_str(&paragraph(&format!("{role}：{}", turn.content), false));
            }
        }
    }
    document.push_str("</w:body></w:document>");

    let content_types = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/></Types>";
    let rels = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/></Relationships>";

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    zip.start_file("[Content_Types].xml", options).map_err(|error| error.to_string())?;
    zip.write_all(content_types.as_bytes()).map_err(|error| error.to_string())?;
    zip.start_file("_rels/.rels", options).map_err(|error| error.to_string())?;
    zip.write_all(rels.as_bytes()).map_err(|error| error.to_string())?;
    zip.start_file("word/document.xml", options).map_err(|error| error.to_string())?;
    zip.write_all(document.as_bytes()).map_err(|error| error.to_string())?;
    let cursor = zip.finish().map_err(|error| error.to_string())?;
    Ok(cursor.into_inner())
}

fn export_one(entry: &HistoryEntry, format: &str) -> Result<(String, Vec<u8>), String> {
    let (extension, bytes): (String, Vec<u8>) = match format {
        "md" => ("md".to_owned(), export_markdown(std::slice::from_ref(entry)).into_bytes()),
        "txt" => ("txt".to_owned(), export_txt(std::slice::from_ref(entry)).into_bytes()),
        "docx" => ("docx".to_owned(), export_docx(std::slice::from_ref(entry))?),
        _ => (
            "json".to_owned(),
            serde_json::to_string_pretty(entry).map_err(|error| error.to_string())?.into_bytes(),
        ),
    };
    Ok((extension, bytes))
}

#[tauri::command]
pub fn export_history(
    app: AppHandle,
    ids: Vec<String>,
    format: String,
    merge: bool,
) -> Result<String, String> {
    let id_set: HashSet<String> = ids.into_iter().collect();
    let selected: Vec<HistoryEntry> = load_entries(&app)
        .into_iter()
        .filter(|entry| id_set.contains(&entry.id))
        .collect();
    if selected.is_empty() {
        return Err("没有可导出的历史记录".to_owned());
    }
    let dir = app
        .path()
        .download_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let stamp = now_ms();
    if merge || selected.len() == 1 {
        let (extension, bytes): (String, Vec<u8>) = match format.as_str() {
            "md" => ("md".to_owned(), export_markdown(&selected).into_bytes()),
            "txt" => ("txt".to_owned(), export_txt(&selected).into_bytes()),
            "docx" => ("docx".to_owned(), export_docx(&selected)?),
            _ => (
                "json".to_owned(),
                serde_json::to_string_pretty(&selected).map_err(|error| error.to_string())?.into_bytes(),
            ),
        };
        let path = dir.join(format!("TextLens-历史导出-{stamp}.{extension}"));
        fs::write(&path, bytes).map_err(|error| error.to_string())?;
        return Ok(path.to_string_lossy().to_string());
    }
    let mut count = 0usize;
    for (index, entry) in selected.iter().enumerate() {
        let (extension, bytes) = export_one(entry, &format)?;
        let path = dir.join(format!("TextLens-历史导出-{stamp}-{}.{extension}", index + 1));
        fs::write(&path, bytes).map_err(|error| error.to_string())?;
        count += 1;
    }
    Ok(format!("已导出 {count} 个文件到：{}", dir.to_string_lossy()))
}
