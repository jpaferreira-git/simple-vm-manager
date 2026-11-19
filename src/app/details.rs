//! The Details tab of a VM, a server or the pool: its main facts first, then every field the
//! server keeps on it and on its metrics (read when the tab is shown, and again each refresh while
//! it is); a search over all of them, and Export, as CSV or PDF.

use super::*;
use crate::details::{self as dt, Section};
use serde_json::Value;

/// Every field of one item, as last read from its server.
#[derive(Default)]
pub(crate) struct FieldRecords {
    pub loading: bool,
    pub at: Option<Instant>,
    /// (what: "VM", "VM metrics"…, the record)
    pub recs: Vec<(String, Value)>,
    pub error: Option<String>,
}

/// Names for the references a pool's records hold: its servers, VMs, snapshots, storage,
/// networks, disks and drives, network cards, schedules and templates.
fn ref_names(d: &PoolData) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for h in &d.hosts {
        m.insert(h.rref.clone(), h.name.clone());
        if !h.control_domain.is_empty() {
            m.insert(h.control_domain.clone(), format!("Control domain of {}", h.name));
        }
    }
    for v in d.vms.iter().chain(&d.user_templates) {
        m.insert(v.rref.clone(), v.name.clone());
        for x in &v.snapshots {
            m.insert(x.rref.clone(), x.name.clone());
        }
        for k in &v.disks {
            let (drive, disc) = if k.kind == "Disk" { (format!("Disk {} ({})", k.device, or(&k.name, "no name")), or(&k.name, "Disk").to_string()) } else { (format!("DVD drive {}", k.device), k.name.clone()) };
            if !k.vbd.is_empty() {
                m.insert(k.vbd.clone(), drive);
            }
            if !k.vdi.is_empty() && !disc.is_empty() {
                m.insert(k.vdi.clone(), disc);
            }
        }
        for n in &v.nics {
            if !n.vif.is_empty() {
                m.insert(n.vif.clone(), format!("Network card {} ({})", n.device, or(&n.network, "no network")));
            }
        }
    }
    for x in &d.srs {
        m.insert(x.rref.clone(), x.name.clone());
    }
    for n in &d.networks {
        m.insert(n.rref.clone(), n.name.clone());
    }
    for x in &d.schedules {
        m.insert(x.rref.clone(), x.name.clone());
    }
    for t in &d.templates {
        m.insert(t.rref.clone(), t.name.clone());
    }
    m
}

/// `text` with every `q` (lower case) in it marked, on one line cut short at `max_width`.
pub(super) fn marked(text: &str, q: &str, font: FontId, color: Color32, mark: Color32, max_width: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let plain = egui::TextFormat::simple(font, color);
    let lower = text.to_lowercase();
    let mut at = 0;
    // (only where lower case keeps every letter's place)
    if !q.is_empty() && lower.len() == text.len() {
        for (i, _) in lower.match_indices(q) {
            let end = i + q.len();
            if i < at || !text.is_char_boundary(i) || !text.is_char_boundary(end) {
                continue;
            }
            job.append(&text[at..i], 0.0, plain.clone());
            job.append(&text[i..end], 0.0, egui::TextFormat { background: mark, ..plain.clone() });
            at = end;
        }
    }
    job.append(&text[at..], 0.0, plain);
    job.wrap = egui::text::TextWrapping { max_width: max_width.max(10.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    job
}

/// A file name from an item's name: what a file system can't take, as "-".
fn file_name(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '-' } else { c }).collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() { "details".into() } else { s }
}

impl App {
    /// What the server reads the item's records by: ("vm" | "host" | "pool", its reference).
    fn records_of(sel: &Node) -> Option<(u32, &'static str, String)> {
        let sid = sel.sid?;
        match sel.kind {
            Kind::Vm => Some((sid, "vm", sel.rref.clone()?)),
            Kind::Host => Some((sid, "host", sel.rref.clone()?)),
            Kind::Pool => Some((sid, "pool", String::new())),
            _ => None,
        }
    }

    /// Reads them in the background when not read yet, or longer ago than a refresh.
    fn want_records(&mut self, sid: u32, kind: &'static str, rref: &str, ctx: &egui::Context) {
        let Some(s) = self.server(sid).filter(|s| s.data.is_some()) else { return };
        let (conn, tx, ctx) = (s.conn.clone(), self.tx.clone(), ctx.clone());
        let key = format!("{}:{}", kind, rref);
        let e = self.field_records.entry((sid, key.clone())).or_default();
        if e.loading || e.at.map_or(false, |t| t.elapsed() < Duration::from_millis(REFRESH_MS)) {
            return;
        }
        e.loading = true;
        let rref = rref.to_string();
        std::thread::spawn(move || {
            let result = conn.records(kind, &rref);
            let _ = tx.send(Msg::Records { sid, key, result });
            ctx.request_repaint();
        });
    }

    /// The tab's cards: the main facts, then each record's every field.
    fn details_sections(&self, sel: &Node) -> Vec<Section> {
        let mut secs = self.general_sections(sel);
        let Some((sid, kind, rref)) = Self::records_of(sel) else { return secs };
        let Some(e) = self.field_records.get(&(sid, format!("{}:{}", kind, rref))) else { return secs };
        let names = self.server(sid).and_then(|s| s.data.as_ref()).map(ref_names).unwrap_or_default();
        let look = |r: &str| names.get(r).cloned();
        for (what, rec) in &e.recs {
            secs.push((format!("All fields · {}", what), dt::flatten(rec, &look)));
        }
        secs
    }

    pub(super) fn details_tab(&mut self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let item = Self::records_of(sel);
        if let Some((sid, kind, rref)) = &item {
            self.want_records(*sid, kind, rref, ui.ctx());
        }
        let secs = self.details_sections(sel);
        let rec = item.as_ref().and_then(|(sid, kind, rref)| self.field_records.get(&(*sid, format!("{}:{}", kind, rref))));
        let first_read = rec.map_or(false, |e| e.loading && e.recs.is_empty());
        let error = rec.and_then(|e| e.error.clone());
        let q = self.details_search.trim().to_lowercase();
        let shown = if q.is_empty() { secs.clone() } else { dt::search(&secs, &q) };
        // over the cards, as wide as they are: the search, how reading them goes, then Export
        let full = ui.available_width();
        let (x0, w) = card_span(full);
        ui.add_space(12.0);
        let (row, _) = ui.allocate_exact_size(vec2(full, 32.0), Sense::hover());
        let bar = Rect::from_min_size(pos2(row.min.x + x0, row.min.y), vec2(w, 32.0));
        let mut export = None;
        let mut left = ui.new_child(egui::UiBuilder::new().max_rect(bar).layout(egui::Layout::left_to_right(egui::Align::Center)));
        left.spacing_mut().item_spacing.x = 8.0;
        let search_w = (w - 150.0).clamp(140.0, 300.0);
        let resp = left.add(egui::TextEdit::singleline(&mut self.details_search).hint_text("Search details").desired_width(search_w).margin(vec2(8.0, 6.0)));
        if resp.has_focus() && left.input(|i| i.key_pressed(Key::Escape)) {
            self.details_search.clear();
        }
        if !q.is_empty() {
            let n: usize = shown.iter().map(|(_, r)| r.len()).sum();
            if flat_button(&mut left, "close", "Clear", None, true, true, &pal).on_hover_text("Clear the search").clicked() {
                self.details_search.clear();
            }
            left.label(egui::RichText::new(if n == 1 { "1 found".to_string() } else { format!("{} found", n) }).size(12.5).color(pal.ink_soft));
        } else if first_read {
            left.add(egui::Spinner::new().size(13.0));
            left.label(egui::RichText::new("Reading every field…").size(12.5).color(pal.ink_soft));
        } else if let Some(e) = &error {
            left.label(egui::RichText::new("Couldn't read every field").size(12.5).color(pal.orange)).on_hover_text(e.as_str());
        }
        let mut right = ui.new_child(egui::UiBuilder::new().max_rect(bar).layout(egui::Layout::right_to_left(egui::Align::Center)));
        let tip = if first_read { "Once every field is read" } else { "Save all of it (every field, not only those found) as a spreadsheet (CSV) or a PDF" };
        let up = flat_button(&mut right, "download", "Export", None, !first_read, w < 420.0, &pal).on_hover_text(tip).on_disabled_hover_text(tip);
        let menu = right.id().with("details-export");
        if up.clicked() {
            right.memory_mut(|m| m.toggle_popup(menu));
        }
        egui::popup::popup_below_widget(&right, menu, &up, egui::PopupCloseBehavior::CloseOnClick, |ui| {
            ui.set_min_width(230.0);
            ui.label(egui::RichText::new("Export as").size(12.0).color(pal.ink_soft));
            if ui.button("CSV  ·  for a spreadsheet…").clicked() {
                export = Some(false);
            }
            if ui.button("PDF  ·  to print or share…").clicked() {
                export = Some(true);
            }
        });
        if let Some(pdf) = export {
            self.export_details(sel, pdf, ui.ctx());
        }
        if shown.is_empty() && !q.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| ui.label(egui::RichText::new("Nothing in its details has those words.").color(pal.ink_faint)));
            return;
        }
        self.cards(ui, &shown, &q, out);
    }

    /// Saves the tab's every detail, as CSV or PDF, where the system's Save dialog says.
    fn export_details(&mut self, sel: &Node, pdf: bool, ctx: &egui::Context) {
        let Some(s) = sel.sid.and_then(|id| self.server(id)) else { return };
        let (sid, server) = (s.id, s.name());
        let secs = self.details_sections(sel);
        let (title, _) = self.header_text(sel);
        let what = match sel.kind {
            Kind::Vm => "VM",
            Kind::Host => "Server",
            _ => "Pool",
        };
        let now = unix_now();
        let sub = format!("{} details  ·  {}  ·  {}", what, server, fmt_date_time(now, local_offset(now)));
        let producer = format!("Simple VM Manager {}", env!("CARGO_PKG_VERSION"));
        let (bytes, ext, kind) = if pdf { (dt::pdf(&title, &sub, &producer, &secs), "pdf", "PDF") } else { (dt::csv(&secs).into_bytes(), "csv", "CSV (spreadsheet)") };
        let name = format!("{} details.{}", file_name(&title), ext);
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
        let dir = home.as_ref().map(|h| h.join("Documents")).filter(|d| d.is_dir()).or(home);
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut dialog = rfd::AsyncFileDialog::new().set_title("Export Details").add_filter(kind, &[ext]).set_file_name(name);
            if let Some(d) = dir {
                dialog = dialog.set_directory(d);
            }
            let Some(path) = pollster::block_on(dialog.save_file()).map(|f| f.path().to_path_buf()) else { return };
            let result = std::fs::write(&path, &bytes).map_err(|e| e.to_string());
            let _ = tx.send(Msg::DetailsSaved { sid, server, title, path, result });
            ctx.request_repaint();
        });
    }
}

/// Where the cards go across `full`: (from the left, how wide).
pub(super) fn card_span(full: f32) -> (f32, f32) {
    let room = full - 32.0;
    let w = (full * 0.8).max(room.min(560.0)).min(room);
    ((full - w) / 2.0, w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        assert_eq!(file_name("Web Server"), "Web Server");
        assert_eq!(file_name("a/b: c?"), "a-b- c-");
        assert_eq!(file_name(" .. "), "details");
    }

    #[test]
    fn marks_found() {
        let job = marked("Memory: memory", "mem", FontId::proportional(13.0), Color32::WHITE, Color32::RED, 100.0);
        let marks: Vec<&str> = job.sections.iter().filter(|s| s.format.background == Color32::RED).map(|s| &job.text[s.byte_range.clone()]).collect();
        assert_eq!(marks, vec!["Mem", "mem"]);
    }
}
