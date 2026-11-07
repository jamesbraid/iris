use crate::i18n::{subtitle, ResponseExt};
use eframe::egui::Ui;
use crate::placement_ui::{self, Device, Media};
use iris::config::MachineConfig;
use std::path::Path;

/// What the user picked from a SCSI submenu, deferred for the App to act on
/// (so we don't hold &mut MachineConfig across nested closures and dialogs).
pub enum ScsiAction {
    None,
    AttachHdd { id: u8, path: String },
    AttachEmptyCdrom { id: u8 },
    AttachCdromWithDisc { id: u8, path: String },
    InsertDisc { id: u8, path: String },
    Eject { id: u8 },
    RemountInIrix { id: u8 },
    Detach { id: u8 },
    CreateBlank { id: u8 },
    ToggleOverlay { id: u8 },
}

/// Build the top-level "SCSI" menu. Returns at most one action per frame.
pub fn draw(ui: &mut Ui, cfg: &MachineConfig) -> ScsiAction {
    let mut action = ScsiAction::None;
    ui.set_min_width(280.0);
    for id in 1u8..=7 {
        let at = placement_ui::controller_zero_target(cfg, id);
        let found = placement_ui::media_at(cfg, &at);
        let dev = found.as_ref();
        let label = render_label(id, dev);
        ui.menu_button(label, |ui| {
            ui.set_min_width(220.0);
            match dev {
                None => {
                    if ui.button(t!("scsi-attach-hdd")).clicked() {
                        if let Some(p) = pick_disk(&t!("scsi-attach-hdd-picker"), "") {
                            action = ScsiAction::AttachHdd { id, path: p };
                        }
                        ui.close();
                    }
                    // Attaching a CD-ROM gives an empty drive by default; the
                    // user loads media afterwards via "Insert disc…". Mirrors
                    // real hardware and avoids an upfront file prompt.
                    if ui.button(t!("scsi-attach-cdrom-empty")).clicked() {
                        action = ScsiAction::AttachEmptyCdrom { id };
                        ui.close();
                    }
                    if ui.button(t!("scsi-attach-cdrom-disc")).clicked() {
                        if let Some(p) = pick_iso(&t!("scsi-attach-cdrom-disc-picker"), "") {
                            action = ScsiAction::AttachCdromWithDisc { id, path: p };
                        }
                        ui.close();
                    }
                    if ui.button(t!("scsi-create-blank")).clicked() {
                        action = ScsiAction::CreateBlank { id };
                        ui.close();
                    }
                }
                Some(d) if d.is_daynaport() => {
                    subtitle(ui, t!("scsi-daynaport-subtitle"));
                    ui.separator();
                    if ui.button(t!("scsi-detach-daynaport")).clicked() {
                        action = ScsiAction::Detach { id };
                        ui.close();
                    }
                }
                Some(d) if d.is_cdrom() => {
                    let has_media = !d.path().is_empty() && Path::new(d.path()).exists();
                    if has_media {
                        if ui.button(t!("scsi-eject")).clicked() {
                            action = ScsiAction::Eject { id };
                            ui.close();
                        }
                    }
                    let insert_label = if has_media { t!("scsi-change-disc") } else { t!("scsi-insert-disc") };
                    if ui.button(insert_label).clicked() {
                        if let Some(p) = pick_iso(&t!("scsi-insert-disc-picker"), d.path()) {
                            action = ScsiAction::InsertDisc { id, path: p };
                        }
                        ui.close();
                    }
                    if has_media {
                        if ui.button(t!("scsi-mount-in-irix")).tip(t!("scsi-mount-in-irix-tip")).clicked() {
                            action = ScsiAction::RemountInIrix { id };
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui.button(t!("scsi-detach-cdrom")).clicked() {
                        action = ScsiAction::Detach { id };
                        ui.close();
                    }
                }
                Some(d) => {
                    // HDD
                    let overlay_label = if d.overlay() {
                        t!("scsi-overlay-disable")
                    } else {
                        t!("scsi-overlay-enable")
                    };
                    if ui.button(overlay_label).tip(t!("scsi-overlay-tip")).clicked() {
                        action = ScsiAction::ToggleOverlay { id };
                        ui.close();
                    }
                    if ui.button(t!("scsi-replace-image")).clicked() {
                        if let Some(p) = pick_disk(&t!("scsi-replace-image-picker"), d.path()) {
                            action = ScsiAction::AttachHdd { id, path: p };
                        }
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(t!("scsi-detach-hdd")).clicked() {
                        action = ScsiAction::Detach { id };
                        ui.close();
                    }
                }
            }
        });
    }
    ui.separator();
    subtitle(ui, t!("scsi-menu-subtitle"));
    action
}

fn render_label(id: u8, dev: Option<&Media>) -> String {
    match dev {
        None => t!("scsi-slot-empty", id = id),
        Some(d) if d.is_daynaport() => t!("scsi-slot-daynaport", id = id),
        Some(d) if d.is_cdrom() => {
            if d.path().is_empty() {
                t!("scsi-slot-cdrom-empty", id = id)
            } else if !Path::new(d.path()).exists() {
                t!("scsi-slot-cdrom-missing", id = id, path = d.path())
            } else {
                let name = Path::new(d.path()).file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| d.path().to_owned());
                t!("scsi-slot-cdrom", id = id, name = name)
            }
        }
        Some(d) => {
            let name = Path::new(d.path()).file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| d.path().to_owned());
            let size = std::fs::metadata(d.path()).map(|m| m.len()).unwrap_or(0);
            let mb = size as f64 / (1024.0 * 1024.0);
            let size_mb = format!("{mb:.0}");
            match (size > 0, d.overlay()) {
                (true, false) => t!("scsi-slot-hdd-size", id = id, name = name, size = size_mb),
                (true, true) => t!("scsi-slot-hdd-size-overlay", id = id, name = name, size = size_mb),
                (false, false) => t!("scsi-slot-hdd", id = id, name = name),
                (false, true) => t!("scsi-slot-hdd-overlay", id = id, name = name),
            }
        }
    }
}

// Seed the picker at `cur`'s folder, else the managed disks dir — never the OS
// default. See `crate::filedialog`, which is where the "else" used to be a
// silent no-op and the panel opened at whatever the user last browsed.
fn dialog_at(title: &str, cur: &str) -> rfd::FileDialog {
    crate::filedialog::dialog(title, cur, crate::filedialog::Anchor::Disks,
                              crate::filedialog::Purpose::Open)
}

fn pick_disk(title: &str, cur: &str) -> Option<String> {
    dialog_at(title, cur)
        .add_filter(t!("scsi-filter-disk"), &["raw", "img", "chd"])
        .add_filter(t!("scsi-filter-all"), &["*"])
        .pick_file()
        .map(|p| crate::picked_path(&p))
}

pub fn pick_iso(title: &str, cur: &str) -> Option<String> {
    dialog_at(title, cur)
        .add_filter(t!("scsi-filter-iso"), &["iso", "chd"])
        .add_filter(t!("scsi-filter-all"), &["*"])
        .pick_file()
        .map(|p| crate::picked_path(&p))
}

/// Apply an action to the config.
pub fn apply(cfg: &mut MachineConfig, action: ScsiAction) -> Option<String> {
    let target = |cfg: &MachineConfig, id: u8| placement_ui::controller_zero_target(cfg, id);
    let attach = |cfg: &mut MachineConfig, id: u8, name: &str, path: String| {
        placement_ui::set_media(cfg, Media { at: target(cfg, id), dev: Device::new(name, path) });
    };
    let change = |cfg: &mut MachineConfig, id: u8, edit: &dyn Fn(&mut Media)| {
        let at = target(cfg, id);
        if let Some(mut m) = placement_ui::media_at(cfg, &at) {
            edit(&mut m);
            placement_ui::set_media(cfg, m);
        }
    };
    match action {
        ScsiAction::None => None,
        ScsiAction::AttachHdd { id, path } => {
            attach(cfg, id, "hdd", path);
            Some(t!("scsi-attached-hdd", id = id))
        }
        ScsiAction::AttachEmptyCdrom { id } => {
            attach(cfg, id, "cdrom", String::new());
            Some(t!("scsi-attached-cdrom-empty", id = id))
        }
        ScsiAction::AttachCdromWithDisc { id, path } => {
            attach(cfg, id, "cdrom", path);
            Some(t!("scsi-attached-cdrom-disc", id = id))
        }
        ScsiAction::InsertDisc { id, path } => {
            change(cfg, id, &|m| {
                if let Some(p) = m.path_mut() {
                    *p = path.clone();
                }
            });
            Some(t!("scsi-disc-inserted", id = id))
        }
        ScsiAction::Eject { id } => {
            change(cfg, id, &|m| {
                if let Some(p) = m.path_mut() {
                    p.clear();
                }
            });
            Some(t!("scsi-disc-ejected", id = id))
        }
        ScsiAction::Detach { id } => {
            let at = target(cfg, id);
            placement_ui::remove_media(cfg, &at);
            Some(t!("scsi-detached", id = id))
        }
        ScsiAction::CreateBlank { .. } => {
            // App opens the CreateDiskDialog; nothing to apply yet.
            None
        }
        ScsiAction::ToggleOverlay { id } => {
            let now = placement_ui::media_at(cfg, &target(cfg, id)).is_some_and(|m| !m.overlay());
            change(cfg, id, &|m| {
                if let Device::Hdd(h) = &mut m.dev {
                    h.overlay = !h.overlay;
                }
            });
            Some(if now { t!("scsi-overlay-on", id = id) } else { t!("scsi-overlay-off", id = id) })
        }
        ScsiAction::RemountInIrix { .. } => None,
    }
}
