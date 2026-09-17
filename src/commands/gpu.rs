//! `wryayer gpu` — the GPUs this machine can render on, and how to name one.
//!
//! The ids are the values `wryayer config <app> gpu <id>` accepts, so the
//! listing is what makes that setting usable: nobody knows their card's PCI
//! address by heart.

use anyhow::Result;

pub fn list() -> Result<()> {
    let gpus = crate::gpu::all();
    if gpus.is_empty() {
        eprintln!("no GPUs found under /sys/class/drm");
        return Ok(());
    }

    for gpu in gpus {
        println!("{}", gpu.id);
        println!("  {}", gpu.label());
        let render = gpu
            .render
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "none (display only)".to_string());
        println!(
            "  driver {} · {} · render node {}",
            gpu.driver.as_deref().unwrap_or("none"),
            gpu.card,
            render
        );
    }

    eprintln!();
    if gpus.len() == 1 {
        // Nothing to choose between, so say that rather than let the user hunt
        // for a setting that would do nothing here.
        eprintln!("Only one GPU: apps render on it whatever the gpu setting says.");
    } else {
        eprintln!("Pin an app to one of them with:");
        eprintln!("    wryayer config <app> gpu {}", gpus[0].id);
        eprintln!("A name works too — e.g. 'nvidia', 'card1', or part of the model name.");
        eprintln!("'auto' hands the choice back to the driver.");
    }
    Ok(())
}
