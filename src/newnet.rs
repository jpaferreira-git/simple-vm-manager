//! The New Network dialog: a network for VMs' cards, on a VLAN of a network card (made on every
//! server of the pool, on the card of that name) or private (no card: it joins each server's VMs
//! among themselves). What's needed comes first; the MTU waits under Advanced. A failure is said
//! in plain words, with what was asked and the server's answer.
//!
//! The same dialog changes a network (Edit Network): its name and description; its MTU (not a
//! card's own network's: that goes with the card); and a VLAN's card and tag, while no running VM
//! has a card on it (the VLAN is made again on every server).

use crate::app::{dialog_button, dialog_buttons, dialog_window, field_label, text_field, Look, LABEL_WIDTH};
use crate::conn::ApiError;
use crate::isolib::{advanced, failure_panel, note, segmented, Failure};
use crate::model::{NetInfo, NetworkEdit, NewNetwork};
use crate::theme::Pal;
use eframe::egui;
use std::time::Duration;

pub enum NetworkAction {
    None,
    Close,
    /// Make it in the pool with this id.
    Create(u32, NewNetwork),
    /// Change it, in the pool with this id.
    Edit(u32, NetworkEdit),
}

/// The network Edit Network changes, as it is; and the running VMs with a card on it (its VLAN
/// can't change meanwhile, and a new MTU waits for them to start again).
#[derive(Clone, Debug)]
pub struct Editing {
    pub was: NetInfo,
    pub running: Vec<String>,
}

/// A pool's network cards to put a VLAN on: each card's name with what's on it now (as the
/// coordinator has them), and the VLANs there already (card, tag, network).
#[derive(Clone, Debug, Default)]
pub struct PoolCards {
    pub sid: u32,
    pub cards: Vec<(String, String)>,
    pub taken: Vec<(String, i64, String)>,
}

pub struct NetworkDialog {
    pub sid: u32,
    pools: Vec<(u32, String)>,
    cards: Vec<PoolCards>,
    name: String,
    description: String,
    /// On a card's VLAN (else private).
    vlan: bool,
    card: usize,
    tag: String,
    mtu: String,
    advanced: bool,
    busy: Option<String>,
    problem: String,
    failure: Option<Failure>,
    editing: Option<Editing>,
}

impl NetworkDialog {
    pub fn new(sid: u32, pools: Vec<(u32, String)>, cards: Vec<PoolCards>) -> NetworkDialog {
        let mut d = NetworkDialog {
            sid,
            pools,
            cards,
            name: String::new(),
            description: String::new(),
            vlan: true,
            card: 0,
            tag: String::new(),
            mtu: "1500".into(),
            advanced: false,
            busy: None,
            problem: String::new(),
            failure: None,
            editing: None,
        };
        // a pool without cards to tag can only have private ones
        d.vlan = !d.pool_cards().cards.is_empty();
        d
    }

    /// Edit Network, for network `e.was` of pool `sid`.
    pub fn edit(sid: u32, pools: Vec<(u32, String)>, cards: Vec<PoolCards>, e: Editing) -> NetworkDialog {
        let mut d = NetworkDialog::new(sid, pools, cards);
        let n = &e.was;
        (d.name, d.description, d.mtu) = (n.name.clone(), n.description.clone(), n.mtu.to_string());
        d.vlan = n.vlan >= 0;
        if d.vlan {
            d.tag = n.vlan.to_string();
            let pc = d.pool_cards();
            match pc.cards.iter().position(|c| c.0 == n.device) {
                Some(i) => d.card = i,
                // (its card isn't the coordinator's: shown as it is)
                None => d.cards.iter_mut().filter(|c| c.sid == sid).for_each(|c| c.cards.push((n.device.clone(), n.name.clone()))),
            }
            d.card = d.pool_cards().cards.iter().position(|c| c.0 == n.device).unwrap_or(0);
        }
        d.editing = Some(e);
        d
    }

    /// A card's own network (no VLAN): what it's on, and its MTU, go with the card.
    fn card_own(&self) -> bool {
        self.editing.as_ref().is_some_and(|e| !e.was.device.is_empty() && e.was.vlan < 0)
    }

    /// What Edit Network would change, if it's right: (the change, why not).
    fn edit_request(&self, e: &Editing) -> Result<NetworkEdit, String> {
        let nw = self.request();
        let was = &e.was;
        let vlan = nw.vlan.clone().filter(|(card, tag)| was.vlan >= 0 && (card != &was.device || *tag != was.vlan));
        // its own VLAN isn't in the way of itself
        let taken: Vec<(String, i64, String)> = self.pool_cards().taken.into_iter().filter(|t| !(t.0 == was.device && t.1 == was.vlan)).collect();
        let check = NewNetwork { vlan: vlan.clone(), ..nw.clone() };
        if let Some(p) = check.problem(&taken) {
            return Err(p);
        }
        if vlan.is_some() && !e.running.is_empty() {
            return Err("Shut down the VMs on it first, to change its VLAN.".into());
        }
        let mtu = (nw.mtu != was.mtu && !self.card_own()).then_some(nw.mtu);
        Ok(NetworkEdit { net: was.rref.clone(), name: nw.name, description: nw.description, mtu, vlan, replug: e.running.is_empty() })
    }

    /// Whether Save has anything to save.
    fn changed(&self) -> bool {
        let Some(e) = &self.editing else { return true };
        let nw = self.request();
        let w = &e.was;
        nw.name != w.name || nw.description != w.description || (nw.mtu != w.mtu && !self.card_own()) || nw.vlan.as_ref().is_some_and(|(c, t)| w.vlan >= 0 && (c != &w.device || *t != w.vlan))
    }

    /// For the screenshot checks: a VLAN typed in.
    pub fn filled(mut self, name: &str, tag: &str, description: &str) -> NetworkDialog {
        (self.name, self.tag, self.description) = (name.into(), tag.into(), description.into());
        self
    }

    fn pool_cards(&self) -> PoolCards {
        self.cards.iter().find(|c| c.sid == self.sid).cloned().unwrap_or_default()
    }

    fn request(&self) -> NewNetwork {
        let pc = self.pool_cards();
        let tag = self.tag.trim().parse::<i64>().unwrap_or(-1);
        NewNetwork {
            name: self.name.trim().to_string(),
            description: self.description.trim().to_string(),
            mtu: self.mtu.trim().parse().unwrap_or(0),
            vlan: if self.vlan { pc.cards.get(self.card).map(|c| (c.0.clone(), tag)) } else { None },
        }
    }

    /// Checks what was typed; if it's right, the request to send (and the dialog waits).
    pub fn submit(&mut self) -> NetworkAction {
        if let Some(e) = self.editing.clone() {
            return match self.edit_request(&e) {
                Err(p) => {
                    self.problem = p;
                    NetworkAction::None
                }
                Ok(x) => {
                    self.problem.clear();
                    self.failure = None;
                    self.busy = Some(match &x.vlan {
                        Some((card, tag)) => format!("Moving it to VLAN {} on {}, on every server…", tag, card),
                        None => "Saving…".into(),
                    });
                    NetworkAction::Edit(self.sid, x)
                }
            };
        }
        let nw = self.request();
        if let Some(p) = nw.problem(&self.pool_cards().taken) {
            self.problem = p;
            return NetworkAction::None;
        }
        self.problem.clear();
        self.failure = None;
        let pool = self.pools.iter().find(|p| p.0 == self.sid).map(|p| p.1.clone()).unwrap_or_default();
        self.busy = Some(match &nw.vlan {
            Some((card, tag)) => format!("Making it on {} of every server in {} (VLAN {})…", card, pool, tag),
            None => format!("Making it in {}…", pool),
        });
        NetworkAction::Create(self.sid, nw)
    }

    /// The server couldn't change it: say why, and show what was asked and what came back.
    pub fn edit_failed(&mut self, x: &NetworkEdit, e: &ApiError) {
        self.busy = None;
        let (title, hint) = match e.code.as_str() {
            "CONNECTION" => ("Couldn't reach your XenServer".to_string(), e.message()),
            "RBAC_PERMISSION_DENIED" => ("Your account isn't allowed to change networks".into(), "Sign in with an account that has the Pool Admin role.".into()),
            "PIF_VLAN_EXISTS" => ("That card has that VLAN already".into(), "Choose another tag. The network is back on its old VLAN.".into()),
            "VLAN_TAG_INVALID" => ("The server doesn't take that VLAN tag".into(), "A tag goes from 1 to 4094. The network is back on its old VLAN.".into()),
            "PIF_IN_USE" | "PIF_DOES_NOT_ALLOW_UNPLUG" => ("Its VLAN is in use on a server".into(), "Something on the servers uses it (storage, or their management): it stays as it was.".into()),
            _ => ("Couldn't change the network".into(), "Your XenServer turned it down. Its answer is below.".into()),
        };
        let quote = |v: &str| if v.is_empty() || v.contains(|c: char| c.is_whitespace() || c == '"') { format!("\"{}\"", v.replace('"', "\\\"")) } else { v.to_string() };
        let mut transcript = format!("$ xe network-param-set uuid=(the network) name-label={} name-description={}\n", quote(&x.name), quote(&x.description));
        if let Some(mtu) = x.mtu {
            transcript.push_str(&format!("$ xe network-param-set uuid=(the network) MTU={}\n", mtu));
        }
        if let Some((card, tag)) = &x.vlan {
            transcript.push_str(&format!("$ xe vlan-destroy uuid=(its VLAN on each server)\n$ xe pool-vlan-create network-uuid=(the network) pif-uuid=(the coordinator's {}) vlan={}\n", card, tag));
        }
        transcript.push_str(&format!("Error code: {}\n", e.code));
        for d in e.details.iter().filter(|d| !d.trim().is_empty()) {
            transcript.push_str(d.trim());
            transcript.push('\n');
        }
        self.failure = Some(Failure { title, hint, transcript: transcript.trim_end().to_string(), copied: None });
    }

    /// The server couldn't make it: say why, and show what was asked and what came back.
    pub fn failed(&mut self, nw: &NewNetwork, e: &ApiError) {
        self.busy = None;
        let (title, hint) = match e.code.as_str() {
            "CONNECTION" => ("Couldn't reach your XenServer".to_string(), e.message()),
            "RBAC_PERMISSION_DENIED" => ("Your account isn't allowed to add networks".into(), "Sign in with an account that has the Pool Admin role.".into()),
            "PIF_VLAN_EXISTS" => ("That card has that VLAN already".into(), "Choose another tag, or use the network that has it.".into()),
            "VLAN_TAG_INVALID" => ("The server doesn't take that VLAN tag".into(), "A tag goes from 1 to 4094.".into()),
            "PIF_IS_VLAN" | "PIF_IS_NOT_PHYSICAL" | "CANNOT_ADD_VLAN_TO_BOND_SLAVE" => ("A VLAN can't go on that card".into(), "Choose a physical card or a bond.".into()),
            _ => ("Couldn't make the network".into(), "Your XenServer turned it down. Its answer is below.".into()),
        };
        let quote = |v: &str| if v.is_empty() || v.contains(|c: char| c.is_whitespace() || c == '"') { format!("\"{}\"", v.replace('"', "\\\"")) } else { v.to_string() };
        let mut transcript = format!("$ xe network-create name-label={} MTU={}\n", quote(&nw.name), nw.mtu);
        if let Some((card, tag)) = &nw.vlan {
            transcript.push_str(&format!("$ xe pool-vlan-create network-uuid=(the new network) pif-uuid=(the coordinator's {}) vlan={}\n", card, tag));
        }
        transcript.push_str(&format!("Error code: {}\n", e.code));
        for d in e.details.iter().filter(|d| !d.trim().is_empty()) {
            transcript.push_str(d.trim());
            transcript.push('\n');
        }
        self.failure = Some(Failure { title, hint, transcript: transcript.trim_end().to_string(), copied: None });
    }

    pub fn ui(&mut self, ctx: &egui::Context, pal: &Pal) -> NetworkAction {
        let mut action = NetworkAction::None;
        let busy = self.busy.is_some();
        let editing = self.editing.clone();
        let card_own = self.card_own();
        // (a VLAN changes only while nothing running is on it)
        let vlan_fixed = editing.as_ref().is_some_and(|e| !e.running.is_empty());
        dialog_window(ctx, if editing.is_some() { "Edit Network" } else { "New Network" }).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            let intro = match &editing {
                None => "A network for your VMs' cards. On a VLAN of a network card it reaches what that card is plugged into, on every server of the pool; a private one joins only each server's VMs among themselves.".to_string(),
                Some(e) if card_own => format!("It's {}'s own network: what it's on and its MTU go with the card. Its name and description can change.", e.was.device),
                Some(e) if e.was.vlan >= 0 => "Change its name and description, its MTU, or the VLAN it's on. The VMs' cards stay on it.".into(),
                Some(_) => "Change its name, its description or its MTU. The VMs' cards stay on it.".into(),
            };
            ui.add(egui::Label::new(egui::RichText::new(intro).color(pal.ink_soft)).wrap());
            ui.add_space(2.0);
            ui.add_enabled_ui(!busy, |ui| {
                if self.pools.len() > 1 && editing.is_none() {
                    ui.horizontal(|ui| {
                        field_label(ui, "Pool:", LABEL_WIDTH);
                        let current = self.pools.iter().find(|p| p.0 == self.sid).map(|p| p.1.clone()).unwrap_or_default();
                        let before = self.sid;
                        egui::ComboBox::from_id_salt("net-pool").selected_text(current).width(ui.available_width() - 8.0).show_ui(ui, |ui| {
                            for (id, name) in &self.pools {
                                ui.selectable_value(&mut self.sid, *id, name);
                            }
                        });
                        if self.sid != before {
                            self.card = 0;
                            self.vlan = self.vlan && !self.pool_cards().cards.is_empty();
                        }
                    });
                }
                ui.horizontal(|ui| {
                    field_label(ui, "Name:", LABEL_WIDTH);
                    text_field(ui, pal, &mut self.name, ui.available_width(), false);
                });
                let pc = self.pool_cards();
                ui.horizontal(|ui| {
                    field_label(ui, "Kind:", LABEL_WIDTH);
                    match &editing {
                        // (what it is stays: a VLAN, a private one, a card's own)
                        Some(e) => {
                            let kind = match (card_own, e.was.vlan >= 0) {
                                (true, _) => format!("{}'s own network", e.was.device),
                                (_, true) => "VLAN on a network card".into(),
                                _ => "Private (no card)".into(),
                            };
                            ui.label(egui::RichText::new(kind).color(pal.ink));
                        }
                        None => {
                            let mut i = if self.vlan { 0 } else { 1 };
                            ui.add_enabled_ui(!pc.cards.is_empty(), |ui| {
                                if segmented(ui, pal, &["VLAN on a network card", "Private (no card)"], &mut i, ui.available_width()) {
                                    self.vlan = i == 0;
                                    self.problem.clear();
                                }
                            });
                        }
                    }
                });
                if self.vlan && !card_own {
                    ui.add_enabled_ui(!vlan_fixed, |ui| {
                        ui.horizontal(|ui| {
                            field_label(ui, "Card:", LABEL_WIDTH);
                            crate::newvm::combo_style(ui, pal);
                            let shown = pc.cards.get(self.card).map(|c| card_label(c)).unwrap_or_default();
                            crate::newvm::combo("net-card", shown, ui.available_width()).show_ui(ui, |ui| {
                                for (i, c) in pc.cards.iter().enumerate() {
                                    ui.selectable_value(&mut self.card, i, card_label(c));
                                }
                            });
                        });
                        ui.horizontal(|ui| {
                            field_label(ui, "VLAN:", LABEL_WIDTH);
                            if text_field(ui, pal, &mut self.tag, 90.0, false).changed() {
                                self.problem.clear();
                            }
                        });
                        let card = pc.cards.get(self.card).map(|c| c.0.as_str()).unwrap_or("");
                        let mine = |t: &(String, i64, String)| editing.as_ref().is_some_and(|e| t.0 == e.was.device && t.1 == e.was.vlan);
                        let mut on: Vec<String> = pc.taken.iter().filter(|t| t.0 == card && !mine(t)).map(|t| t.1.to_string()).collect();
                        on.sort_by_key(|t| t.parse::<i64>().unwrap_or(0));
                        let there = if on.is_empty() { String::new() } else { format!(" On {} already: {}.", card, on.join(", ")) };
                        note(ui, pal, &format!("1 to 4094; the switch port the card is plugged into must carry it (tagged).{}", there));
                    });
                    if let Some(e) = editing.as_ref().filter(|e| !e.running.is_empty()) {
                        let more = if e.running.len() > 3 { format!(" and {} more", e.running.len() - 3) } else { String::new() };
                        note(ui, pal, &format!("To change its VLAN, shut down the VMs on it first: {}{}.", e.running.iter().take(3).cloned().collect::<Vec<_>>().join(", "), more));
                    }
                } else if !card_own {
                    note(ui, pal, "Its VMs reach each other only on the same server; nothing outside.");
                }
                ui.horizontal(|ui| {
                    field_label(ui, "Description:", LABEL_WIDTH);
                    text_field(ui, pal, &mut self.description, ui.available_width(), false);
                });
                note(ui, pal, "For example its addresses: 192.168.10.0/24.");
                advanced(ui, pal, &mut self.advanced, &format!("MTU {}", self.mtu.trim()));
                if self.advanced {
                    ui.add_enabled_ui(!card_own, |ui| {
                        ui.horizontal(|ui| {
                            field_label(ui, "MTU:", LABEL_WIDTH);
                            if text_field(ui, pal, &mut self.mtu, 90.0, false).changed() {
                                self.problem.clear();
                            }
                        });
                    });
                    match &editing {
                        Some(e) if card_own => note(ui, pal, &format!("{}'s MTU is set with the card, on each server.", e.was.device)),
                        Some(e) if !e.running.is_empty() => note(ui, pal, "1500 usually; 9000 for jumbo frames. The VMs running on it get a new one when they start again."),
                        _ => note(ui, pal, "1500 usually; 9000 for jumbo frames, when every switch on the way takes them."),
                    }
                }
            });
            if !self.problem.is_empty() {
                ui.colored_label(pal.red, self.problem.as_str());
            }
            if let Some(what) = &self.busy {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label(egui::RichText::new(what.as_str()).color(pal.ink_soft));
                });
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            if let Some(f) = self.failure.as_mut() {
                failure_panel(ui, pal, f);
            }
            ui.add_space(4.0);
            dialog_buttons(ui, |ui| {
                // while the server's at it there's nothing to cancel: the window closes, it goes on
                if dialog_button(ui, pal, "close", if busy { "Close" } else { "Cancel" }, Look::Plain, true) {
                    action = NetworkAction::Close;
                }
                let ready = !busy && !self.name.trim().is_empty() && (!self.vlan || !self.tag.trim().is_empty()) && self.changed();
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let (glyph, label) = if editing.is_some() { ("check", "Save") } else { ("plus", "Create") };
                if dialog_button(ui, pal, glyph, label, Look::Primary, ready) || (ready && enter && self.failure.is_none()) {
                    action = self.submit();
                }
            });
        });
        action
    }
}

/// A card as the list shows it: its name, and the network on it now.
fn card_label(c: &(String, String)) -> String {
    if c.1.is_empty() {
        c.0.clone()
    } else {
        format!("{}  ·  {}", c.0, c.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog() -> NetworkDialog {
        let cards = vec![PoolCards { sid: 1, cards: vec![("eth0".into(), "Pool-wide network 0".into()), ("eth1".into(), "Storage".into())], taken: vec![("eth0".into(), 10, "Office".into())] }];
        NetworkDialog::new(1, vec![(1, "home".into())], cards)
    }

    #[test]
    fn what_is_sent() {
        let mut d = dialog().filled(" VNET-iot ", "30", "192.168.30.0/24");
        let NetworkAction::Create(sid, nw) = d.submit() else { panic!("not sent") };
        assert_eq!(sid, 1);
        assert_eq!(nw, NewNetwork { name: "VNET-iot".into(), description: "192.168.30.0/24".into(), mtu: 1500, vlan: Some(("eth0".into(), 30)) });
        // a private one
        let mut d = dialog().filled("Lab", "", "");
        d.vlan = false;
        let NetworkAction::Create(_, nw) = d.submit() else { panic!("not sent") };
        assert_eq!(nw.vlan, None);
    }

    #[test]
    fn what_an_edit_sends() {
        let was = NetInfo { rref: "net-1".into(), uuid: "u".into(), name: "Office".into(), description: String::new(), bridge: "xapi0".into(), mtu: 1500, device: "eth0".into(), vlan: 10 };
        let edit = |running: Vec<String>| NetworkDialog::edit(1, vec![(1, "home".into())], dialog().cards, Editing { was: was.clone(), running });
        // nothing changed: nothing to save
        let mut d = edit(vec![]);
        assert!(!d.changed());
        // a new name: only that (its own VLAN 10 isn't "taken")
        d.name = "Office LAN".into();
        let NetworkAction::Edit(_, x) = d.submit() else { panic!("not sent") };
        assert_eq!((x.name.as_str(), x.mtu, x.vlan.clone(), x.replug), ("Office LAN", None, None, true));
        // another tag and MTU
        let mut d = edit(vec![]);
        (d.tag, d.mtu) = ("20".into(), "9000".into());
        let NetworkAction::Edit(_, x) = d.submit() else { panic!("not sent") };
        assert_eq!((x.mtu, x.vlan), (Some(9000), Some(("eth0".into(), 20))));
        // not while a VM runs on it; its MTU waits for them
        let mut d = edit(vec!["dc01".into()]);
        d.tag = "20".into();
        assert!(matches!(d.submit(), NetworkAction::None));
        let mut d = edit(vec!["dc01".into()]);
        d.mtu = "9000".into();
        let NetworkAction::Edit(_, x) = d.submit() else { panic!("not sent") };
        assert_eq!((x.mtu, x.replug), (Some(9000), false));
        // a card's own network: its MTU stays
        let own = NetInfo { device: "eth1".into(), vlan: -1, ..was.clone() };
        let mut d = NetworkDialog::edit(1, vec![(1, "home".into())], dialog().cards, Editing { was: own, running: vec![] });
        d.mtu = "9000".into();
        assert!(!d.changed());
    }

    #[test]
    fn what_is_refused() {
        for (tag, why) in [("10", "eth0 has VLAN 10 already: 'Office'."), ("5000", "A VLAN tag goes from 1 to 4094."), ("x", "A VLAN tag goes from 1 to 4094.")] {
            let mut d = dialog().filled("New", tag, "");
            assert!(matches!(d.submit(), NetworkAction::None));
            assert_eq!(d.problem, why);
        }
        let mut d = dialog().filled("New", "20", "");
        d.mtu = "20000".into();
        assert!(matches!(d.submit(), NetworkAction::None));
        assert!(d.problem.starts_with("The MTU goes from 68"));
    }
}
