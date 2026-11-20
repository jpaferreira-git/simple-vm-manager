//! A server's network card's address (Change IP…, on its Networking tab): automatic (DHCP),
//! fixed (address, netmask, gateway, DNS servers) or none, and on the card it's managed through,
//! its hostname too.

use crate::app::{dialog_button, dialog_buttons, dialog_window, field_label, text_field, Look};
use crate::model::*;
use crate::theme::Pal;
use eframe::egui::{self, Rounding, Ui};
use std::net::Ipv4Addr;

const LABEL: f32 = 104.0;

/// What Save asks for: the card's new address (None: unchanged), its server's new hostname
/// (None: unchanged).
pub struct IpChange {
    pub sid: u32,
    pub host: String,
    pub host_name: String,
    pub pif: String,
    pub device: String,
    /// (mode, IP, netmask, gateway, DNS)
    pub address: Option<(String, String, String, String, String)>,
    pub hostname: Option<String>,
}

pub enum IpAction {
    None,
    Close,
    Save(IpChange),
}

pub struct IpDialog {
    pub sid: u32,
    pub host: String,
    pub pif: String,
    host_name: String,
    device: String,
    management: bool,
    coordinator: bool,
    /// 0 none, 1 DHCP, 2 fixed
    mode: usize,
    ip: String,
    netmask: String,
    gateway: String,
    dns: String,
    hostname: String,
    /// As they were, to tell what changed.
    was: (usize, String, String, String, String, String),
    /// Addresses the pool's other cards have.
    taken: Vec<String>,
}

fn mode_of(m: &str) -> usize {
    match m {
        "DHCP" => 1,
        "Static" => 2,
        _ => 0,
    }
}

/// A netmask from "255.255.255.0", "24" or "/24": the mask, or None.
pub fn parse_mask(s: &str) -> Option<u32> {
    let s = s.trim();
    let m = match s.trim_start_matches('/').parse::<u32>() {
        Ok(bits) if s.len() <= 3 => {
            if !(1..=32).contains(&bits) {
                return None;
            }
            u32::MAX << (32 - bits)
        }
        _ => u32::from(s.parse::<Ipv4Addr>().ok()?),
    };
    // its ones together on the left
    (m != 0 && (!m & (!m).wrapping_add(1)) == 0).then_some(m)
}

fn ip4(s: &str) -> Option<u32> {
    s.trim().parse::<Ipv4Addr>().ok().map(u32::from)
}

/// DNS servers from "1.1.1.1, 8.8.8.8" (commas or spaces between): each one, or the one that
/// isn't an address.
pub fn parse_dns(s: &str) -> Result<Vec<String>, String> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|x| !x.is_empty())
        .map(|x| if ip4(x).is_some() { Ok(x.to_string()) } else { Err(x.to_string()) })
        .collect()
}

/// A hostname: letters, digits and hyphens, in dot-separated parts of up to 63 (not starting or
/// ending with a hyphen).
pub fn valid_hostname(s: &str) -> bool {
    !s.is_empty() && s.len() <= 253 && s.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// "192.168.1.0/24"
fn network(ip: u32, mask: u32) -> String {
    format!("{}/{}", Ipv4Addr::from(ip & mask), mask.count_ones())
}

impl IpDialog {
    pub fn new(sid: u32, d: &PoolData, host: &HostInfo, n: &HostNic) -> Self {
        let mode = mode_of(&n.mode);
        let dns = n.dns.split(',').map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>().join(", ");
        let taken = d.hosts.iter().flat_map(|h| h.nics.iter().filter(|x| x.pif != n.pif).map(|x| x.ip.clone()).chain(std::iter::once(h.address.clone()).filter(|_| h.rref != host.rref))).filter(|a| !a.is_empty()).collect();
        IpDialog {
            sid,
            host: host.rref.clone(),
            pif: n.pif.clone(),
            host_name: host.name.clone(),
            device: n.device.clone(),
            management: n.management,
            coordinator: host.is_master,
            mode,
            ip: n.ip.clone(),
            netmask: n.netmask.clone(),
            gateway: n.gateway.clone(),
            dns: dns.clone(),
            hostname: host.hostname.clone(),
            was: (mode, n.ip.clone(), n.netmask.clone(), n.gateway.clone(), dns, host.hostname.clone()),
            taken,
        }
    }

    /// A new address typed in (for the screenshots).
    pub fn typed_ip(&mut self, ip: &str) {
        self.ip = ip.into();
    }

    fn address_changed(&self) -> bool {
        let (m, ip, mask, gw, dns, _) = &self.was;
        self.mode != *m || (self.mode == 2 && (self.ip.trim() != ip || self.netmask.trim() != mask || self.gateway.trim() != gw || self.dns.trim() != dns))
    }

    fn hostname_changed(&self) -> bool {
        self.management && self.hostname.trim() != self.was.5
    }

    /// What stops it being saved, if anything.
    fn problem(&self) -> Option<String> {
        if !self.address_changed() && !self.hostname_changed() {
            return Some("Nothing changed yet.".into());
        }
        if self.hostname_changed() && !valid_hostname(self.hostname.trim()) {
            return Some("A hostname has letters, digits and hyphens (dots between its parts).".into());
        }
        if self.mode != 2 {
            return None;
        }
        let Some(ip) = ip4(&self.ip) else { return Some("Type its address, like 192.168.1.20.".into()) };
        let Some(mask) = parse_mask(&self.netmask) else { return Some("Type its netmask, like 255.255.255.0 (or 24).".into()) };
        if mask.count_ones() > 30 {
            return Some("That netmask leaves no room for other addresses.".into());
        }
        if ip & !mask == 0 || ip | mask == u32::MAX {
            return Some(format!("{} names the network {} itself, not an address on it.", self.ip.trim(), network(ip, mask)));
        }
        if self.taken.iter().any(|t| t == self.ip.trim()) {
            return Some(format!("{} is another card's address in this pool.", self.ip.trim()));
        }
        if !self.gateway.trim().is_empty() {
            let Some(gw) = ip4(&self.gateway) else { return Some("The gateway is an address, like 192.168.1.1.".into()) };
            if gw & mask != ip & mask {
                return Some(format!("The gateway isn't on its network ({}).", network(ip, mask)));
            }
            if gw == ip {
                return Some("The gateway is another device, not this address.".into());
            }
        } else if self.management {
            return Some("The card it's managed through needs a gateway.".into());
        }
        if let Err(bad) = parse_dns(&self.dns) {
            return Some(format!("'{}' isn't a DNS server's address.", bad));
        }
        None
    }

    pub fn ui(&mut self, ctx: &egui::Context, pal: &Pal, saving: bool) -> IpAction {
        let mut action = IpAction::None;
        dialog_window(ctx, &format!("IP Address of {}", self.device)).id(egui::Id::new("ip-dialog")).show(ctx, |ui| {
            ui.set_width(500.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            let what = if self.management { "the card the pool manages it through" } else { "one of its network cards" };
            small(ui, pal, &format!("{} on '{}': {}.", self.device, self.host_name, what));
            ui.add_enabled_ui(!saving, |ui| {
                ui.horizontal(|ui| {
                    field_label(ui, "Its address:", LABEL);
                    // (the card it's managed through always has one)
                    let mut at = if self.management { self.mode.max(1) - 1 } else { self.mode };
                    let opts: &[&str] = if self.management { &["Automatic (DHCP)", "Fixed"] } else { &["None", "Automatic (DHCP)", "Fixed"] };
                    if crate::isolib::segmented(ui, pal, opts, &mut at, ui.available_width() - 4.0) {
                        self.mode = if self.management { at + 1 } else { at };
                    }
                });
                let fixed = self.mode == 2;
                let w = 220.0;
                for (label, value, hint) in [("IP address:", &mut self.ip, "192.168.1.20"), ("Netmask:", &mut self.netmask, "255.255.255.0"), ("Gateway:", &mut self.gateway, "192.168.1.1"), ("DNS servers:", &mut self.dns, "1.1.1.1, 8.8.8.8")] {
                    ui.horizontal(|ui| {
                        field_label(ui, label, LABEL);
                        ui.add_enabled_ui(fixed, |ui| {
                            let r = text_field(ui, pal, value, if label == "DNS servers:" { ui.available_width() - 4.0 } else { w }, false);
                            if fixed && value.is_empty() && !r.has_focus() {
                                ui.painter().text(r.rect.left_center() + egui::vec2(8.0, 0.0), egui::Align2::LEFT_CENTER, hint, egui::FontId::proportional(13.0), pal.ink_faint);
                            }
                        });
                    });
                }
                if self.mode == 1 {
                    indent(ui, pal, "The network's DHCP server gives it all of these (best with a reservation for its MAC, so it keeps the same address).");
                }
                if self.management {
                    ui.horizontal(|ui| {
                        field_label(ui, "Hostname:", LABEL);
                        text_field(ui, pal, &mut self.hostname, w, false);
                    });
                    indent(ui, pal, "Its system's own name for itself, on the network; its name here doesn't change.");
                }
            });
            // what happens to the connection
            if self.management && self.address_changed() {
                let mut text = "It stops answering at its old address for a moment. If these settings are wrong it may not answer at all, and they'd have to be put back at its own screen and keyboard.".to_string();
                if self.coordinator && self.mode == 2 {
                    text.push_str(" It's the pool coordinator: the app follows it to the new address.");
                }
                if self.mode == 1 {
                    text.push_str(" With DHCP its address may change: if it does, connect again at the new one.");
                }
                egui::Frame::none().fill(pal.orange.gamma_multiply(if pal.dark { 0.16 } else { 0.10 })).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::symmetric(12.0, 9.0)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(egui::Label::new(egui::RichText::new(text).size(12.5).color(pal.ink)).wrap());
                });
            }
            let problem = self.problem();
            ui.add_space(4.0);
            dialog_buttons(ui, |ui| {
                if dialog_button(ui, pal, "close", "Cancel", Look::Plain, !saving) || ui.input(|i| i.key_pressed(egui::Key::Escape)) && !saving {
                    action = IpAction::Close;
                }
                let ready = problem.is_none() && !saving;
                if dialog_button(ui, pal, "check", if saving { "Changing…" } else { "Change" }, Look::Primary, ready) {
                    action = IpAction::Save(self.change());
                }
                if let Some(why) = &problem {
                    ui.add(egui::Label::new(egui::RichText::new(why).size(12.5).color(pal.ink_faint)).truncate());
                }
            });
        });
        action
    }

    fn change(&self) -> IpChange {
        let address = self.address_changed().then(|| match self.mode {
            2 => {
                let mask = parse_mask(&self.netmask).map(|m| Ipv4Addr::from(m).to_string()).unwrap_or_default();
                let dns = parse_dns(&self.dns).unwrap_or_default().join(",");
                ("Static".to_string(), self.ip.trim().to_string(), mask, self.gateway.trim().to_string(), dns)
            }
            1 => ("DHCP".to_string(), String::new(), String::new(), String::new(), String::new()),
            _ => ("None".to_string(), String::new(), String::new(), String::new(), String::new()),
        });
        IpChange {
            sid: self.sid,
            host: self.host.clone(),
            host_name: self.host_name.clone(),
            pif: self.pif.clone(),
            device: self.device.clone(),
            address,
            hostname: self.hostname_changed().then(|| self.hostname.trim().to_string()),
        }
    }
}

fn small(ui: &mut Ui, pal: &Pal, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).color(pal.ink_soft)).wrap());
}

fn indent(ui: &mut Ui, pal: &Pal, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(LABEL + ui.spacing().item_spacing.x);
        ui.add(egui::Label::new(egui::RichText::new(text).size(12.0).color(pal.ink_faint)).wrap());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog() -> IpDialog {
        let n = HostNic { device: "eth0".into(), ip: "192.168.1.20".into(), netmask: "255.255.255.0".into(), gateway: "192.168.1.1".into(), dns: "1.1.1.1".into(), mode: "Static".into(), management: true, pif: "p0".into(), ..Default::default() };
        let other = HostNic { device: "eth0".into(), ip: "192.168.1.21".into(), pif: "p1".into(), ..Default::default() };
        let h = HostInfo { rref: "h0".into(), name: "godofredo".into(), hostname: "godofredo".into(), address: "192.168.1.20".into(), is_master: true, nics: vec![n.clone()], ..Default::default() };
        let h2 = HostInfo { rref: "h1".into(), address: "192.168.1.21".into(), nics: vec![other], ..Default::default() };
        let d = PoolData { hosts: vec![h.clone(), h2], ..Default::default() };
        IpDialog::new(1, &d, &h, &n)
    }

    #[test]
    fn masks() {
        assert_eq!(parse_mask("255.255.255.0"), Some(0xffff_ff00));
        assert_eq!(parse_mask("24"), Some(0xffff_ff00));
        assert_eq!(parse_mask("/16"), Some(0xffff_0000));
        assert_eq!(parse_mask("255.0.255.0"), None);
        assert_eq!(parse_mask("0.0.0.0"), None);
        assert_eq!(parse_mask("33"), None);
        assert_eq!(parse_dns("1.1.1.1, 8.8.8.8 9.9.9.9"), Ok(vec!["1.1.1.1".to_string(), "8.8.8.8".to_string(), "9.9.9.9".to_string()]));
        assert_eq!(parse_dns("1.1.1.1, dns.local"), Err("dns.local".to_string()));
        assert!(valid_hostname("godofredo") && valid_hostname("xen-01.home.lan"));
        assert!(!valid_hostname("-bad") && !valid_hostname("a b") && !valid_hostname("a..b"));
    }

    #[test]
    fn what_stops_it() {
        let mut d = dialog();
        assert_eq!(d.problem().as_deref(), Some("Nothing changed yet."));
        d.ip = "192.168.1.30".into();
        assert_eq!(d.problem(), None);
        d.ip = "192.168.1.21".into();
        assert_eq!(d.problem().as_deref(), Some("192.168.1.21 is another card's address in this pool."));
        d.ip = "192.168.1.255".into();
        assert!(d.problem().unwrap().contains("network 192.168.1.0/24 itself"));
        d.ip = "192.168.1.30".into();
        d.gateway = "10.0.0.1".into();
        assert_eq!(d.problem().as_deref(), Some("The gateway isn't on its network (192.168.1.0/24)."));
        d.gateway = String::new();
        assert_eq!(d.problem().as_deref(), Some("The card it's managed through needs a gateway."));
        d.gateway = "192.168.1.1".into();
        d.dns = "1.1.1.1, x".into();
        assert_eq!(d.problem().as_deref(), Some("'x' isn't a DNS server's address."));
        // DHCP: nothing to type
        d.mode = 1;
        assert_eq!(d.problem(), None);
        let c = d.change();
        assert_eq!(c.address.unwrap().0, "DHCP");
        // the hostname alone
        let mut d = dialog();
        d.hostname = "xen 1".into();
        assert!(d.problem().unwrap().starts_with("A hostname"));
        d.hostname = "xen-1".into();
        let c = d.change();
        assert!(c.address.is_none() && c.hostname.as_deref() == Some("xen-1"));
        // a fixed address saved with the netmask written out and DNS servers comma-separated
        let mut d = dialog();
        (d.ip, d.netmask, d.dns) = ("192.168.1.40".into(), "24".into(), "1.1.1.1 8.8.8.8".into());
        let (mode, ip, mask, gw, dns) = d.change().address.unwrap();
        assert_eq!((mode.as_str(), ip.as_str(), mask.as_str(), gw.as_str(), dns.as_str()), ("Static", "192.168.1.40", "255.255.255.0", "192.168.1.1", "1.1.1.1,8.8.8.8"));
    }
}
