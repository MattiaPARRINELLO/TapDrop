//! The window: what is paired, what is waiting, and the three settings that matter.
//!
//! It is a thin client of the control socket, so it can be opened and closed at will while the
//! service keeps running in the background.

use crate::control;
use gtk::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use serde_json::{json, Value};

pub fn run() -> anyhow::Result<()> {
    let app = gtk::Application::builder()
        .application_id("app.tapdrop.linux")
        .build();
    app.connect_activate(build);
    // Without this, GApplication would treat the "ui" argument as a file to open and quit.
    app.run_with_args::<&str>(&[]);
    Ok(())
}

fn build(app: &gtk::Application) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("TapDrop")
        .default_width(480)
        .default_height(560)
        .build();

    let header = gtk::HeaderBar::new();
    header.set_title_widget(Some(&gtk::Label::new(Some("TapDrop"))));
    window.set_titlebar(Some(&header));

    let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_markup("<b>Connexion…</b>");
    content.append(&status);

    content.append(&section("Téléphones"));
    let devices = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.append(&devices);

    content.append(&section("Appairage"));
    let pairing = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.append(&pairing);

    content.append(&section("Adresse pour le téléphone"));
    let address_hint = gtk::Label::new(Some(
        "Hors du même Wi-Fi : Tailscale sur les deux appareils, puis cette adresse dans TapDrop → Edit address.",
    ));
    address_hint.set_xalign(0.0);
    address_hint.set_wrap(true);
    address_hint.add_css_class("dim-label");
    content.append(&address_hint);
    let addresses = gtk::Box::new(gtk::Orientation::Vertical, 4);
    content.append(&addresses);

    content.append(&section("Réglages"));

    let name_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let name_entry = gtk::Entry::new();
    name_entry.set_hexpand(true);
    let rename = gtk::Button::with_label("Renommer");
    name_row.append(&name_entry);
    name_row.append(&rename);
    content.append(&name_row);

    let folder_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let folder_label = gtk::Label::new(None);
    folder_label.set_xalign(0.0);
    folder_label.set_hexpand(true);
    folder_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    let folder_button = gtk::Button::with_label("Changer…");
    folder_row.append(&folder_label);
    folder_row.append(&folder_button);
    content.append(&folder_row);

    let links_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let links_label = gtk::Label::new(Some("Ouvrir les liens dans le navigateur"));
    links_label.set_xalign(0.0);
    links_label.set_hexpand(true);
    let links_switch = gtk::Switch::new();
    links_row.append(&links_label);
    links_row.append(&links_switch);
    content.append(&links_row);

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&content));

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.set_margin_top(10);
    footer.set_margin_bottom(10);
    footer.set_margin_start(18);
    footer.set_margin_end(18);
    let clipboard = gtk::Button::with_label("Envoyer le presse-papiers");
    let quit = gtk::Button::with_label("Quitter TapDrop");
    quit.add_css_class("destructive-action");
    footer.append(&clipboard);
    footer.append(&quit);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&scroll);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&footer);
    window.set_child(Some(&root));

    clipboard.connect_clicked(|button| {
        button.set_sensitive(false);
        let answer = control::request_blocking(&json!({ "cmd": "clipboard" }));
        button.set_sensitive(true);
        match answer {
            Ok(v)
                if v["queued"].as_bool().unwrap_or(false)
                    || v["queued"].as_u64().unwrap_or(0) > 0 =>
            {
                button.set_label("Envoyé au téléphone");
            }
            Ok(_) => button.set_label("Presse-papiers vide"),
            Err(_) => button.set_label("Service injoignable"),
        }
    });

    quit.connect_clicked(|_| {
        let _ = control::request_blocking(&json!({ "cmd": "quit" }));
    });

    rename.connect_clicked({
        let entry = name_entry.clone();
        move |_| {
            let name = entry.text().to_string();
            let _ = control::request_blocking(&json!({ "cmd": "rename", "name": name }));
        }
    });

    links_switch.connect_state_set(|_, state| {
        let _ = control::request_blocking(&json!({ "cmd": "links", "enabled": state }));
        glib::Propagation::Proceed
    });

    folder_button.connect_clicked({
        let window = window.clone();
        let label = folder_label.clone();
        move |_| {
            let chooser = gtk::FileChooserNative::builder()
                .title("Dossier de réception")
                .action(gtk::FileChooserAction::SelectFolder)
                .transient_for(&window)
                .build();
            let label = label.clone();
            chooser.connect_response(move |chooser, response| {
                if response == gtk::ResponseType::Accept {
                    if let Some(path) = chooser.file().and_then(|f| f.path()) {
                        let path = path.display().to_string();
                        let _ =
                            control::request_blocking(&json!({ "cmd": "folder", "path": path }));
                        label.set_label(&path);
                    }
                }
                chooser.destroy();
            });
            chooser.show();
        }
    });

    let refresh = {
        let status = status.clone();
        let devices = devices.clone();
        let pairing = pairing.clone();
        let addresses = addresses.clone();
        let name_entry = name_entry.clone();
        let folder_label = folder_label.clone();
        let links_switch = links_switch.clone();
        move || {
            let Ok(state) = control::request_blocking(&json!({ "cmd": "status" })) else {
                status.set_markup(
                    "<b>Service arrêté</b>\nLance « tapdrop-linux run » dans un terminal.",
                );
                return;
            };
            let name = state["name"].as_str().unwrap_or("?");
            let port = state["port"].as_u64().unwrap_or(0);
            let tap = if state["notch"].as_bool().unwrap_or(false) {
                "tap prêt"
            } else {
                "tap indisponible"
            };
            status.set_markup(&format!("<b>{name}</b>, port {port}\n{tap}"));

            while let Some(child) = devices.first_child() {
                devices.remove(&child);
            }
            let list = state["devices"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                devices.append(&hint("Aucun téléphone appairé pour l'instant."));
            }
            for device in list {
                devices.append(&device_row(&device));
            }

            while let Some(child) = addresses.first_child() {
                addresses.remove(&child);
            }
            let list = state["addresses"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                addresses.append(&hint("Aucune adresse réseau."));
            }
            for address in list {
                addresses.append(&address_row(&address));
            }

            while let Some(child) = pairing.first_child() {
                pairing.remove(&child);
            }
            let pending = state["pending"].as_array().cloned().unwrap_or_default();
            if pending.is_empty() {
                pairing.append(&hint("Rien en attente."));
            }
            for request in pending {
                pairing.append(&pairing_row(&request));
            }

            if name_entry.text().is_empty() {
                name_entry.set_text(name);
            }
            folder_label.set_label(state["save_folder"].as_str().unwrap_or("?"));
            links_switch.set_active(state["open_links"].as_bool().unwrap_or(true));
        }
    };

    refresh();
    glib::timeout_add_seconds_local(2, move || {
        refresh();
        glib::ControlFlow::Continue
    });

    window.present();
}

fn section(title: &str) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_xalign(0.0);
    label.set_margin_top(6);
    label.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(title)));
    label
}

fn hint(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("dim-label");
    label
}

fn device_row(device: &Value) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let name = gtk::Label::new(Some(device["name"].as_str().unwrap_or("Téléphone")));
    name.set_xalign(0.0);
    name.set_hexpand(true);
    let forget = gtk::Button::with_label("Oublier");
    forget.add_css_class("flat");
    let id = device["id"].as_str().unwrap_or_default().to_string();
    forget.connect_clicked(move |_| {
        let _ = control::request_blocking(&json!({ "cmd": "forget", "device": id }));
    });
    row.append(&name);
    row.append(&forget);
    row.upcast()
}

fn address_row(address: &Value) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let endpoint = address["endpoint"].as_str().unwrap_or("?").to_string();
    let label = gtk::Label::new(Some(&format!(
        "{}  ·  {}",
        endpoint,
        address["kind"].as_str().unwrap_or("réseau")
    )));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_selectable(true);
    let copy = gtk::Button::with_label("Copier");
    copy.add_css_class("flat");
    copy.connect_clicked(move |_| {
        let _ = crate::clipboard::write_text(&endpoint);
    });
    row.append(&label);
    row.append(&copy);
    row.upcast()
}

fn pairing_row(request: &Value) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let text = gtk::Label::new(Some(&format!(
        "{} demande le code {}",
        request["device"].as_str().unwrap_or("Un téléphone"),
        request["code"].as_str().unwrap_or("?")
    )));
    text.set_xalign(0.0);
    text.set_hexpand(true);

    let code = request["code"].as_str().unwrap_or_default().to_string();
    let accept = gtk::Button::with_label("Accepter");
    accept.add_css_class("suggested-action");
    accept.connect_clicked({
        let code = code.clone();
        move |_| {
            let _ = control::request_blocking(&json!({ "cmd": "approve", "code": code }));
        }
    });
    let refuse = gtk::Button::with_label("Refuser");
    refuse.connect_clicked(move |_| {
        let _ = control::request_blocking(&json!({ "cmd": "deny", "code": code }));
    });

    row.append(&text);
    row.append(&accept);
    row.append(&refuse);
    row.upcast()
}
