//! The New Network dialog: a network for VMs' cards, on a VLAN of a network card (made on every
//! server of the pool, on the card of that name) or private (no card: it joins each server's VMs
//! among themselves). What's needed comes first; the MTU waits under Advanced. A failure is said
//! in plain words, with what was asked and the server's answer.

use crate::app::{dialog_button, dialog_buttons, dialog_window, field_label, text_field, Look, LABEL_WIDTH};
use crate::conn::ApiError;
use crate::isolib::{advanced, failure_panel, note, segmented, Failure};
use crate::model::NewNetwork;
use crate::theme::Pal;
use eframe::egui;
use std::time::Duration;

pub enum NetworkAction {
    None,
    Close,
    /// Make it in the pool with this id.
    Create(u32, NewNetwork),
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
        };
        // a pool without cards to tag can only have private ones
        d.vlan = !d.pool_cards().cards.is_empty();
        d
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
        dialog_window(ctx, "New Network").show(ctx, |ui| {
            ui.set_width(480.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            let intro = "A network for your VMs' cards. On a VLAN of a network card it reaches what that card is plugged into, on every server of the pool; a private one joins only each server's VMs among themselves.";
            ui.add(egui::Label::new(egui::RichText::new(intro).color(pal.ink_soft)).wrap());
            ui.add_space(2.0);
            ui.add_enabled_ui(!busy, |ui| {
                if self.pools.len() > 1 {
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
                    let mut i = if self.vlan { 0 } else { 1 };
                    ui.add_enabled_ui(!pc.cards.is_empty(), |ui| {
                        if segmented(ui, pal, &["VLAN on a network card", "Private (no card)"], &mut i, ui.available_width()) {
                            self.vlan = i == 0;
                            self.problem.clear();
                        }
                    });
                });
                if self.vlan {
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
                    let mut on: Vec<String> = pc.taken.iter().filter(|t| t.0 == card).map(|t| t.1.to_string()).collect();
                    on.sort_by_key(|t| t.parse::<i64>().unwrap_or(0));
                    let there = if on.is_empty() { String::new() } else { format!(" On {} already: {}.", card, on.join(", ")) };
                    note(ui, pal, &format!("1 to 4094; the switch port the card is plugged into must carry it (tagged).{}", there));
                } else {
                    note(ui, pal, "Its VMs reach each other only on the same server; nothing outside.");
                }
                ui.horizontal(|ui| {
                    field_label(ui, "Description:", LABEL_WIDTH);
                    text_field(ui, pal, &mut self.description, ui.available_width(), false);
                });
                note(ui, pal, "For example its addresses: 192.168.10.0/24.");
                advanced(ui, pal, &mut self.advanced, &format!("MTU {}", self.mtu.trim()));
                if self.advanced {
                    ui.horizontal(|ui| {
                        field_label(ui, "MTU:", LABEL_WIDTH);
                        if text_field(ui, pal, &mut self.mtu, 90.0, false).changed() {
                            self.problem.clear();
                        }
                    });
                    note(ui, pal, "1500 usually; 9000 for jumbo frames, when every switch on the way takes them.");
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
                let ready = !busy && !self.name.trim().is_empty() && (!self.vlan || !self.tag.trim().is_empty());
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                if dialog_button(ui, pal, "plus", "Create", Look::Primary, ready) || (ready && enter && self.failure.is_none()) {
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
