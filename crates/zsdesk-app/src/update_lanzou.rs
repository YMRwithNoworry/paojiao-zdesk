//! Anonymous access to the application's public Lanzou release folder.
use aes::{cipher::{BlockEncrypt, KeyInit}, Aes128};
use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) const MANIFEST: &str = "zsdesk-update.json";
const API: &str = "https://apix.ilanzou.com/unproved";
const PAGE_SIZE: usize = 60;

fn encode(text: &str) -> String {
    let cipher = Aes128::new(b"lanZouY-disk-app".into());
    let mut data = text.as_bytes().to_vec();
    let pad = 16 - data.len() % 16;
    data.resize(data.len() + pad, pad as u8);
    for block in data.chunks_exact_mut(16) {
        cipher.encrypt_block(block.into());
    }
    data.iter().map(|b| format!("{b:02X}")).collect()
}

fn numeric_id(v: &Value) -> Result<String> {
    if let Some(id) = v.as_u64() { return Ok(id.to_string()); }
    let id = v.as_str().context("蓝奏文件标识缺失")?;
    ensure!(!id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_digit()), "蓝奏文件标识无效");
    Ok(id.to_owned())
}

fn directory_request(url: String, post: &mut impl FnMut(&str) -> Result<Vec<u8>>) -> Result<Value> {
    let response: Value = serde_json::from_slice(&post(&url)?)?;
    ensure!(response["code"].as_i64() == Some(200) && response["status"].as_i64().unwrap_or(0) == 0, "蓝奏公开目录暂不可用");
    Ok(response)
}

fn find_named(entries: &[Value], name: &str, found: &mut Option<String>) -> Result<()> {
    for entry in entries.iter().filter(|item| item["fileName"].as_str() == Some(name)) {
        ensure!(found.is_none(), "蓝奏目录中有重复的更新文件");
        *found = Some(numeric_id(&entry["fileId"])?);
    }
    Ok(())
}

pub(super) fn resolve(folder: &str, name: &str, mut post: impl FnMut(&str) -> Result<Vec<u8>>) -> Result<String> {
    let share = folder.trim().strip_prefix("https://www.ilanzou.com/s/")
        .or_else(|| folder.trim().strip_prefix("https://ilanzou.com/s/")).context("蓝奏公开目录地址无效")?;
    ensure!(!share.is_empty() && share.len() <= 128 && share.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b,b'_'|b'-')), "蓝奏分享标识无效");
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis().to_string();
    let uuid = format!("{:?}", unsafe { windows::Win32::System::Com::CoCreateGuid()? }).to_ascii_lowercase();
    let timestamp = encode(&now);
    let fields = format!("devType=6&devModel=Chrome&uuid={uuid}&extra=2&timestamp={timestamp}&code=&shareId={share}");
    let root = directory_request(format!("{API}/recommend/list?{fields}&type=0&offset=1&limit={PAGE_SIZE}"), &mut post)?;
    let shares=root["list"].as_array().context("蓝奏分享信息缺失")?;
    let [record]=shares.as_slice() else{bail!("蓝奏公开接口返回 {} 个分享条目，需要一个明确的目录",shares.len())};
    ensure!(record["amt"].as_f64().unwrap_or(0.0)==0.0 && record["code"].as_str().unwrap_or("").is_empty(), "蓝奏目录需要额外授权");
    let files = record["fileList"].as_array().context("蓝奏目录没有文件列表")?;
    let mut found = None;
    find_named(files, name, &mut found)?;
    if found.is_none() {
        let [entry] = files.as_slice() else { bail!("蓝奏更新文件缺失") };
        let folder_id = numeric_id(&entry["folderId"])?;
        for page in 1..=4 {
            let data = directory_request(format!("{API}/share/list?{fields}&folderId={folder_id}&offset={page}&limit={PAGE_SIZE}"), &mut post)?;
            let entries = data["list"].as_array().context("蓝奏目录列表无效")?;
            find_named(entries, name, &mut found)?;
            if entries.len() < PAGE_SIZE || data["total"].as_u64().is_some_and(|total|total<= (page*PAGE_SIZE) as u64) { break; }
            ensure!(page < 4,"蓝奏更新目录超过容量限制");
        }
    }
    let id = found.context("蓝奏目录中没有此更新文件")?;
    let download_id = encode(&format!("{id}|"));
    let auth = encode(&format!("{id}|{now}"));
    Ok(format!("{API}/file/redirect?downloadId={download_id}&enable=0&devType=6&uuid={uuid}&timestamp={timestamp}&auth={auth}&shareId={share}"))
}
