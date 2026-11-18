//! Moving a VM: to another server, its disks to other storage, or both, in one dialog. Running, it
//! keeps running (moved live, its disks copied as it runs); off, its disks are copied and swapped
//! in, and it starts on the chosen server from then on. The server's choices say why one can't
//! take it; each disk offers the storage that server sees, where it is kept when it can be.

use crate::app::{dialog_button, dialog_buttons, dialog_window, Look};
use crate::model::*;
use crate::newvm::alert;
use crate::theme::Pal;
use eframe::egui::{self, Ui};

/// A server it could go to: whether it's where it is, its free memory, and why it can't (if so).
pub struct MoveHost {
    pub rref: String,
    pub name: String,
    pub now: bool,
    pub free: i64,
    pub why_not: Option<String>,
}

/// One of its disks: its name and size, and the storage it's on.
pub struct MoveDisk {
    pub vdi: String,
    pub name: String,
    pub size: i64,
    pub sr: String,
}

/// Storage a disk could go to: its name, its room, the servers that see it, and whether it can
/// send or take a running VM's disk (live storage migration).
pub struct MoveSr {
    pub rref: String,
    pub name: String,
    pub free: i64,
    pub seen_by: Vec<String>,
    pub live_out: bool,
    pub live_in: bool,
}

pub enum MoveAction {
    None,
    Close,
    Move(MoveVm),
}

pub struct MoveDialog {
    pub sid: u32,
    pub vm: String,
    name: String,
    running: bool,
    /// Why it can't move at all now (suspended, busy…).
    stuck: Option<String>,
    /// The memory it needs where it goes.
    mem: i64,
    hosts: Vec<MoveHost>,
    disks: Vec<MoveDisk>,
    srs: Vec<MoveSr>,
    /// Its DVD drive with a disc: the drive, the disc's name, and the servers that see its library.
    dvd: Option<(String, String, Vec<String>)>,
    /// What's chosen: the server, and each disk's storage.
    host: String,
    to: Vec<String>,
    /// Settings of the pool's pointing to storage that's gone (XenServer uses them when moving a
    /// running VM's disks).
    gone: Vec<String>,
}

impl MoveDialog {
    /// For VM `v` of pool `d`; `to` the server to start from (dropped on it in the tree).
    pub fn new(sid: u32, d: &PoolData, v: &VmInfo, to: Option<&str>, busy: bool) -> MoveDialog {
        let running = matches!(v.power, PowerState::Running);
        let stuck = match v.power {
            _ if busy => Some("Wait for what's being done to it to finish.".to_string()),
            PowerState::Suspended => Some("It's suspended: resume it or shut it down to move it.".to_string()),
            PowerState::Paused => Some("It's paused: unpause it or shut it down to move it.".to_string()),
            _ if d.host_down_for(v).is_some() => Some("Its server isn't there.".to_string()),
            _ => None,
        };
        let mem = if v.mem_actual > 0 { v.mem_actual } else { v.mem_dyn_max };
        let here = v.home().map(String::from);
        let mut hosts: Vec<MoveHost> = d
            .hosts
            .iter()
            .map(|h| {
                let now = here.as_deref() == Some(h.rref.as_str());
                let why_not = if !h.is_up() {
                    Some("off".to_string())
                } else if !h.enabled {
                    Some("not taking VMs".to_string())
                } else if running && !now && h.mem_free < mem {
                    Some(format!("only {} free", fmt_bytes(h.mem_free)))
                } else {
                    None
                };
                MoveHost { rref: h.rref.clone(), name: h.name.clone(), now, free: h.mem_free, why_not }
            })
            .collect();
        hosts.sort_by_key(|h| (!h.now, h.name.to_lowercase()));
        let disks: Vec<MoveDisk> = v.disks.iter().filter(|k| k.kind == "Disk" && !k.vdi.is_empty()).map(|k| MoveDisk { vdi: k.vdi.clone(), name: k.name.clone(), size: k.size, sr: k.sr.clone() }).collect();
        let srs: Vec<MoveSr> = d
            .srs
            .iter()
            .filter(|x| !x.is_iso() && x.kind != "udev" && x.size > 0)
            .map(|x| {
                let seen_by = match &x.host {
                    Some(h) => vec![h.clone()],
                    None => x.links.iter().filter(|l| l.1).map(|l| l.0.clone()).collect(),
                };
                MoveSr {
                    rref: x.rref.clone(),
                    name: if x.host.is_some() { format!("{} ({})", x.name, x.host.as_deref().and_then(|h| d.host(h)).map_or("", |h| h.name.as_str())) } else { x.name.clone() },
                    free: (x.size - x.used).max(0),
                    seen_by,
                    live_out: x.live_out,
                    live_in: x.live_in,
                }
            })
            .collect();
        let dvd = v.disks.iter().find(|k| k.kind == "DVD drive" && !k.vdi.is_empty()).map(|k| {
            let seen_by = match d.sr(&k.sr) {
                Some(x) if x.host.is_some() => x.host.iter().cloned().collect(),
                Some(x) => x.links.iter().filter(|l| l.1).map(|l| l.0.clone()).collect(),
                None => d.hosts.iter().map(|h| h.rref.clone()).collect(),
            };
            (k.vbd.clone(), k.name.clone(), seen_by)
        });
        let start = to.map(String::from).or(here.clone()).or_else(|| hosts.iter().find(|h| h.why_not.is_none()).map(|h| h.rref.clone())).unwrap_or_default();
        let gone = d.gone_storage().into_iter().map(|g| g.0).collect();
        let mut dialog = MoveDialog { sid, vm: v.rref.clone(), name: v.name.clone(), running, stuck, mem, hosts, disks, srs, dvd, host: String::new(), to: vec![], gone };
        dialog.to = dialog.disks.iter().map(|k| k.sr.clone()).collect();
        dialog.pick(&start);
        dialog
    }

    /// The storage the server `h` sees.
    fn seen(&self, h: &str) -> Vec<&MoveSr> {
        let mut out: Vec<&MoveSr> = self.srs.iter().filter(|x| x.seen_by.iter().any(|s| s == h)).collect();
        out.sort_by_key(|x| x.name.to_lowercase());
        out
    }

    /// Server `h` chosen: each disk stays where it is when `h` sees that storage, else goes to the
    /// storage `h` sees with the most room (its own first).
    fn pick(&mut self, h: &str) {
        self.host = h.to_string();
        let running = self.running;
        let seen: Vec<(String, i64, bool)> = self.seen(h).iter().filter(|x| !running || x.live_in).map(|x| (x.rref.clone(), x.free, x.seen_by.len() == 1)).collect();
        let kept: Vec<String> = self.seen(h).iter().map(|x| x.rref.clone()).collect();
        for (i, k) in self.disks.iter().enumerate() {
            if seen.iter().any(|s| s.0 == self.to[i]) || (self.to[i] == k.sr && kept.contains(&k.sr)) {
                continue;
            }
            self.to[i] = if kept.contains(&k.sr) {
                k.sr.clone()
            } else {
                seen.iter().filter(|s| s.1 >= k.size).max_by_key(|s| (s.2, s.1)).or(seen.iter().max_by_key(|s| s.1)).map_or(String::new(), |s| s.0.clone())
            };
        }
    }

    fn sr(&self, r: &str) -> Option<&MoveSr> {
        self.srs.iter().find(|x| x.rref == r)
    }

    /// What moving would do now, or why it can't.
    fn plan(&self) -> Result<MoveVm, String> {
        if let Some(why) = &self.stuck {
            return Err(why.clone());
        }
        let h = self.hosts.iter().find(|h| h.rref == self.host).ok_or("Choose a server.")?;
        if let Some(why) = &h.why_not {
            return Err(format!("{} is {}.", h.name, why));
        }
        let mut disks = vec![];
        let mut need: Vec<(String, i64)> = vec![];
        for (i, k) in self.disks.iter().enumerate() {
            let to = &self.to[i];
            let x = self.sr(to).ok_or_else(|| format!("{} sees no storage for {}.", h.name, k.name))?;
            if !x.seen_by.iter().any(|s| *s == self.host) {
                return Err(format!("{} doesn't see {}.", h.name, x.name));
            }
            if *to != k.sr {
                // a running VM's disk moves live: its storage must send it, the new one take it
                if self.running {
                    if let Some(from) = self.sr(&k.sr).filter(|f| !f.live_out) {
                        return Err(format!("{} can't send a disk away while its VM runs: shut it down to move {}.", from.name, k.name));
                    }
                    if !x.live_in {
                        return Err(format!("{} can't take a disk while its VM runs: choose other storage, or shut the VM down first.", x.name));
                    }
                }
                disks.push((k.vdi.clone(), to.clone()));
                match need.iter_mut().find(|n| n.0 == *to) {
                    Some(n) => n.1 += k.size,
                    None => need.push((to.clone(), k.size)),
                }
            }
        }
        for (r, size) in &need {
            if let Some(x) = self.sr(r).filter(|x| x.free < *size) {
                return Err(format!("{} has {} free, and {} has to go there.", x.name, fmt_bytes(x.free), fmt_bytes(*size)));
            }
        }
        if h.now && disks.is_empty() {
            return Err(if self.running { "It's already there: choose another server or storage.".into() } else { "That's where it is: choose another server or storage.".into() });
        }
        Ok(MoveVm { vm: self.vm.clone(), host: self.host.clone(), disks, eject: self.eject(), running: self.running })
    }

    /// Its DVD drive, to empty first: its disc's library isn't seen where it goes.
    fn eject(&self) -> Option<String> {
        let (vbd, _, seen_by) = self.dvd.as_ref()?;
        (!seen_by.iter().any(|h| *h == self.host)).then(|| vbd.clone())
    }

    pub fn ui(&mut self, ctx: &egui::Context, pal: &Pal) -> MoveAction {
        let mut action = MoveAction::None;
        dialog_window(ctx, &format!("Move '{}'", self.name)).id(egui::Id::new("move-vm-dialog")).show(ctx, |ui| {
            ui.set_width(540.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            if let Some(why) = self.stuck.clone() {
                alert(ui, pal, false, "It can't move now.", &why, None);
            } else {
                self.choices(ui, pal);
            }
            let plan = self.plan();
            ui.add_space(4.0);
            match &plan {
                Ok(m) => {
                    let h = self.hosts.iter().find(|h| h.rref == m.host).map_or("", |h| h.name.as_str());
                    let size: i64 = m.disks.iter().filter_map(|(vdi, _)| self.disks.iter().find(|k| k.vdi == *vdi)).map(|k| k.size).sum();
                    let n = m.disks.len();
                    let disks = if n == 1 { format!("its disk ({})", fmt_bytes(size)) } else { format!("its {} disks ({})", n, fmt_bytes(size)) };
                    let text = match (m.running, self.hosts.iter().any(|x| x.now && x.rref == m.host), n) {
                        (true, false, 0) => format!("It keeps running: moved live to {}, its memory ({}) copied as it runs.", h, fmt_bytes(self.mem)),
                        (true, false, _) => format!("It keeps running: moved live to {}, {} copied as it runs; that can take a while.", h, disks),
                        (true, true, _) => format!("It keeps running: {} copied to the new storage as it runs; that can take a while.", disks),
                        (false, _, 0) => format!("It starts on {} from now on; its disks stay where they are.", h),
                        (false, _, 1) => format!("{} copied to the new storage, the copy taking its place; it starts on {} from now on.", first_up(&disks), h),
                        (false, _, _) => format!("{} copied to the new storage, the copies taking their place; it starts on {} from now on.", first_up(&disks), h),
                    };
                    ui.label(egui::RichText::new(text).size(12.5).color(pal.ink_soft));
                    if m.running && !m.disks.is_empty() && !self.gone.is_empty() {
                        let what = self.gone.join("; ");
                        alert(ui, pal, false, "This will fail.", &format!("{}: storage that no longer exists, which XenServer uses when moving a running VM's disks. Make another storage the pool's default first (Make Default, on its page).", what), None);
                    }
                    if m.eject.is_some() {
                        let disc = self.dvd.as_ref().map_or("", |d| d.1.as_str());
                        alert(ui, pal, false, "Its DVD drive is emptied first.", &format!("'{}' is in a library {} doesn't see.", disc, h), None);
                    }
                }
                Err(why) if self.stuck.is_none() => {
                    ui.label(egui::RichText::new(why).size(12.5).color(pal.ink_faint));
                }
                Err(_) => {}
            }
            ui.add_space(4.0);
            dialog_buttons(ui, |ui| {
                if dialog_button(ui, pal, "move", "Move", Look::Primary, plan.is_ok()) {
                    if let Ok(m) = plan {
                        action = MoveAction::Move(m);
                    }
                }
                if dialog_button(ui, pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    action = MoveAction::Close;
                }
            });
        });
        action
    }

    /// The servers to choose from, then each disk's storage.
    fn choices(&mut self, ui: &mut Ui, pal: &Pal) {
        ui.label(egui::RichText::new(if self.running { "TO SERVER" } else { "STARTS ON" }).size(11.0).color(pal.ink_faint));
        let mut picked = None;
        for h in &self.hosts {
            ui.horizontal(|ui| {
                let on = h.rref == self.host;
                let resp = ui.add_enabled(h.why_not.is_none(), egui::RadioButton::new(on, &h.name));
                if resp.clicked() && !on {
                    picked = Some(h.rref.clone());
                }
                let aside = match (&h.why_not, h.now) {
                    (Some(why), _) => why.clone(),
                    (None, true) => format!("where it is  ·  {} free", fmt_bytes(h.free)),
                    (None, false) => format!("{} free", fmt_bytes(h.free)),
                };
                ui.label(egui::RichText::new(aside).size(12.0).color(pal.ink_faint));
            });
        }
        if let Some(h) = picked {
            self.pick(&h);
        }
        if self.disks.is_empty() {
            return;
        }
        ui.add_space(4.0);
        ui.label(egui::RichText::new("DISKS").size(11.0).color(pal.ink_faint));
        let seen: Vec<(String, String, i64, bool)> = self.seen(&self.host).iter().map(|x| (x.rref.clone(), x.name.clone(), x.free, !self.running || x.live_in)).collect();
        let running = self.running;
        for i in 0..self.disks.len() {
            let (name, size, now) = (self.disks[i].name.clone(), self.disks[i].size, self.disks[i].sr.clone());
            // its name and size, then where it goes
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&name).color(pal.ink));
                ui.label(egui::RichText::new(fmt_bytes(size)).size(12.0).color(pal.ink_faint));
            });
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                let label = |r: &str| -> String {
                    match seen.iter().find(|s| s.0 == r) {
                        Some((_, n, _, _)) if r == now => format!("{}  ·  where it is", n),
                        Some((_, n, _, false)) => format!("{}  ·  can't take it while it runs", n),
                        Some((_, n, free, _)) => format!("{}  ·  {} free", n, fmt_bytes(*free)),
                        None => "Choose storage".to_string(),
                    }
                };
                let shown = label(&self.to[i]);
                egui::ComboBox::from_id_salt(("move-disk", i)).width(ui.available_width() - 4.0).selected_text(shown).show_ui(ui, |ui| {
                    for (r, _, _, ok) in &seen {
                        let on = self.to[i] == *r;
                        let resp = ui.add_enabled(*ok || *r == now, egui::SelectableLabel::new(on, label(r)));
                        let resp = if running { resp.on_disabled_hover_text("Its kind of storage doesn't support moving a running VM's disk to it (live storage migration): shut the VM down to move the disk there") } else { resp };
                        if resp.clicked() {
                            self.to[i] = r.clone();
                        }
                    }
                });
            });
        }
    }
}

/// `text` with its first letter in capitals.
fn first_up(text: &str) -> String {
    let mut c = text.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::{Connection, DemoConnection};

    #[test]
    fn where_it_goes() {
        let d = DemoConnection::new().fetch(0).unwrap();
        let vm = |r: &str| d.vm(r).unwrap();
        let disk = |r: &str| vm(r).disks.iter().find(|k| k.kind == "Disk").unwrap().vdi.clone();
        // running, its disk on shared storage both servers see: live, the disk stays
        let m = MoveDialog::new(1, &d, vm("vm-web"), Some("host-b"), false).plan().unwrap();
        assert_eq!((m.host.as_str(), m.disks.len(), m.running), ("host-b", 0, true));
        // running on its server's own storage: its disk goes to the other server's own
        let m = MoveDialog::new(1, &d, vm("vm-mail"), Some("host-a"), false).plan().unwrap();
        assert_eq!(m.disks, vec![(disk("vm-mail"), "sr-local1".to_string())]);
        // off: the same, and it starts there from then on
        let m = MoveDialog::new(1, &d, vm("vm-desk"), Some("host-b"), false).plan().unwrap();
        assert_eq!((m.disks.clone(), m.running), (vec![(disk("vm-desk"), "sr-local2".to_string())], false));
        // storage that can't take a running VM's disk (no VDI_MIRROR_IN): not chosen for it, the
        // shared one instead; but fine for one that's off
        let mut d2 = d.clone();
        d2.srs.iter_mut().filter(|x| x.rref == "sr-local1").for_each(|x| x.live_in = false);
        let m = MoveDialog::new(1, &d2, d2.vm("vm-mail").unwrap(), Some("host-a"), false).plan().unwrap();
        assert_eq!(m.disks, vec![(disk("vm-mail"), "sr-nfs".to_string())]);
        let mut chosen = MoveDialog::new(1, &d2, d2.vm("vm-mail").unwrap(), Some("host-a"), false);
        chosen.to[0] = "sr-local1".into();
        assert!(chosen.plan().unwrap_err().contains("can't take a disk while its VM runs"));
        d2.srs.iter_mut().filter(|x| x.rref == "sr-local2").for_each(|x| x.live_in = false);
        let m = MoveDialog::new(1, &d2, d2.vm("vm-desk").unwrap(), Some("host-b"), false).plan().unwrap();
        assert_eq!(m.disks, vec![(disk("vm-desk"), "sr-local2".to_string())]);
        // nowhere new, or can't now
        assert!(MoveDialog::new(1, &d, vm("vm-web"), None, false).plan().is_err());
        assert!(MoveDialog::new(1, &d, vm("vm-build"), Some("host-a"), false).plan().is_err()); // suspended
        assert!(MoveDialog::new(1, &d, vm("vm-web"), Some("host-b"), true).plan().is_err()); // busy
    }
}
