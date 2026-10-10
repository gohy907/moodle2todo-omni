//! Fetches Moodle's .ics feed and creates/updates the corresponding tasks
//! (VTODO) in a CalDAV calendar (Nextcloud, anytype-caldav, ...). Completion
//! status set in the client (Calino, etc.) is not overwritten: only the
//! SYNC_KEYS fields are synced.

use std::{env, fs, thread, time::Duration};

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use reqwest::StatusCode;

/// Fields taken from Moodle. Everything else in the task (STATUS, COMPLETED,
/// PERCENT-COMPLETE, ...) is left untouched.
const SYNC_KEYS: [&str; 5] = ["SUMMARY", "DESCRIPTION", "DUE", "CATEGORIES", "URL"];

/// How a task's resource name (`<name>.ics`) is derived from the event UID.
#[derive(Clone, Copy)]
enum Naming {
    /// Every character outside `[A-Za-z0-9-_.]` becomes `_`.
    Sanitized,
    /// Calino's `eventResourceFilename`: every other byte becomes `~XX`.
    /// Required by servers that insist on it, e.g. anytype-caldav.
    Calino,
}

struct Config {
    moodle_url: String,
    calendar_url: String,
    user: String,
    pass: String,
    naming: Naming,
    /// Events with any of these CATEGORIES (Moodle puts the course name there) are skipped.
    exclude: Vec<String>,
}

enum Outcome {
    Created,
    Updated,
    Unchanged,
}

fn env_req(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("environment variable {name} is not set"))
}

/// Reads `name`, falling back to the legacy (Nextcloud-era) `legacy` name.
fn env_any(name: &str, legacy: &str) -> Option<String> {
    env::var(name).or_else(|_| env::var(legacy)).ok()
}

fn env_any_req(name: &str, legacy: &str) -> Result<String> {
    env_any(name, legacy).with_context(|| format!("environment variable {name} is not set"))
}

fn env_true(name: &str, legacy: &str) -> bool {
    matches!(
        env_any(name, legacy).map(|v| v.to_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

fn naming_from_env() -> Result<Naming> {
    match env::var("CALDAV_RESOURCE_NAMING").as_deref() {
        Err(_) | Ok("sanitized") => Ok(Naming::Sanitized),
        Ok("calino") => Ok(Naming::Calino),
        Ok(other) => bail!("CALDAV_RESOURCE_NAMING must be `sanitized` or `calino`, got `{other}`"),
    }
}

// ---------- minimal iCalendar handling ----------

/// Unfolds "folded" lines (a continuation starts with a space/tab).
fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            out.last_mut().unwrap().push_str(&line[1..]);
        } else if !line.is_empty() {
            out.push(line.to_string());
        }
    }
    out
}

/// Folds a line at 75 octets without splitting UTF-8 characters.
fn fold(line: &str) -> String {
    let mut out = String::new();
    let mut len = 0usize;
    for ch in line.chars() {
        let n = ch.len_utf8();
        if len + n > 75 {
            out.push_str("\r\n ");
            len = 1;
        }
        out.push(ch);
        len += n;
    }
    out
}

fn serialize(lines: &[String]) -> String {
    let mut s = String::new();
    for l in lines {
        s.push_str(&fold(l));
        s.push_str("\r\n");
    }
    s
}

/// Property name in uppercase: everything before the first ':' or ';'.
fn prop_name(line: &str) -> String {
    line.split(|c: char| c == ':' || c == ';')
        .next()
        .unwrap_or("")
        .to_uppercase()
}

/// Returns the properties of each VEVENT (nested components, e.g. VALARM, are skipped).
fn parse_events(text: &str) -> Vec<Vec<String>> {
    let mut events = Vec::new();
    let mut cur: Option<Vec<String>> = None;
    let mut nested: i32 = 0;
    for line in unfold(text) {
        let up = line.to_uppercase();
        if cur.is_none() {
            if up == "BEGIN:VEVENT" {
                cur = Some(Vec::new());
                nested = 0;
            }
            continue;
        }
        if up == "END:VEVENT" && nested == 0 {
            events.push(cur.take().unwrap());
        } else if up.starts_with("BEGIN:") {
            nested += 1;
        } else if up.starts_with("END:") {
            nested -= 1;
        } else if nested == 0 {
            cur.as_mut().unwrap().push(line);
        }
    }
    events
}

/// Values of all CATEGORIES properties of an event (comma-separated, `\,` is a literal comma).
fn categories(ev: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for l in ev.iter().filter(|l| prop_name(l) == "CATEGORIES") {
        let Some((_, v)) = l.split_once(':') else { continue };
        let mut cur = String::new();
        let mut chars = v.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => cur.extend(chars.next()),
                ',' => out.push(std::mem::take(&mut cur).trim().to_string()),
                _ => cur.push(c),
            }
        }
        out.push(cur.trim().to_string());
    }
    out
}

fn is_excluded(ev: &[String], exclude: &[String]) -> bool {
    categories(ev).iter().any(|c| exclude.contains(c))
}

/// Builds the UID and the set of property lines for VTODO from a VEVENT.
/// DUE is taken from DTEND (or DTSTART if DTEND is absent).
fn todo_props(ev: &[String]) -> Result<(String, Vec<String>)> {
    let uid = ev
        .iter()
        .find(|l| prop_name(l) == "UID")
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .context("event has no UID")?;

    let mut props: Vec<String> = ev
        .iter()
        .filter(|l| {
            matches!(
                prop_name(l).as_str(),
                "SUMMARY" | "DESCRIPTION" | "CATEGORIES" | "URL"
            )
        })
        .cloned()
        .collect();

    let due = ev
        .iter()
        .find(|l| prop_name(l) == "DTEND")
        .or_else(|| ev.iter().find(|l| prop_name(l) == "DTSTART"));
    if let Some(l) = due {
        let pos = l.find(|c: char| c == ':' || c == ';').unwrap_or(l.len());
        props.push(format!("DUE{}", &l[pos..]));
    }
    Ok((uid, props))
}

fn new_todo(uid: &str, props: &[String]) -> String {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let mut lines: Vec<String> = vec![
        "BEGIN:VCALENDAR".into(),
        "VERSION:2.0".into(),
        "PRODID:-//moodle2todo//EN".into(),
        "BEGIN:VTODO".into(),
        format!("UID:{uid}"),
        format!("DTSTAMP:{stamp}"),
    ];
    lines.extend(props.iter().cloned());
    lines.push("END:VTODO".into());
    lines.push("END:VCALENDAR".into());
    serialize(&lines)
}

/// Indices of SYNC_KEYS lines located directly in VTODO (not in a nested VALARM).
fn sync_indices(lines: &[String]) -> Vec<usize> {
    let mut idx = Vec::new();
    let mut in_todo = false;
    let mut nested: i32 = 0;
    for (i, l) in lines.iter().enumerate() {
        let up = l.to_uppercase();
        if !in_todo {
            if up == "BEGIN:VTODO" {
                in_todo = true;
                nested = 0;
            }
            continue;
        }
        if up == "END:VTODO" && nested == 0 {
            break;
        }
        if up.starts_with("BEGIN:") {
            nested += 1;
        } else if up.starts_with("END:") {
            nested -= 1;
        } else if nested == 0 && SYNC_KEYS.contains(&prop_name(l).as_str()) {
            idx.push(i);
        }
    }
    idx
}

/// Substitutes fresh SYNC fields into an existing task.
/// Returns None if there is nothing to change.
fn merge(body: &str, props: &[String]) -> Option<String> {
    let lines = unfold(body);
    let idx = sync_indices(&lines);

    let mut old: Vec<&String> = idx.iter().map(|&i| &lines[i]).collect();
    let mut new: Vec<&String> = props.iter().collect();
    old.sort();
    new.sort();
    if old == new {
        return None;
    }

    let mut out: Vec<String> = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| !idx.contains(i))
        .map(|(_, l)| l.clone())
        .collect();
    let end = out
        .iter()
        .position(|l| l.eq_ignore_ascii_case("END:VTODO"))?;
    for (k, p) in props.iter().enumerate() {
        out.insert(end + k, p.clone());
    }
    Some(serialize(&out))
}

// ---------- CalDAV ----------

fn resource_name(uid: &str, naming: Naming) -> String {
    let safe = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.');
    let mut s = String::new();
    for b in uid.bytes() {
        match (naming, safe(b)) {
            (_, true) => s.push(b as char),
            (Naming::Sanitized, false) => s.push('_'),
            (Naming::Calino, false) => s.push_str(&format!("~{b:02X}")),
        }
    }
    format!("{s}.ics")
}

fn put(
    caldav: &Client,
    cfg: &Config,
    url: &str,
    body: String,
    cond: (&str, &str),
) -> Result<()> {
    let r = caldav
        .put(url)
        .basic_auth(&cfg.user, Some(&cfg.pass))
        .header("Content-Type", "text/calendar; charset=utf-8")
        .header(cond.0, cond.1)
        .body(body)
        .send()?;
    if !r.status().is_success() {
        bail!("PUT {url} -> {}", r.status());
    }
    Ok(())
}

fn process(caldav: &Client, cfg: &Config, ev: &[String]) -> Result<Outcome> {
    let (uid, props) = todo_props(ev)?;
    let url = format!(
        "{}/{}",
        cfg.calendar_url.trim_end_matches('/'),
        resource_name(&uid, cfg.naming)
    );

    let resp = caldav.get(&url).basic_auth(&cfg.user, Some(&cfg.pass)).send()?;
    match resp.status() {
        StatusCode::NOT_FOUND => {
            put(caldav, cfg, &url, new_todo(&uid, &props), ("If-None-Match", "*"))?;
            Ok(Outcome::Created)
        }
        s if s.is_success() => {
            let etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let body = resp.text()?;
            match merge(&body, &props) {
                None => Ok(Outcome::Unchanged),
                Some(new_body) => {
                    let (h, v) = match &etag {
                        Some(e) => ("If-Match", e.as_str()),
                        None => ("If-Match", "*"),
                    };
                    put(caldav, cfg, &url, new_body, (h, v))?;
                    Ok(Outcome::Updated)
                }
            }
        }
        s => bail!("GET {url} -> {s}"),
    }
}

fn sync_once(moodle: &Client, caldav: &Client, cfg: &Config) -> Result<()> {
    let ics = moodle
        .get(&cfg.moodle_url)
        .send()?
        .error_for_status()?
        .text()?;
    let events = parse_events(&ics);

    let (mut created, mut updated, mut unchanged, mut skipped, mut errors) = (0, 0, 0, 0, 0);
    for ev in &events {
        if is_excluded(ev, &cfg.exclude) {
            skipped += 1;
            continue;
        }
        match process(caldav, cfg, ev) {
            Ok(Outcome::Created) => created += 1,
            Ok(Outcome::Updated) => updated += 1,
            Ok(Outcome::Unchanged) => unchanged += 1,
            Err(e) => {
                errors += 1;
                eprintln!("error: {e:#}");
            }
        }
    }
    println!(
        "total={} created={created} updated={updated} unchanged={unchanged} skipped={skipped} errors={errors}",
        events.len()
    );
    Ok(())
}

fn main() -> Result<()> {
    let cfg = Config {
        moodle_url: env_req("MOODLE_ICS_URL")?,
        calendar_url: env_req("CALENDAR_URL")?,
        user: env_any_req("CALDAV_USER", "NC_USER")?,
        pass: env_any_req("CALDAV_PASSWORD", "NC_APP_PASSWORD")?,
        naming: naming_from_env()?,
        exclude: env::var("EXCLUDE_CATEGORIES")
            .unwrap_or_default()
            .split(';')
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect(),
    };

    let moodle = Client::builder().timeout(Duration::from_secs(30)).build()?;

    let mut b = Client::builder().timeout(Duration::from_secs(30));
    if env_true("CALDAV_INSECURE", "NC_INSECURE") {
        b = b.danger_accept_invalid_certs(true);
    }
    if let Some(path) = env_any("CALDAV_CA_FILE", "NC_CA_FILE") {
        let pem = fs::read(&path).with_context(|| format!("failed to read {path}"))?;
        b = b.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
    }
    let caldav = b.build()?;

    let interval = env::var("SYNC_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());

    let Some(secs) = interval else {
        return sync_once(&moodle, &caldav, &cfg);
    };
    loop {
        if let Err(e) = sync_once(&moodle, &caldav, &cfg) {
            eprintln!("sync error: {e:#}");
        }
        thread::sleep(Duration::from_secs(secs));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitized_replaces_unsafe_chars() {
        assert_eq!(
            resource_name("123@moodle.example.com", Naming::Sanitized),
            "123_moodle.example.com.ics"
        );
    }

    /// Must match anytype-caldav's `calino_filename`.
    #[test]
    fn calino_encodes_unsafe_bytes_as_tilde_hex() {
        assert_eq!(
            resource_name("123@moodle.example.com", Naming::Calino),
            "123~40moodle.example.com.ics"
        );
        assert_eq!(resource_name("а b", Naming::Calino), "~D0~B0~20b.ics");
    }

    #[test]
    fn excludes_by_course_category() {
        let ev = vec![
            "UID:1@x".to_string(),
            "CATEGORIES:Бэкэнд ПИ_ФИИТ_КБ".to_string(),
        ];
        assert!(is_excluded(&ev, &["Бэкэнд ПИ_ФИИТ_КБ".to_string()]));
        assert!(!is_excluded(&ev, &["ООП_КНиИТ".to_string()]));
        assert!(!is_excluded(&ev, &[]));
    }

    #[test]
    fn categories_split_on_unescaped_commas() {
        let ev = vec![r"CATEGORIES:Дом\, дача,Работа".to_string()];
        assert_eq!(categories(&ev), vec!["Дом, дача", "Работа"]);
    }
}
