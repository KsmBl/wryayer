//! Which GPU a sandboxed app renders on.
//!
//! A laptop with two GPUs runs everything on the integrated one unless the app
//! is told otherwise, and the way to tell it differs per driver stack: Mesa
//! reads `DRI_PRIME`, its Vulkan drivers read `MESA_VK_DEVICE_SELECT`, and the
//! NVIDIA stack ignores both in favour of its own `__NV_PRIME_*` /
//! `__GLX_VENDOR_LIBRARY_NAME` trio. wryayer already owns the environment its
//! apps start in, so the choice belongs here as one setting rather than in
//! whatever wrapper script the user would otherwise write per app.
//!
//! Everything is read from sysfs: no root, no probing, nothing that needs the
//! GPU to be idle.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Who made the card. Only used to pick the right environment variables — the
/// driver stack, not the marketing name, is what differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Amd,
    Intel,
    Nvidia,
    Other,
}

impl Vendor {
    fn from_pci_id(id: u16) -> Self {
        match id {
            0x1002 | 0x1022 => Self::Amd,
            0x8086 => Self::Intel,
            0x10de => Self::Nvidia,
            _ => Self::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Amd => "AMD",
            Self::Intel => "Intel",
            Self::Nvidia => "NVIDIA",
            Self::Other => "GPU",
        }
    }
}

/// One DRM device, as the config file and the launcher see it.
#[derive(Debug, Clone, PartialEq)]
pub struct Gpu {
    /// Stable identifier, and the value stored in config.ini — the PCI address
    /// in the shape Mesa's `DRI_PRIME` wants, e.g. `pci-0000_03_00_0`. Stable
    /// across reboots, unlike the card number, which follows probe order.
    pub id: String,
    /// The sysfs/devnode name, e.g. `card1`.
    pub card: String,
    /// Human-readable model, from the PCI ID database when it is installed.
    pub name: String,
    pub vendor: Vendor,
    pub vendor_id: u16,
    pub device_id: u16,
    /// Kernel driver bound to it (`amdgpu`, `i915`, `xe`, `nvidia`, …).
    pub driver: Option<String>,
    /// `/dev/dri/cardN` — the primary node, used by the display server.
    pub primary: PathBuf,
    /// `/dev/dri/renderDN` — the render node apps actually draw through.
    /// Absent for display-only devices.
    pub render: Option<PathBuf>,
    /// Whether one of this card's connectors has a display plugged into it.
    /// On a hybrid laptop that is the integrated GPU: it drives the panel, and
    /// everything else presents through it.
    pub drives_display: bool,
}

impl Gpu {
    /// Short label for menus and one-line listings.
    pub fn label(&self) -> String {
        format!("{} {}", self.vendor.label(), self.name)
    }

    /// Whether `needle` names this GPU. Accepts the stored id, the card name,
    /// the kernel driver, the vendor, or any substring of the model name, so a
    /// user can write `wryayer config foo gpu nvidia` and be understood.
    pub fn matches(&self, needle: &str) -> bool {
        let n = needle.trim().to_lowercase();
        if n.is_empty() {
            return false;
        }
        self.id.to_lowercase() == n
            || self.card == n
            || self.driver.as_deref().is_some_and(|d| d.to_lowercase() == n)
            || self.vendor.label().to_lowercase() == n
            || self.name.to_lowercase().contains(&n)
    }
}

/// Every GPU on the machine, in card order. Scanned once per process: a card
/// cannot appear or leave while an app is starting, and this is read from the
/// TUI's render loop.
pub fn all() -> &'static [Gpu] {
    static GPUS: OnceLock<Vec<Gpu>> = OnceLock::new();
    GPUS.get_or_init(|| scan(Path::new("/sys/class/drm"), Path::new("/dev/dri")))
}

/// The GPU a stored config value refers to, or None for "let the driver pick".
pub fn resolve(value: Option<&str>) -> Option<&'static Gpu> {
    let want = value?.trim();
    if want.is_empty() || want == "auto" {
        return None;
    }
    all().iter().find(|g| g.matches(want))
}

/// Read the DRM devices under `drm_dir`, pairing each card with its render node.
///
/// Takes its roots as arguments so the parsing can be tested against a fixture
/// tree instead of the developer's own hardware.
pub fn scan(drm_dir: &Path, dev_dir: &Path) -> Vec<Gpu> {
    let Ok(entries) = std::fs::read_dir(drm_dir) else {
        return Vec::new();
    };
    // `card1-HDMI-A-1` and friends are connectors, not devices: the name of a
    // card is "card" followed by digits and nothing else.
    let mut cards: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let digits = name.strip_prefix("card")?;
            (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())).then_some(name)
        })
        .collect();
    cards.sort();

    let render_nodes = render_nodes(drm_dir);
    cards
        .iter()
        .filter_map(|card| {
            let device = drm_dir.join(card).join("device");
            let vendor_id = read_hex(&device.join("vendor"))?;
            let device_id = read_hex(&device.join("device"))?;
            let slot = pci_slot(&device)?;
            let render = render_nodes
                .iter()
                .find(|(_, dev)| *dev == std::fs::canonicalize(&device).ok())
                .map(|(node, _)| dev_dir.join(node));
            Some(Gpu {
                id: slot_to_id(&slot),
                card: card.clone(),
                name: model_name(vendor_id, device_id),
                vendor: Vendor::from_pci_id(vendor_id),
                vendor_id,
                device_id,
                driver: std::fs::read_link(device.join("driver"))
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
                primary: dev_dir.join(card),
                render,
                drives_display: has_connected_connector(drm_dir, card),
            })
        })
        .collect()
}

/// The render nodes under `drm_dir`, each paired with the PCI device it belongs
/// to, so a card can claim its own.
fn render_nodes(drm_dir: &Path) -> Vec<(String, Option<PathBuf>)> {
    let Ok(entries) = std::fs::read_dir(drm_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with("renderD") {
                return None;
            }
            let dev = std::fs::canonicalize(drm_dir.join(&name).join("device")).ok();
            Some((name, dev))
        })
        .collect()
}

/// Whether any of `card`'s connectors reports a display plugged in.
///
/// Connectors are sysfs siblings of the card named `<card>-<connector>`
/// (`card0-eDP-1`), each with a `status` file reading `connected` or
/// `disconnected`.
fn has_connected_connector(drm_dir: &Path, card: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(drm_dir) else {
        return false;
    };
    let prefix = format!("{card}-");
    entries.flatten().any(|e| {
        e.file_name().to_string_lossy().starts_with(&prefix)
            && std::fs::read_to_string(e.path().join("status"))
                .is_ok_and(|s| s.trim() == "connected")
    })
}

/// Read a sysfs file holding `0x1002`-style hex.
fn read_hex(path: &Path) -> Option<u16> {
    let raw = std::fs::read_to_string(path).ok()?;
    let raw = raw.trim();
    u16::from_str_radix(raw.strip_prefix("0x").unwrap_or(raw), 16).ok()
}

/// The PCI address of a device, e.g. `0000:03:00.0`.
///
/// Read from `uevent` rather than taken from the directory name: a DRM device
/// need not sit directly under its PCI node (Tegra and some USB displaylink
/// devices do not), and `uevent` says so explicitly when it does.
fn pci_slot(device: &Path) -> Option<String> {
    let uevent = std::fs::read_to_string(device.join("uevent")).ok()?;
    uevent
        .lines()
        .find_map(|l| l.strip_prefix("PCI_SLOT_NAME=").map(str::to_owned))
        .or_else(|| {
            std::fs::canonicalize(device)
                .ok()?
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
}

/// `0000:03:00.0` → `pci-0000_03_00_0`, the spelling Mesa's `DRI_PRIME` parses.
fn slot_to_id(slot: &str) -> String {
    format!("pci-{}", slot.replace([':', '.'], "_"))
}

/// Look the card up in the system PCI ID database, falling back to the raw id.
///
/// hwdata is not guaranteed to be installed, and a card newer than the local
/// copy is missing from it either way, so the fallback has to read acceptably
/// on its own: "Intel device 3ea0" still tells the user which row is which.
fn model_name(vendor_id: u16, device_id: u16) -> String {
    pci_ids_lookup(Path::new(PCI_IDS), vendor_id, device_id)
        .unwrap_or_else(|| format!("device {device_id:04x}"))
}

const PCI_IDS: &str = "/usr/share/hwdata/pci.ids";

/// Find a device's name in a `pci.ids` file.
///
/// The format is two levels of indentation: a vendor line at column 0, then its
/// devices indented by one tab. So the scan runs to the wanted vendor, then
/// reads its indented block until the next unindented line.
fn pci_ids_lookup(path: &Path, vendor_id: u16, device_id: u16) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let vendor_key = format!("{vendor_id:04x}");
    let device_key = format!("\t{device_id:04x}");
    let mut in_vendor = false;
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if !line.starts_with('\t') {
            if in_vendor {
                return None; // past our vendor's block
            }
            in_vendor = line.starts_with(&vendor_key);
            continue;
        }
        if in_vendor && !line.starts_with("\t\t") {
            if let Some(rest) = line.strip_prefix(&device_key) {
                return Some(rest.trim().to_owned());
            }
        }
    }
    None
}

/// The GPU row's menu entries: "auto" first, then one per detected GPU, in the
/// order [`all`] reports them.
///
/// Leaked once because the settings screens type their option lists as
/// `&'static str` — the hardware cannot change under a running process, so a
/// one-time leak buys the whole UI the strings it needs without threading a
/// lifetime through every row helper.
pub fn menu_labels() -> &'static [&'static str] {
    static LABELS: OnceLock<Vec<&'static str>> = OnceLock::new();
    LABELS.get_or_init(|| {
        let mut v = vec!["auto"];
        v.extend(
            all()
                .iter()
                .map(|g| &*Box::leak(g.label().into_boxed_str()) as &'static str),
        );
        v
    })
}

/// One line per [`menu_labels`] entry, explaining what picking it does.
pub fn menu_descriptions() -> &'static [&'static str] {
    static DESCRIPTIONS: OnceLock<Vec<&'static str>> = OnceLock::new();
    DESCRIPTIONS.get_or_init(|| {
        let mut v = vec![
            "auto — Let the driver choose. On a hybrid machine that is the \
             integrated GPU, whatever else is installed.",
        ];
        v.extend(all().iter().map(|g| {
            let line = format!(
                "{} — {}, driver {}. The app renders here; the render nodes of other GPUs are hidden from its sandbox, except the one driving your screen, which it still presents through.",
                g.label(),
                g.id,
                g.driver.as_deref().unwrap_or("none"),
            );
            &*Box::leak(line.into_boxed_str()) as &'static str
        }));
        v
    })
}

/// The environment that points a process at `gpu`.
///
/// `others` is the rest of the machine's GPUs: what has to be said depends on
/// what else is present, because the NVIDIA stack only steps aside when it is
/// told to, and only understands being told in its own variables.
pub fn env_for(gpu: &Gpu, others: &[Gpu]) -> Vec<(String, String)> {
    let mut env = vec![
        // Mesa's GL/GLES device selection.
        ("DRI_PRIME".to_string(), gpu.id.clone()),
        // Mesa's Vulkan device selection, which DRI_PRIME does not cover.
        (
            "MESA_VK_DEVICE_SELECT".to_string(),
            format!("{:04x}:{:04x}", gpu.vendor_id, gpu.device_id),
        ),
    ];
    let nvidia_present = std::iter::once(gpu)
        .chain(others)
        .any(|g| g.vendor == Vendor::Nvidia);
    if !nvidia_present {
        return env;
    }
    let want_nvidia = gpu.vendor == Vendor::Nvidia;
    if want_nvidia {
        // The PRIME render-offload handshake: without the first two, a GLX app
        // keeps rendering on whichever GPU drives the display.
        env.push(("__NV_PRIME_RENDER_OFFLOAD".to_string(), "1".to_string()));
        env.push(("__GLX_VENDOR_LIBRARY_NAME".to_string(), "nvidia".to_string()));
        env.push(("__VK_LAYER_NV_optimus".to_string(), "NVIDIA_only".to_string()));
    } else {
        // Keep libglvnd on Mesa, and hide the NVIDIA card from the Vulkan
        // loader, so an app that enumerates devices itself cannot pick it.
        env.push(("__GLX_VENDOR_LIBRARY_NAME".to_string(), "mesa".to_string()));
        env.push((
            "__VK_LAYER_NV_optimus".to_string(),
            "non_NVIDIA_only".to_string(),
        ));
    }
    env
}

/// Render nodes to hide from a sandbox that is pinned to `gpu`.
///
/// The environment above is a request, not a fence: an app that opens
/// `/dev/dri/renderD*` itself — Chromium and anything built on it does — can
/// still land on the other card. Masking the nodes it must not use makes the
/// setting mean what it says.
///
/// Primary nodes (`cardN`) are deliberately left alone: they are how the
/// display server hands out buffers, and an app that is merely rendering
/// elsewhere still has to present through them.
///
/// Nor is a card that drives a display ever masked, even when the app renders
/// on another one. A client on a hybrid machine allocates and shares buffers
/// through the compositor's card — that is what dma-buf feedback names — and
/// taking its render node away does not move the work to the chosen GPU, it
/// drops the app to software rendering.
pub fn nodes_to_mask(gpu: &Gpu, others: &[Gpu]) -> Vec<PathBuf> {
    if gpu.render.is_none() {
        // Nothing to pin to, so masking would only remove options.
        return Vec::new();
    }
    others
        .iter()
        .filter(|g| g.id != gpu.id && !g.drives_display)
        .filter_map(|g| g.render.clone())
        .collect()
}

// ── The NVIDIA userspace stack ────────────────────────────────────────────────
//
// An app tree is its own /usr, filled once at install time. That is fine for
// Mesa, whose drivers talk to a stable kernel interface, and wrong for NVIDIA:
// its userspace libraries must match the running kernel module build exactly,
// and the module belongs to the host. So an app installed before the last
// driver update carries a libGLX_nvidia.so that refuses to initialise, GL falls
// back to llvmpipe, and the symptom is an idle GPU next to a pegged CPU.
//
// The fix is the one Flatpak arrived at: take the userspace stack from the
// host, next to the module it has to match, rather than from the tree.

/// Directories a distribution keeps its shared libraries in.
const LIB_DIRS: &[&str] = &[
    "/usr/lib",
    // 32-bit, for the wine games wryayer installs into their own containers.
    "/usr/lib32",
    // Debian/Ubuntu multiarch, which wryayer also installs onto.
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib/i386-linux-gnu",
];

/// Library name prefixes that belong to the NVIDIA driver.
const NVIDIA_LIBS: &[&str] = &[
    "libGLX_nvidia.so",
    "libEGL_nvidia.so",
    "libGLESv1_CM_nvidia.so",
    "libGLESv2_nvidia.so",
    "libnvidia-",
    "libcuda.so",
    "libnvcuvid.so",
    "libnvoptix.so",
    "libvdpau_nvidia.so",
];

/// Driver manifests: the files that tell libglvnd, the Vulkan loader and the
/// EGL platform machinery that an NVIDIA driver exists at all.
const NVIDIA_MANIFESTS: &[&str] = &[
    "/usr/share/glvnd/egl_vendor.d/10_nvidia.json",
    "/usr/share/vulkan/icd.d/nvidia_icd.json",
    "/usr/share/vulkan/implicit_layer.d/nvidia_layers.json",
    "/usr/share/egl/egl_external_platform.d/10_nvidia_wayland.json",
    "/usr/share/egl/egl_external_platform.d/15_nvidia_gbm.json",
    "/etc/OpenCL/vendors/nvidia.icd",
];

/// The host's NVIDIA driver files, to bind over whatever the app tree has.
///
/// Empty when the host has no NVIDIA driver installed — on such a machine there
/// is nothing to match, and an app tree's nvidia libraries are dead weight
/// either way.
pub fn nvidia_host_files() -> &'static [PathBuf] {
    static FILES: OnceLock<Vec<PathBuf>> = OnceLock::new();
    FILES.get_or_init(|| nvidia_files_in(LIB_DIRS, NVIDIA_MANIFESTS))
}

/// The scan behind [`nvidia_host_files`], with its roots given so it can be
/// tested against a fixture instead of the developer's own driver install.
fn nvidia_files_in(lib_dirs: &[&str], manifests: &[&str]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in lib_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Both the sonames and the versioned files behind them: a library
            // is opened by soname, but its own DT_NEEDED entries name the
            // versioned files, so binding one without the other loads nothing.
            if NVIDIA_LIBS.iter().any(|p| name.starts_with(p)) {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    files.extend(
        manifests
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.exists()),
    );
    files
}

/// Whether this machine has an NVIDIA card at all.
pub fn nvidia_present() -> bool {
    all().iter().any(|g| g.vendor == Vendor::Nvidia)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// Build a fake /sys/class/drm + /dev/dri pair holding one PCI card.
    fn fixture(root: &Path, card: &str, render: Option<&str>, slot: &str, vendor: &str, device: &str) {
        let drm = root.join("sys/class/drm");
        let pci = root.join("sys/devices").join(slot);
        write(&pci.join("vendor"), &format!("{vendor}\n"));
        write(&pci.join("device"), &format!("{device}\n"));
        write(&pci.join("uevent"), &format!("DRIVER=testdrv\nPCI_SLOT_NAME={slot}\n"));
        std::fs::create_dir_all(drm.join(card)).unwrap();
        std::os::unix::fs::symlink(&pci, drm.join(card).join("device")).unwrap();
        if let Some(r) = render {
            std::fs::create_dir_all(drm.join(r)).unwrap();
            std::os::unix::fs::symlink(&pci, drm.join(r).join("device")).unwrap();
        }
        std::fs::create_dir_all(root.join("dev/dri")).unwrap();
    }

    /// Give `card` a connector, plugged in or not.
    fn connector(root: &Path, card: &str, name: &str, status: &str) {
        let dir = root.join("sys/class/drm").join(format!("{card}-{name}"));
        write(&dir.join("status"), &format!("{status}\n"));
    }

    #[test]
    fn a_card_is_found_with_its_render_node() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fixture(root, "card1", Some("renderD128"), "0000:00:02.0", "0x8086", "0x3ea0");
        let gpus = scan(&root.join("sys/class/drm"), &root.join("dev/dri"));
        assert_eq!(gpus.len(), 1, "{gpus:?}");
        let g = &gpus[0];
        assert_eq!(g.id, "pci-0000_00_02_0");
        assert_eq!(g.vendor, Vendor::Intel);
        assert_eq!(g.render, Some(root.join("dev/dri/renderD128")));
        assert_eq!(g.primary, root.join("dev/dri/card1"));
    }

    #[test]
    fn connectors_are_not_cards() {
        // card1-HDMI-A-1 is an output on card1, not a second GPU — counting it
        // would offer the user a GPU that cannot render anything.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fixture(root, "card1", None, "0000:00:02.0", "0x8086", "0x3ea0");
        std::fs::create_dir_all(root.join("sys/class/drm/card1-HDMI-A-1")).unwrap();
        let gpus = scan(&root.join("sys/class/drm"), &root.join("dev/dri"));
        assert_eq!(gpus.len(), 1, "{gpus:?}");
    }

    #[test]
    fn each_card_keeps_its_own_render_node() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fixture(root, "card0", Some("renderD128"), "0000:00:02.0", "0x8086", "0x3ea0");
        fixture(root, "card1", Some("renderD129"), "0000:01:00.0", "0x10de", "0x2520");
        let gpus = scan(&root.join("sys/class/drm"), &root.join("dev/dri"));
        assert_eq!(gpus.len(), 2, "{gpus:?}");
        assert_eq!(gpus[0].render, Some(root.join("dev/dri/renderD128")));
        assert_eq!(gpus[1].render, Some(root.join("dev/dri/renderD129")));
        assert_eq!(gpus[1].vendor, Vendor::Nvidia);
    }

    #[test]
    fn the_other_cards_render_nodes_are_the_ones_masked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fixture(root, "card0", Some("renderD128"), "0000:00:02.0", "0x8086", "0x3ea0");
        fixture(root, "card1", Some("renderD129"), "0000:01:00.0", "0x10de", "0x2520");
        let gpus = scan(&root.join("sys/class/drm"), &root.join("dev/dri"));
        let masked = nodes_to_mask(&gpus[1], &gpus);
        assert_eq!(masked, vec![root.join("dev/dri/renderD128")]);
    }

    #[test]
    fn a_display_only_device_masks_nothing() {
        // No render node of its own: pinning to it can only take options away.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fixture(root, "card0", None, "0000:00:02.0", "0x8086", "0x3ea0");
        fixture(root, "card1", Some("renderD129"), "0000:01:00.0", "0x10de", "0x2520");
        let gpus = scan(&root.join("sys/class/drm"), &root.join("dev/dri"));
        assert!(nodes_to_mask(&gpus[0], &gpus).is_empty());
    }

    #[test]
    fn nvidia_gets_the_offload_handshake_and_mesa_gets_told_to_step_aside() {
        let intel = Gpu {
            id: "pci-0000_00_02_0".into(), card: "card0".into(), name: "UHD".into(),
            vendor: Vendor::Intel, vendor_id: 0x8086, device_id: 0x3ea0,
            driver: None, primary: "/dev/dri/card0".into(), render: None,
            drives_display: false,
        };
        let nvidia = Gpu {
            id: "pci-0000_01_00_0".into(), card: "card1".into(), name: "RTX".into(),
            vendor: Vendor::Nvidia, vendor_id: 0x10de, device_id: 0x2520,
            driver: None, primary: "/dev/dri/card1".into(), render: None,
            drives_display: false,
        };
        let both = [intel.clone(), nvidia.clone()];

        let env = env_for(&nvidia, &both);
        assert!(env.contains(&("__NV_PRIME_RENDER_OFFLOAD".into(), "1".into())), "{env:?}");
        assert!(env.contains(&("__GLX_VENDOR_LIBRARY_NAME".into(), "nvidia".into())), "{env:?}");

        let env = env_for(&intel, &both);
        assert!(env.contains(&("__GLX_VENDOR_LIBRARY_NAME".into(), "mesa".into())), "{env:?}");
        assert!(env.contains(&("DRI_PRIME".into(), "pci-0000_00_02_0".into())), "{env:?}");
        assert!(env.contains(&("MESA_VK_DEVICE_SELECT".into(), "8086:3ea0".into())), "{env:?}");
    }

    #[test]
    fn an_all_mesa_machine_is_not_told_about_nvidia() {
        // Setting __GLX_VENDOR_LIBRARY_NAME where no NVIDIA driver exists points
        // libglvnd at a vendor library that isn't there.
        let amd = Gpu {
            id: "pci-0000_03_00_0".into(), card: "card0".into(), name: "RX".into(),
            vendor: Vendor::Amd, vendor_id: 0x1002, device_id: 0x73df,
            driver: None, primary: "/dev/dri/card0".into(), render: None,
            drives_display: false,
        };
        let env = env_for(&amd, std::slice::from_ref(&amd));
        assert!(!env.iter().any(|(k, _)| k.starts_with("__")), "{env:?}");
    }

    #[test]
    fn a_gpu_answers_to_its_id_card_driver_vendor_and_model() {
        let g = Gpu {
            id: "pci-0000_01_00_0".into(), card: "card1".into(),
            name: "GA107M [GeForce RTX 3050 Mobile]".into(),
            vendor: Vendor::Nvidia, vendor_id: 0x10de, device_id: 0x2520,
            driver: Some("nvidia".into()), primary: "/dev/dri/card1".into(), render: None,
            drives_display: false,
        };
        for needle in ["pci-0000_01_00_0", "card1", "nvidia", "NVIDIA", "rtx 3050"] {
            assert!(g.matches(needle), "should match '{needle}'");
        }
        for needle in ["card0", "amd", "", "  "] {
            assert!(!g.matches(needle), "should not match '{needle}'");
        }
    }

    #[test]
    fn a_model_name_comes_from_the_pci_id_database() {
        let tmp = tempfile::tempdir().unwrap();
        let ids = tmp.path().join("pci.ids");
        std::fs::write(
            &ids,
            "# comment\n\
             1002  Advanced Micro Devices, Inc. [AMD/ATI]\n\
             \t73df  Navi 22 [Radeon RX 6700 XT]\n\
             \t\t1043 0000  Sub-device\n\
             8086  Intel Corporation\n\
             \t3ea0  WhiskeyLake-U GT2 [UHD Graphics 620]\n",
        )
        .unwrap();
        assert_eq!(
            pci_ids_lookup(&ids, 0x1002, 0x73df).as_deref(),
            Some("Navi 22 [Radeon RX 6700 XT]")
        );
        assert_eq!(
            pci_ids_lookup(&ids, 0x8086, 0x3ea0).as_deref(),
            Some("WhiskeyLake-U GT2 [UHD Graphics 620]")
        );
        // A card the local database has never heard of.
        assert_eq!(pci_ids_lookup(&ids, 0x8086, 0xffff), None);
    }

    #[test]
    fn an_unknown_card_still_gets_a_usable_name() {
        let name = model_name(0x8086, 0xfffe);
        assert!(name.contains("fffe"), "{name}");
    }

    #[test]
    fn the_card_driving_the_screen_is_never_masked() {
        // The regression this rule exists for: pinning a hybrid laptop to its
        // discrete card masked the integrated one, which is the card the
        // compositor allocates and shares buffers through — the app dropped to
        // software rendering instead of moving to the dGPU.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fixture(root, "card0", Some("renderD128"), "0000:00:02.0", "0x8086", "0x3ea0");
        fixture(root, "card1", Some("renderD129"), "0000:01:00.0", "0x10de", "0x2520");
        connector(root, "card0", "eDP-1", "connected");
        connector(root, "card1", "DP-1", "disconnected");

        let gpus = scan(&root.join("sys/class/drm"), &root.join("dev/dri"));
        assert!(gpus[0].drives_display, "the panel is on card0");
        assert!(!gpus[1].drives_display);
        assert!(nodes_to_mask(&gpus[1], &gpus).is_empty(), "the display card must stay");
        // The other direction still masks: nothing presents through card1.
        assert_eq!(nodes_to_mask(&gpus[0], &gpus), vec![root.join("dev/dri/renderD129")]);
    }

    #[test]
    fn the_nvidia_stack_is_collected_sonames_and_versioned_files_alike() {
        // A library is opened by soname, but its own DT_NEEDED entries name the
        // versioned files — bind one without the other and nothing loads.
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        for f in [
            "libGLX_nvidia.so.0",
            "libGLX_nvidia.so.610.57.04",
            "libnvidia-glcore.so.610.57.04",
            "libcuda.so.1",
            "libEGL_mesa.so.0",   // not NVIDIA's
            "libfoo.so.1",
        ] {
            write(&lib.join(f), "");
        }
        let manifest = tmp.path().join("10_nvidia.json");
        write(&manifest, "{}");

        let found = nvidia_files_in(
            &[lib.to_str().unwrap()],
            &[manifest.to_str().unwrap(), "/nonexistent/nvidia_icd.json"],
        );
        let names: Vec<String> =
            found.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert!(names.contains(&"libGLX_nvidia.so.0".to_string()), "{names:?}");
        assert!(names.contains(&"libGLX_nvidia.so.610.57.04".to_string()), "{names:?}");
        assert!(names.contains(&"libnvidia-glcore.so.610.57.04".to_string()), "{names:?}");
        assert!(names.contains(&"libcuda.so.1".to_string()), "{names:?}");
        assert!(names.contains(&"10_nvidia.json".to_string()), "{names:?}");
        assert!(!names.contains(&"libEGL_mesa.so.0".to_string()), "{names:?}");
        assert!(!names.contains(&"libfoo.so.1".to_string()), "{names:?}");
        // A manifest the host doesn't have is dropped, not bound as a hole.
        assert!(!names.iter().any(|n| n == "nvidia_icd.json"), "{names:?}");
    }

    #[test]
    fn a_host_without_the_nvidia_driver_yields_nothing_to_bind() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        write(&lib.join("libEGL_mesa.so.0"), "");
        assert!(nvidia_files_in(&[lib.to_str().unwrap()], &[]).is_empty());
    }

    #[test]
    fn auto_and_blank_resolve_to_no_pinning() {
        assert!(resolve(None).is_none());
        assert!(resolve(Some("auto")).is_none());
        assert!(resolve(Some("")).is_none());
    }
}
