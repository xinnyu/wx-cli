use anyhow::{Context, Result};
use chrono::{Local, TimeZone};
use regex::Regex;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::OnceLock;

use super::cache::DbCache;

/// 静态编译的 Msg 表名正则，避免在热路径中重复编译
fn msg_table_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^Msg_[0-9a-f]{32}$").unwrap())
}

/// 联系人名称缓存
#[derive(Clone)]
pub struct Names {
    /// username -> display_name
    pub map: HashMap<String, String>,
    /// md5(username) -> username（用于从 Msg_<md5> 表名反推联系人）
    pub md5_to_uname: HashMap<String, String>,
    /// 消息 DB 的相对路径列表（message/message_N.db）
    pub msg_db_keys: Vec<String>,
}

impl Names {
    pub fn display(&self, username: &str) -> String {
        self.map.get(username).cloned().unwrap_or_else(|| username.to_string())
    }
}

/// 加载联系人缓存（从 contact/contact.db）
pub async fn load_names(db: &DbCache) -> Result<Names> {
    let path = db.get("contact/contact.db").await?;
    let mut map = HashMap::new();
    if let Some(p) = path {
        let p2 = p.clone();
        let rows: Vec<(String, String, String)> = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&p2).context("打开 contact.db 失败")?;
            let mut stmt = conn.prepare(
                "SELECT username, nick_name, remark FROM contact"
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1).unwrap_or_default(),
                    row.get::<_, String>(2).unwrap_or_default(),
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, anyhow::Error>(rows)
        }).await??;

        for (uname, nick, remark) in rows {
            let display = if !remark.is_empty() { remark }
                else if !nick.is_empty() { nick }
                else { uname.clone() };
            map.insert(uname, display);
        }
    }

    let md5_to_uname: HashMap<String, String> = map.keys()
        .map(|u| (format!("{:x}", md5::compute(u.as_bytes())), u.clone()))
        .collect();

    Ok(Names { map, md5_to_uname, msg_db_keys: Vec::new() })
}

/// 查询最近会话列表
pub async fn q_sessions(db: &DbCache, names: &Names, limit: usize) -> Result<Value> {
    let path = db.get("session/session.db").await?
        .context("无法解密 session.db")?;

    let path2 = path.clone();
    let limit_val = limit;
    let rows: Vec<(String, i64, Vec<u8>, i64, i64, String, String)> = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&path2)?;
        let mut stmt = conn.prepare(
            "SELECT username, unread_count, summary, last_timestamp,
                    last_msg_type, last_msg_sender, last_sender_display_name
             FROM SessionTable
             WHERE last_timestamp > 0
             ORDER BY last_timestamp DESC LIMIT ?"
        )?;
        let rows = stmt.query_map([limit_val as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1).unwrap_or(0),
                get_content_bytes(row, 2),
                row.get::<_, i64>(3).unwrap_or(0),
                row.get::<_, i64>(4).unwrap_or(0),
                row.get::<_, String>(5).unwrap_or_default(),
                row.get::<_, String>(6).unwrap_or_default(),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok::<_, anyhow::Error>(rows)
    }).await??;

    let mut results = Vec::new();
    for (username, unread, summary_bytes, ts, msg_type, sender, sender_name) in rows {
        let display = names.display(&username);
        let is_group = username.contains("@chatroom");

        // 尝试 zstd 解压 summary
        let summary = decompress_or_str(&summary_bytes);
        let summary = strip_group_prefix(&summary);

        let sender_display = if is_group && !sender.is_empty() {
            names.map.get(&sender).cloned().unwrap_or_else(|| {
                if !sender_name.is_empty() { sender_name.clone() } else { sender.clone() }
            })
        } else {
            String::new()
        };

        results.push(json!({
            "chat": display,
            "username": username,
            "is_group": is_group,
            "unread": unread,
            "last_msg_type": fmt_type(msg_type),
            "last_sender": sender_display,
            "summary": summary,
            "timestamp": ts,
            "time": fmt_time(ts, "%m-%d %H:%M"),
        }));
    }
    Ok(json!({ "sessions": results }))
}

/// 查询聊天记录
pub async fn q_history(
    db: &DbCache,
    names: &Names,
    chat: &str,
    limit: usize,
    offset: usize,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Value> {
    let username = resolve_username(chat, names)
        .with_context(|| format!("找不到联系人: {}", chat))?;
    let display = names.display(&username);
    let is_group = username.contains("@chatroom");

    let tables = find_msg_tables(db, names, &username).await?;
    if tables.is_empty() {
        anyhow::bail!("找不到 {} 的消息记录", display);
    }

    let mut all_msgs: Vec<Value> = Vec::new();
    for (db_path, table_name) in &tables {
        let path = db_path.clone();
        let tname = table_name.clone();
        let uname = username.clone();
        let is_group2 = is_group;
        let names_map = names.map.clone();
        let since2 = since;
        let until2 = until;
        let limit2 = limit;
        let offset2 = offset;

        let msgs: Vec<Value> = tokio::task::spawn_blocking(move || {
            query_messages(&path, &tname, &uname, is_group2, &names_map, since2, until2, limit2 + offset2, 0)
        }).await??;

        all_msgs.extend(msgs);
    }

    all_msgs.sort_by_key(|m| std::cmp::Reverse(m["timestamp"].as_i64().unwrap_or(0)));
    let paged: Vec<Value> = all_msgs.into_iter().skip(offset).take(limit).collect();
    let mut paged = paged;
    paged.sort_by_key(|m| m["timestamp"].as_i64().unwrap_or(0));

    Ok(json!({
        "chat": display,
        "username": username,
        "is_group": is_group,
        "count": paged.len(),
        "messages": paged,
    }))
}

/// 搜索消息
pub async fn q_search(
    db: &DbCache,
    names: &Names,
    keyword: &str,
    chats: Option<Vec<String>>,
    limit: usize,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Value> {
    let mut targets: Vec<(String, String, String, String)> = Vec::new(); // (path, table, display, uname)

    if let Some(chat_names) = chats {
        for chat_name in &chat_names {
            if let Some(uname) = resolve_username(chat_name, names) {
                let tables = find_msg_tables(db, names, &uname).await?;
                for (p, t) in tables {
                    targets.push((p.to_string_lossy().into_owned(), t, names.display(&uname), uname.clone()));
                }
            }
        }
    } else {
        // 全局搜索：遍历所有消息 DB
        for rel_key in &names.msg_db_keys {
            let path = match db.get(rel_key).await? {
                Some(p) => p,
                None => continue,
            };
            let path2 = path.clone();
            let md5_lookup = names.md5_to_uname.clone();
            let names_map = names.map.clone();

            let table_targets: Vec<(String, String, String, String)> = match tokio::task::spawn_blocking(move || {
                let conn = Connection::open(&path2)?;
                let mut stmt = conn.prepare(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'"
                )?;
                let table_names: Vec<String> = stmt.query_map([], |row| row.get(0))?
                    .filter_map(|r| r.ok())
                    .collect();

                let re = msg_table_re();
                let mut result = Vec::new();
                for tname in table_names {
                    if !re.is_match(&tname) {
                        continue;
                    }
                    let hash = &tname[4..];
                    let uname = md5_lookup.get(hash).cloned().unwrap_or_default();
                    let display = if uname.is_empty() {
                        String::new()
                    } else {
                        names_map.get(&uname).cloned().unwrap_or_else(|| uname.clone())
                    };
                    result.push((
                        path2.to_string_lossy().into_owned(),
                        tname,
                        display,
                        uname,
                    ));
                }
                Ok::<_, anyhow::Error>(result)
            }).await {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => { eprintln!("[search] skip DB {}: {}", rel_key, e); continue; }
                Err(e) => { eprintln!("[search] task error {}: {}", rel_key, e); continue; }
            };

            targets.extend(table_targets);
        }
    }

    // 按 db_path 分组
    let mut by_path: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
    for (p, t, d, u) in targets {
        by_path.entry(p).or_default().push((t, d, u));
    }

    let mut results: Vec<Value> = Vec::new();
    let kw = keyword.to_string();
    for (db_path, table_list) in by_path {
        let kw2 = kw.clone();
        let since2 = since;
        let until2 = until;
        let limit2 = limit * 3;

        let names_map2 = names.map.clone();
        let found: Vec<Value> = match tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut all = Vec::new();
            for (tname, display, uname) in &table_list {
                let is_group = uname.contains("@chatroom");
                match search_in_table(&conn, tname, &uname, is_group,
                    &names_map2, &kw2, since2, until2, limit2)
                {
                    Ok(rows) => {
                        for mut row in rows {
                            if row.get("chat").map(|v| v.as_str().unwrap_or("")).unwrap_or("").is_empty() {
                                if let Some(obj) = row.as_object_mut() {
                                    obj.insert("chat".into(), serde_json::Value::String(
                                        if display.is_empty() { tname.clone() } else { display.clone() }
                                    ));
                                }
                            }
                            all.push(row);
                        }
                    }
                    Err(e) => eprintln!("[search] skip table {}: {}", tname, e),
                }
            }
            Ok::<_, anyhow::Error>(all)
        }).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => { eprintln!("[search] skip DB: {}", e); continue; }
            Err(e) => { eprintln!("[search] task error: {}", e); continue; }
        };

        results.extend(found);
    }

    results.sort_by_key(|r| std::cmp::Reverse(r["timestamp"].as_i64().unwrap_or(0)));
    let paged: Vec<Value> = results.into_iter().take(limit).collect();
    Ok(json!({ "keyword": keyword, "count": paged.len(), "results": paged }))
}

/// 查询联系人
pub async fn q_contacts(names: &Names, query: Option<&str>, limit: usize) -> Result<Value> {
    let mut contacts: Vec<Value> = names.map.iter()
        .filter(|(u, _)| !u.starts_with("gh_") && !u.starts_with("biz_"))
        .map(|(u, d)| json!({ "username": u, "display": d }))
        .collect();

    if let Some(q) = query {
        let low = q.to_lowercase();
        contacts.retain(|c| {
            c["display"].as_str().map(|s| s.to_lowercase().contains(&low)).unwrap_or(false)
            || c["username"].as_str().map(|s| s.to_lowercase().contains(&low)).unwrap_or(false)
        });
    }

    contacts.sort_by(|a, b| {
        a["display"].as_str().unwrap_or("").cmp(b["display"].as_str().unwrap_or(""))
    });

    let total = contacts.len();
    contacts.truncate(limit);
    Ok(json!({ "contacts": contacts, "total": total }))
}

// ─── 内部辅助函数 ────────────────────────────────────────────────────────────

fn resolve_username(chat_name: &str, names: &Names) -> Option<String> {
    if names.map.contains_key(chat_name)
        || chat_name.contains("@chatroom")
        || chat_name.starts_with("wxid_")
    {
        return Some(chat_name.to_string());
    }
    let low = chat_name.to_lowercase();
    // 精确匹配显示名
    for (uname, display) in &names.map {
        if low == display.to_lowercase() {
            return Some(uname.clone());
        }
    }
    // 模糊匹配
    for (uname, display) in &names.map {
        if display.to_lowercase().contains(&low) {
            return Some(uname.clone());
        }
    }
    None
}

async fn find_msg_tables(
    db: &DbCache,
    names: &Names,
    username: &str,
) -> Result<Vec<(std::path::PathBuf, String)>> {
    let table_name = format!("Msg_{:x}", md5::compute(username.as_bytes()));
    let re = Regex::new(r"^Msg_[0-9a-f]{32}$").unwrap();
    if !re.is_match(&table_name) {
        return Ok(Vec::new());
    }

    let mut results: Vec<(i64, std::path::PathBuf, String)> = Vec::new();
    for rel_key in &names.msg_db_keys {
        let path = match db.get(rel_key).await? {
            Some(p) => p,
            None => continue,
        };
        let tname = table_name.clone();
        let path2 = path.clone();
        let max_ts: Option<i64> = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&path2)?;
            let table_exists: Option<i64> = conn.query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
                [&tname],
                |row| row.get(0),
            ).ok().flatten();
            if table_exists.is_none() {
                return Ok::<_, anyhow::Error>(None);
            }
            let ts: Option<i64> = conn.query_row(
                &format!("SELECT MAX(create_time) FROM [{}]", tname),
                [],
                |row| row.get(0),
            ).ok().flatten();
            Ok(ts)
        }).await??;

        if let Some(ts) = max_ts {
            results.push((ts, path.clone(), table_name.clone()));
        }
    }

    // 按最大时间戳降序排列（最新的优先）
    results.sort_by_key(|(ts, _, _)| std::cmp::Reverse(*ts));
    Ok(results.into_iter().map(|(_, p, t)| (p, t)).collect())
}

fn query_messages(
    db_path: &std::path::Path,
    table: &str,
    chat_username: &str,
    is_group: bool,
    names_map: &HashMap<String, String>,
    since: Option<i64>,
    until: Option<i64>,
    limit: usize,
    offset: usize,
) -> Result<Vec<Value>> {
    let conn = Connection::open(db_path)?;
    let id2u = load_id2u(&conn);

    let mut clauses = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(s) = since {
        clauses.push("create_time >= ?");
        params.push(Box::new(s));
    }
    if let Some(u) = until {
        clauses.push("create_time <= ?");
        params.push(Box::new(u));
    }
    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };

    let sql = format!(
        "SELECT local_id, local_type, create_time, real_sender_id,
                message_content, WCDB_CT_message_content
         FROM [{}] {} ORDER BY create_time DESC LIMIT ? OFFSET ?",
        table, where_clause
    );

    params.push(Box::new(limit as i64));
    params.push(Box::new(offset as i64));

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_ref.as_slice(), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            get_content_bytes(row, 4),
            row.get::<_, i64>(5).unwrap_or(0),
        ))
    })?
    .filter_map(|r| r.ok())
    .collect::<Vec<_>>();

    let mut result = Vec::new();
    for (local_id, local_type, ts, real_sender_id, content_bytes, ct) in rows {
        let content = decompress_message(&content_bytes, ct);
        let sender = sender_label(real_sender_id, &content, is_group, chat_username, &id2u, names_map);
        let text = fmt_content(local_id, local_type, &content, is_group);

        result.push(json!({
            "timestamp": ts,
            "time": fmt_time(ts, "%Y-%m-%d %H:%M"),
            "sender": sender,
            "content": text,
            "type": fmt_type(local_type),
            "local_id": local_id,
        }));
    }
    Ok(result)
}

fn search_in_table(
    conn: &Connection,
    table: &str,
    chat_username: &str,
    is_group: bool,
    names_map: &HashMap<String, String>,
    keyword: &str,
    since: Option<i64>,
    until: Option<i64>,
    limit: usize,
) -> Result<Vec<Value>> {
    let id2u = load_id2u(conn);
    // 转义 LIKE 通配符，使用 '\' 作为 ESCAPE 字符
    let escaped_kw = keyword.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    let mut clauses = vec!["message_content LIKE ? ESCAPE '\\'".to_string()];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(format!("%{}%", escaped_kw))];
    if let Some(s) = since {
        clauses.push("create_time >= ?".into());
        params.push(Box::new(s));
    }
    if let Some(u) = until {
        clauses.push("create_time <= ?".into());
        params.push(Box::new(u));
    }
    let where_clause = format!("WHERE {}", clauses.join(" AND "));
    let sql = format!(
        "SELECT local_id, local_type, create_time, real_sender_id,
                message_content, WCDB_CT_message_content
         FROM [{}] {} ORDER BY create_time DESC LIMIT ?",
        table, where_clause
    );
    params.push(Box::new(limit as i64));

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_ref.as_slice(), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            get_content_bytes(row, 4),
            row.get::<_, i64>(5).unwrap_or(0),
        ))
    })?
    .filter_map(|r| r.ok())
    .collect::<Vec<_>>();

    let mut result = Vec::new();
    for (local_id, local_type, ts, real_sender_id, content_bytes, ct) in rows {
        let content = decompress_message(&content_bytes, ct);
        let sender = sender_label(real_sender_id, &content, is_group, chat_username, &id2u, names_map);
        let text = fmt_content(local_id, local_type, &content, is_group);

        result.push(json!({
            "timestamp": ts,
            "time": fmt_time(ts, "%Y-%m-%d %H:%M"),
            "chat": "",
            "sender": sender,
            "content": text,
            "type": fmt_type(local_type),
        }));
    }
    Ok(result)
}

fn load_id2u(conn: &Connection) -> HashMap<i64, String> {
    let mut map = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT rowid, user_name FROM Name2Id") {
        let _ = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        }).map(|rows| {
            for r in rows.flatten() {
                map.insert(r.0, r.1);
            }
        });
    }
    map
}

fn sender_label(
    real_sender_id: i64,
    content: &str,
    is_group: bool,
    chat_username: &str,
    id2u: &HashMap<i64, String>,
    names: &HashMap<String, String>,
) -> String {
    let sender_uname = id2u.get(&real_sender_id).cloned().unwrap_or_default();
    if is_group {
        if !sender_uname.is_empty() && sender_uname != chat_username {
            return names.get(&sender_uname).cloned().unwrap_or(sender_uname);
        }
        if content.contains(":\n") {
            let raw = content.splitn(2, ":\n").next().unwrap_or("");
            return names.get(raw).cloned().unwrap_or_else(|| raw.to_string());
        }
        return String::new();
    }
    if !sender_uname.is_empty() && sender_uname != chat_username {
        return names.get(&sender_uname).cloned().unwrap_or(sender_uname);
    }
    String::new()
}

/// 读取消息内容列（兼容 TEXT 和 BLOB 两种存储类型）
///
/// SQLite 中 message_content 在未压缩时为 TEXT，zstd 压缩后为 BLOB。
/// rusqlite 的 Vec<u8> FromSql 只接受 BLOB，读 TEXT 会静默返回空。
fn get_content_bytes(row: &rusqlite::Row<'_>, idx: usize) -> Vec<u8> {
    // 先尝试 BLOB，再 fallback 到 TEXT→bytes
    row.get::<_, Vec<u8>>(idx)
        .or_else(|_| row.get::<_, String>(idx).map(|s| s.into_bytes()))
        .unwrap_or_default()
}

fn decompress_message(data: &[u8], ct: i64) -> String {
    if ct == 4 && !data.is_empty() {
        // zstd 压缩
        if let Ok(dec) = zstd::decode_all(data) {
            return String::from_utf8_lossy(&dec).into_owned();
        }
    }
    String::from_utf8_lossy(data).into_owned()
}

fn decompress_or_str(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }
    // 尝试 zstd 解压
    if let Ok(dec) = zstd::decode_all(data) {
        if let Ok(s) = String::from_utf8(dec) {
            return s;
        }
    }
    String::from_utf8_lossy(data).into_owned()
}

fn strip_group_prefix(s: &str) -> String {
    if s.contains(":\n") {
        s.splitn(2, ":\n").nth(1).unwrap_or(s).to_string()
    } else {
        s.to_string()
    }
}

pub fn fmt_type(t: i64) -> String {
    let base = (t as u64 & 0xFFFFFFFF) as i64;
    match base {
        1 => "文本".into(),
        3 => "图片".into(),
        34 => "语音".into(),
        42 => "名片".into(),
        43 => "视频".into(),
        47 => "表情".into(),
        48 => "位置".into(),
        49 => "链接/文件".into(),
        50 => "通话".into(),
        10000 => "系统".into(),
        10002 => "撤回".into(),
        _ => format!("type={}", base),
    }
}

fn fmt_content(local_id: i64, local_type: i64, content: &str, is_group: bool) -> String {
    let base = (local_type as u64 & 0xFFFFFFFF) as i64;
    match base {
        3 => return format!("[图片] local_id={}", local_id),
        34 => return "[语音]".into(),
        43 => return "[视频]".into(),
        47 => return "[表情]".into(),
        50 => return "[通话]".into(),
        10000 => return parse_sysmsg(content).unwrap_or_else(|| "[系统消息]".into()),
        10002 => return parse_revoke(content).unwrap_or_else(|| "[撤回了一条消息]".into()),
        _ => {}
    }

    let text = if is_group && content.contains(":\n") {
        content.splitn(2, ":\n").nth(1).unwrap_or(content)
    } else {
        content
    };

    if base == 49 && text.contains("<appmsg") {
        if let Some(parsed) = parse_appmsg(text) {
            return parsed;
        }
    }
    text.to_string()
}

/// 解析撤回消息 XML，提取被撤回的内容摘要
/// `<sysmsg type="revokemsg"><revokemsg><content>...</content></revokemsg></sysmsg>`
fn parse_revoke(xml: &str) -> Option<String> {
    let inner = extract_xml_text(xml, "content")?;
    // 有时 content 是 "xxx recalled a message" 英文，有时是中文
    if inner.is_empty() {
        return Some("[撤回了一条消息]".into());
    }
    // 尝试简化：如果是 XML 格式的撤回内容，直接显示摘要
    Some(format!("[撤回] {}", inner
        .chars()
        .take(30)
        .collect::<String>()))
}

/// 解析系统消息 XML（群通知等）
fn parse_sysmsg(xml: &str) -> Option<String> {
    // 常见格式：<sysmsg type="...">...</sysmsg>
    // 尝试提取 content 标签
    if let Some(s) = extract_xml_text(xml, "content") {
        if !s.is_empty() {
            return Some(format!("[系统] {}", s.chars().take(50).collect::<String>()));
        }
    }
    // 纯文本系统消息（无 XML）
    if !xml.starts_with('<') {
        return Some(format!("[系统] {}", xml.chars().take(50).collect::<String>()));
    }
    Some("[系统消息]".into())
}

fn parse_appmsg(text: &str) -> Option<String> {
    // 简单 XML 解析，避免引入重量级 XML 库（或直接用 minidom）
    // 这里用基本字符串搜索实现
    let title = extract_xml_text(text, "title")?;
    let atype = extract_xml_text(text, "type").unwrap_or_default();
    match atype.as_str() {
        "6" => Some(if !title.is_empty() { format!("[文件] {}", title) } else { "[文件]".into() }),
        "57" => {
            let ref_content = extract_xml_text(text, "content")
                .map(|s| {
                    // content 可能是 HTML 转义的 XML（被引用的消息是 appmsg 时）
                    let unescaped = unescape_html(&s);
                    // 如果解转义后是 XML，尝试递归解析
                    if unescaped.contains("<appmsg") {
                        if let Some(parsed) = parse_appmsg(&unescaped) {
                            return parsed;
                        }
                    }
                    let s: String = unescaped.split_whitespace().collect::<Vec<_>>().join(" ");
                    if s.chars().count() > 40 {
                        format!("{}...", s.chars().take(40).collect::<String>())
                    } else { s }
                })
                .unwrap_or_default();
            let quote = if !title.is_empty() { format!("[引用] {}", title) } else { "[引用]".into() };
            if !ref_content.is_empty() {
                Some(format!("{}\n  \u{21b3} {}", quote, ref_content))
            } else {
                Some(quote)
            }
        }
        "33" | "36" | "44" => Some(if !title.is_empty() { format!("[小程序] {}", title) } else { "[小程序]".into() }),
        _ => Some(if !title.is_empty() { format!("[链接] {}", title) } else { "[链接/文件]".into() }),
    }
}

fn extract_xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)?;
    let content_start = start + open.len();
    let end = xml[content_start..].find(&close)?;
    Some(xml[content_start..content_start + end].trim().to_string())
}

fn unescape_html(s: &str) -> String {
    s.replace("&lt;", "<")
     .replace("&gt;", ">")
     .replace("&amp;", "&")
     .replace("&quot;", "\"")
     .replace("&apos;", "'")
}

fn fmt_time(ts: i64, fmt: &str) -> String {
    Local.timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.format(fmt).to_string())
        .unwrap_or_else(|| ts.to_string())
}

