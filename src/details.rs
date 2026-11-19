//! The Details tab's every field: a server's records (a VM's, a server's, the pool's, and their
//! metrics) made into rows to read and search, and those rows saved as CSV or PDF.

use crate::model::{fmt_bytes, parse_xen_time};
use serde_json::Value;
use std::io::Write;

/// A card of the tab: its title, then (field, value) rows.
pub type Section = (String, Vec<(String, String)>);

// ---------------------------------------------------------------------------------------------
// Records into rows

/// A record's fields as rows, by name (case aside): a map's entries each a row of their own
/// ("platform › acpi"), a list on one line; a reference to something known by its name.
pub fn flatten(rec: &Value, names: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let mut rows = vec![];
    let Some(obj) = rec.as_object() else { return rows };
    let mut keys: Vec<&String> = obj.keys().collect();
    keys.sort_by_key(|k| k.to_lowercase());
    for k in keys {
        match &obj[k] {
            Value::Object(m) if !m.is_empty() => {
                let mut sub: Vec<&String> = m.keys().collect();
                sub.sort_by_key(|k| k.to_lowercase());
                for s in sub {
                    rows.push((format!("{} › {}", k, s), text(s, &m[s], names)));
                }
            }
            v => rows.push((k.clone(), text(k, v, names))),
        }
    }
    rows
}

/// A field's value as text (`key` says what it is: memory in bytes, a password).
fn text(key: &str, v: &Value, names: &dyn Fn(&str) -> Option<String>) -> String {
    match v {
        Value::Null => "—".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => with_size(key, &n.to_string()),
        Value::String(s) => scalar(key, s, names),
        Value::Array(a) if a.is_empty() => "—".into(),
        Value::Array(a) => a
            .iter()
            .map(|x| match x {
                Value::String(s) => scalar(key, s, names),
                o => text(key, o, names),
            })
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(m) if m.is_empty() => "—".into(),
        Value::Object(m) => serde_json::to_string(m).unwrap_or_default(),
    }
}

fn scalar(key: &str, s: &str, names: &dyn Fn(&str) -> Option<String>) -> String {
    let k = key.to_lowercase();
    if s.is_empty() {
        return "—".into();
    }
    if (k.contains("password") || k.contains("secret")) && !s.starts_with("OpaqueRef:") {
        return "••••••".into();
    }
    if s == "OpaqueRef:NULL" {
        return "none".into();
    }
    if let Some(n) = names(s) {
        return n;
    }
    // the API's times ("20261119T10:22:33Z"); its zero is "never"
    let b = s.as_bytes();
    if s.len() >= 17 && b[8] == b'T' && s.ends_with('Z') && b[..8].iter().all(u8::is_ascii_digit) {
        if s.starts_with("19700101T") {
            return "never".into();
        }
        if let Some(t) = parse_xen_time(s) {
            return if t <= 0 { "never".into() } else { format!("{}-{}-{} {} UTC", &s[0..4], &s[4..6], &s[6..8], &s[9..17]) };
        }
    }
    with_size(key, s)
}

/// A number of bytes (memory, a size) with how much that is: "4294967296  (4 GB)".
fn with_size(key: &str, s: &str) -> String {
    let k = key.to_lowercase();
    let bytes = (k.contains("memory") || k.contains("size") || k.contains("utilisation") || k.contains("overhead")) && !k.contains("ratio");
    match s.parse::<i64>() {
        Ok(n) if bytes && n >= 1 << 20 => format!("{}  ({})", s, fmt_bytes(n)),
        _ => s.to_string(),
    }
}

/// Whether `q` (lower case) is in `text`, case aside.
pub fn found(text: &str, q: &str) -> bool {
    q.is_empty() || text.to_lowercase().contains(q)
}

/// The rows with `q` (lower case) in their field or value; a section whose title has it keeps
/// them all; sections left with none go.
pub fn search(secs: &[Section], q: &str) -> Vec<Section> {
    secs.iter()
        .filter_map(|(title, rows)| {
            let rows: Vec<(String, String)> = if found(title, q) { rows.clone() } else { rows.iter().filter(|(k, v)| found(k, q) || found(v, q)).cloned().collect() };
            (!rows.is_empty()).then(|| (title.clone(), rows))
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// CSV

/// Section, Field, Value; UTF-8 with a BOM (so spreadsheets read accents right), CRLF lines.
pub fn csv(secs: &[Section]) -> String {
    let cell = |s: &str| {
        // (a value a spreadsheet would run as a formula is kept as text)
        let s = if s.starts_with(['=', '+', '@']) { format!("'{}", s) } else { s.to_string() };
        if s.contains([',', '"', '\n', '\r']) || s.starts_with(' ') || s.ends_with(' ') {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s
        }
    };
    let mut out = String::from("\u{feff}Section,Field,Value\r\n");
    for (title, rows) in secs {
        for (k, v) in rows {
            out.push_str(&format!("{},{},{}\r\n", cell(title), cell(k), cell(v)));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// PDF: A4 pages of Helvetica (built into every PDF reader, so nothing is embedded); each section
// a heading over its rows, field on the left and value on the right, both wrapped.

const PAGE_W: f32 = 595.28;
const PAGE_H: f32 = 841.89;
const MARGIN: f32 = 48.0;
const SIZE: f32 = 8.5;
const LEAD: f32 = 11.0;
const KEY_W: f32 = 180.0;
const GAP: f32 = 12.0;

/// Helvetica's widths (thousandths of the size) for ' ' to '~', then Helvetica-Bold's.
const WIDTHS: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722,
    667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556, 556, 278, 556, 556, 222,
    222, 500, 222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];
const BOLD_WIDTHS: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 333, 333, 584, 584, 584, 611, 975, 722, 722, 722, 722,
    667, 611, 778, 722, 278, 556, 722, 611, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 333, 278, 333, 584, 556, 333, 556, 611, 556, 611, 556, 333, 611, 611, 278,
    278, 556, 278, 889, 611, 611, 611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584,
];

/// A character in the PDF's WinAnsi encoding ('?' for one it hasn't).
fn win(c: char) -> u8 {
    match c as u32 {
        0x20..=0x7e | 0xa0..=0xff => c as u32 as u8,
        _ => match c {
            '€' => 0x80,
            '…' => 0x85,
            '‹' => 0x8b,
            '‘' => 0x91,
            '’' => 0x92,
            '“' => 0x93,
            '”' => 0x94,
            '•' => 0x95,
            '–' => 0x96,
            '—' => 0x97,
            '™' => 0x99,
            '›' => 0x9b,
            '\t' | '\n' | '\r' => b' ',
            _ => b'?',
        },
    }
}

fn width(s: &str, bold: bool, size: f32) -> f32 {
    let table = if bold { &BOLD_WIDTHS } else { &WIDTHS };
    s.chars().map(|c| win(c)).map(|b| if (0x20..=0x7e).contains(&b) { table[(b - 0x20) as usize] } else { 556 } as f32).sum::<f32>() * size / 1000.0
}

/// `s` in lines no wider than `w`: at spaces, and inside a word too long for a line.
fn wrap(s: &str, bold: bool, size: f32, w: f32) -> Vec<String> {
    let mut lines = vec![];
    for para in s.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{} {}", line, word) };
            if width(&candidate, bold, size) <= w {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            // the word alone, cut where it must be
            for c in word.chars() {
                if width(&format!("{}{}", line, c), bold, size) > w && !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                line.push(c);
            }
        }
        lines.push(line);
    }
    lines
}

/// A PDF string literal: ( ) and \ escaped, anything outside ASCII as an octal escape.
fn pdf_str(s: &str) -> String {
    let mut out = String::from("(");
    for b in s.chars().map(win) {
        match b {
            b'(' | b')' | b'\\' => {
                out.push('\\');
                out.push(b as char);
            }
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\{:03o}", b)),
        }
    }
    out.push(')');
    out
}

/// Text at (x, y) on a page's contents, in a grey (0 black, 1 white).
fn put_text(page: &mut String, x: f32, y: f32, s: &str, bold: bool, size: f32, grey: f32) {
    let f = if bold { "F2" } else { "F1" };
    page.push_str(&format!("{g:.2} {g:.2} {g:.2} rg BT /{f} {size} Tf {x:.2} {y:.2} Td {s} Tj ET\n", g = grey, f = f, size = size, x = x, y = y, s = pdf_str(s)));
}

/// Text for the document's properties (its title): as is when plain ASCII, else UTF-16 (the
/// pages' WinAnsi isn't what properties are read in).
fn pdf_info(s: &str) -> String {
    if s.chars().all(|c| (' '..='~').contains(&c)) {
        return pdf_str(s);
    }
    let hex: String = s.encode_utf16().map(|u| format!("{:04X}", u)).collect();
    format!("<FEFF{}>", hex)
}

struct Pages {
    pages: Vec<String>,
    y: f32,
}

impl Pages {
    fn page(&mut self) -> &mut String {
        self.pages.last_mut().unwrap()
    }
    fn new_page(&mut self) {
        self.pages.push(String::new());
        self.y = PAGE_H - MARGIN;
    }
    /// Room for `h` more on this page, else a new one.
    fn room(&mut self, h: f32) {
        if self.y - h < MARGIN + 18.0 {
            self.new_page();
        }
    }
    fn text(&mut self, x: f32, y: f32, s: &str, bold: bool, size: f32, grey: f32) {
        put_text(self.page(), x, y, s, bold, size, grey);
    }
    fn rule(&mut self, y: f32, grey: f32, w: f32) {
        let line = format!("{g:.2} {g:.2} {g:.2} RG {w} w {x0:.2} {y:.2} m {x1:.2} {y:.2} l S\n", g = grey, w = w, x0 = MARGIN, x1 = PAGE_W - MARGIN, y = y);
        self.page().push_str(&line);
    }
}

/// The sections as a PDF: `title` on top with `sub` under it (what, when), page numbers below.
pub fn pdf(title: &str, sub: &str, producer: &str, secs: &[Section]) -> Vec<u8> {
    let mut p = Pages { pages: vec![], y: 0.0 };
    p.new_page();
    let full = PAGE_W - 2.0 * MARGIN;
    for line in wrap(title, true, 16.0, full) {
        p.y -= 18.0;
        p.text(MARGIN, p.y, &line, true, 16.0, 0.1);
    }
    p.y -= 14.0;
    p.text(MARGIN, p.y, sub, false, 9.0, 0.45);
    p.y -= 6.0;
    let value_x = MARGIN + KEY_W + GAP;
    let value_w = PAGE_W - MARGIN - value_x;
    for (name, rows) in secs {
        // a heading with at least its first row under it
        p.room(22.0 + 16.0 + LEAD);
        p.y -= 22.0;
        p.text(MARGIN, p.y, name, true, 11.0, 0.1);
        p.y -= 6.0;
        p.rule(p.y, 0.55, 0.8);
        p.y -= 4.0;
        for (k, v) in rows {
            let kl = wrap(k, false, SIZE, KEY_W);
            let vl = wrap(v, false, SIZE, value_w);
            let h = kl.len().max(vl.len()) as f32 * LEAD + 5.0;
            p.room(h);
            let top = p.y - LEAD + 2.0;
            for (i, l) in kl.iter().enumerate() {
                p.text(MARGIN, top - i as f32 * LEAD, l, false, SIZE, 0.42);
            }
            for (i, l) in vl.iter().enumerate() {
                p.text(value_x, top - i as f32 * LEAD, l, false, SIZE, 0.08);
            }
            p.y -= h;
            p.rule(p.y + 1.5, 0.88, 0.4);
        }
    }
    // each page's foot: what, and its number
    let n = p.pages.len();
    let foot = format!("{}  ·  {}", producer, title);
    for (i, page) in p.pages.iter_mut().enumerate() {
        let num = format!("Page {} of {}", i + 1, n);
        put_text(page, MARGIN, MARGIN - 16.0, &foot, false, 7.5, 0.55);
        put_text(page, PAGE_W - MARGIN - width(&num, false, 7.5), MARGIN - 16.0, &num, false, 7.5, 0.55);
    }
    // the file: catalog, page tree, the two fonts, info, then each page and its contents
    let mut out: Vec<u8> = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets: Vec<usize> = vec![];
    let obj = |out: &mut Vec<u8>, offsets: &mut Vec<usize>, body: &[u8]| {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", offsets.len()).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    };
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 6 + 2 * i)).collect();
    obj(&mut out, &mut offsets, b"<< /Type /Catalog /Pages 2 0 R >>");
    obj(&mut out, &mut offsets, format!("<< /Type /Pages /Kids [{}] /Count {} >>", kids.join(" "), n).as_bytes());
    obj(&mut out, &mut offsets, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>");
    obj(&mut out, &mut offsets, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>");
    obj(&mut out, &mut offsets, format!("<< /Title {} /Producer {} >>", pdf_info(title), pdf_info(producer)).as_bytes());
    for (i, page) in p.pages.iter().enumerate() {
        let page_obj = format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {} 0 R >>", PAGE_W, PAGE_H, 7 + 2 * i);
        obj(&mut out, &mut offsets, page_obj.as_bytes());
        let mut z = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::default());
        let _ = z.write_all(page.as_bytes());
        let data = z.finish().unwrap_or_default();
        let mut body = format!("<< /Length {} /Filter /FlateDecode >>\nstream\n", data.len()).into_bytes();
        body.extend_from_slice(&data);
        body.extend_from_slice(b"\nendstream");
        obj(&mut out, &mut offsets, &body);
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
    for o in &offsets {
        out.extend_from_slice(format!("{:010} 00000 n \n", o).as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R /Info 5 0 R >>\nstartxref\n{}\n%%EOF\n", offsets.len() + 1, xref).as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_made_rows() {
        let names = |r: &str| (r == "OpaqueRef:h1").then(|| "host-01".to_string());
        let rec = json!({
            "name_label": "Web", "resident_on": "OpaqueRef:h1", "affinity": "OpaqueRef:NULL", "VBDs": ["OpaqueRef:b1", "OpaqueRef:b2"],
            "platform": { "acpi": "1", "Viridian": "false" }, "blobs": {}, "memory_static_max": "4294967296", "memory-ratio": "0.25",
            "start_time": "20261119T10:22:33Z", "install_time": "19700101T00:00:00Z", "is_a_template": false, "tags": [], "description": "",
            "repository_proxy_password": "hunter2", "domid": -1,
        });
        let rows = flatten(&rec, &names);
        let get = |k: &str| rows.iter().find(|r| r.0 == k).map(|r| r.1.as_str()).unwrap_or("(missing)");
        assert_eq!(get("resident_on"), "host-01");
        assert_eq!(get("affinity"), "none");
        assert_eq!(get("VBDs"), "OpaqueRef:b1, OpaqueRef:b2");
        assert_eq!(get("platform › acpi"), "1");
        assert_eq!(get("blobs"), "—");
        assert_eq!(get("memory_static_max"), "4294967296  (4 GB)");
        assert_eq!(get("memory-ratio"), "0.25");
        assert_eq!(get("start_time"), "2026-11-19 10:22:33 UTC");
        assert_eq!(get("install_time"), "never");
        assert_eq!(get("is_a_template"), "false");
        assert_eq!(get("tags"), "—");
        assert_eq!(get("description"), "—");
        assert_eq!(get("repository_proxy_password"), "••••••");
        assert_eq!(get("domid"), "-1");
        // by name, case aside; a map's entries in order too
        let keys: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
        assert!(keys.iter().position(|k| *k == "affinity") < keys.iter().position(|k| *k == "blobs"));
        assert!(keys.iter().position(|k| *k == "platform › acpi") < keys.iter().position(|k| *k == "platform › Viridian"));
    }

    #[test]
    fn searched() {
        let secs = vec![
            ("General".to_string(), vec![("Name".to_string(), "Web".to_string()), ("Memory".to_string(), "4 GB".to_string())]),
            ("All fields · VM metrics".to_string(), vec![("hvm".to_string(), "true".to_string())]),
        ];
        let r = search(&secs, "memory");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].1, vec![("Memory".to_string(), "4 GB".to_string())]);
        // a section's title found: all of it
        assert_eq!(search(&secs, "metrics")[0].1.len(), 1);
        assert_eq!(search(&secs, "web")[0].1.len(), 1);
        assert_eq!(search(&secs, "").len(), 2);
        assert!(search(&secs, "nothing like it").is_empty());
    }

    #[test]
    fn csv_quoted() {
        let secs = vec![("All fields · VM".to_string(), vec![("name_label".to_string(), "Web, \"main\"".to_string()), ("x".to_string(), "=1+1".to_string())])];
        assert_eq!(csv(&secs), "\u{feff}Section,Field,Value\r\nAll fields · VM,name_label,\"Web, \"\"main\"\"\"\r\nAll fields · VM,x,'=1+1\r\n");
    }

    #[test]
    fn wrapped_to_width() {
        let lines = wrap("the quick brown fox jumps over the lazy dog", false, 10.0, 60.0);
        assert!(lines.len() > 1 && lines.iter().all(|l| width(l, false, 10.0) <= 60.0));
        // a word longer than a line is cut
        let long = wrap(&"x".repeat(80), false, 10.0, 60.0);
        assert!(long.len() > 1 && long.concat() == "x".repeat(80));
        assert_eq!(pdf_str("a(b)\\ é·"), "(a\\(b\\)\\\\ \\351\\267)");
        assert_eq!(pdf_info("Web"), "(Web)");
        assert_eq!(pdf_info("Web — é"), "<FEFF00570065006200202014002000E9>");
    }

    #[test]
    fn pdf_well_formed() {
        let rows: Vec<(String, String)> = (0..300).map(|i| (format!("field_{}", i), format!("value {} ", i).repeat(i % 7 + 1))).collect();
        let b = pdf("Web Server", "VM details · 19 Nov 2026", "Simple VM Manager 1.0", &[("All fields · VM".to_string(), rows)]);
        assert!(b.starts_with(b"%PDF-1.4") && b.ends_with(b"%%EOF\n"));
        let s = String::from_utf8_lossy(&b);
        // several pages, and each object where the cross-reference says
        let pages: usize = s.split("/Count ").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
        assert!(pages > 3);
        let xref: usize = s.rsplit("startxref\n").next().unwrap().lines().next().unwrap().parse().unwrap();
        let table = String::from_utf8_lossy(&b[xref..]).to_string();
        for (i, line) in table.lines().skip(3).take(5 + 2 * pages).enumerate() {
            let off: usize = line[..10].parse().unwrap();
            assert!(b[off..].starts_with(format!("{} 0 obj", i + 1).as_bytes()), "object {}", i + 1);
        }
    }
}
