//! Built-in CPU profiles for /proc/cpuinfo spoofing.
//!
//! Each profile renders a full, believable `/proc/cpuinfo` (one block per
//! logical CPU). Profiles are stored in a config as `preset:<key>`; the sandbox
//! launcher calls [`cpuinfo_for`] to materialise the text before binding it over
//! `/proc/cpuinfo`. The set spans budget → flagship → server across both Intel
//! and AMD so an app sees a plausible machine from any tier.

pub struct CpuProfile {
    /// Stable identifier stored in the config as `preset:<key>`.
    pub key: &'static str,
    /// Short label shown in the settings picker.
    pub label: &'static str,
    /// One-line description shown in the option help.
    pub desc: &'static str,
    vendor_id: &'static str,
    family: u32,
    model: u32,
    model_name: &'static str,
    cores: u32,
    threads: u32,
    mhz: u32,
    cache_kb: u32,
    address_sizes: &'static str,
    flags: &'static str,
}

// Representative modern flag sets. Not exhaustive, but enough that feature
// probes (AVX2, AES-NI, etc.) see something coherent for the vendor.
const INTEL_FLAGS: &str = "fpu vme de pse tsc msr pae mce cx8 apic sep mtrr pge mca cmov pat pse36 clflush dts acpi mmx fxsr sse sse2 ss ht tm pbe syscall nx pdpe1gb rdtscp lm constant_tsc art arch_perfmon pebs bts rep_good nopl xtopology nonstop_tsc cpuid aperfmperf tsc_known_freq pni pclmulqdq dtes64 monitor ds_cpl vmx smx est tm2 ssse3 sdbg fma cx16 xtpr pdcm pcid sse4_1 sse4_2 x2apic movbe popcnt tsc_deadline_timer aes xsave avx f16c rdrand lahf_lm abm 3dnowprefetch cpuid_fault invpcid_single ssbd ibrs ibpb stibp fsgsbase tsc_adjust bmi1 avx2 smep bmi2 erms invpcid rdseed adx smap clflushopt clwb sha_ni xsaveopt xsavec xgetbv1 xsaves";
const AMD_FLAGS: &str = "fpu vme de pse tsc msr pae mce cx8 apic sep mtrr pge mca cmov pat pse36 clflush mmx fxsr sse sse2 ht syscall nx mmxext fxsr_opt pdpe1gb rdtscp lm constant_tsc rep_good nopl nonstop_tsc cpuid extd_apicid aperfmperf rapl pni pclmulqdq monitor ssse3 fma cx16 sse4_1 sse4_2 movbe popcnt aes xsave avx f16c rdrand lahf_lm cmp_legacy svm extapic cr8_legacy abm sse4a misalignsse 3dnowprefetch osvw ibs skinit wdt tce topoext perfctr_core perfctr_nb bpext perfctr_llc mwaitx cpb cat_l3 cdp_l3 hw_pstate ssbd mba ibrs ibpb stibp vmmcall fsgsbase bmi1 avx2 smep bmi2 erms invpcid rdseed adx smap clflushopt clwb sha_ni xsaveopt xsavec xgetbv1 xsaves";

pub const CPU_PROFILES: &[CpuProfile] = &[
    CpuProfile {
        key: "celeron-n4020",
        label: "Intel Celeron N4020 (2C budget)",
        desc: "Intel Celeron N4020 — 2-core entry laptop chip. Lowest tier, lowest price.",
        vendor_id: "GenuineIntel", family: 6, model: 122,
        model_name: "Intel(R) Celeron(R) N4020 CPU @ 1.10GHz",
        cores: 2, threads: 2, mhz: 1100, cache_kb: 4096,
        address_sizes: "39 bits physical, 48 bits virtual", flags: INTEL_FLAGS,
    },
    CpuProfile {
        key: "core-i3-12100",
        label: "Intel Core i3-12100 (4C entry)",
        desc: "Intel Core i3-12100 — 4-core/8-thread Alder Lake. Budget desktop.",
        vendor_id: "GenuineIntel", family: 6, model: 151,
        model_name: "12th Gen Intel(R) Core(TM) i3-12100 @ 3.30GHz",
        cores: 4, threads: 8, mhz: 3300, cache_kb: 12288,
        address_sizes: "46 bits physical, 48 bits virtual", flags: INTEL_FLAGS,
    },
    CpuProfile {
        key: "core-i5-12400",
        label: "Intel Core i5-12400 (6C mid)",
        desc: "Intel Core i5-12400 — 6-core/12-thread Alder Lake. Mainstream desktop.",
        vendor_id: "GenuineIntel", family: 6, model: 151,
        model_name: "12th Gen Intel(R) Core(TM) i5-12400 @ 2.50GHz",
        cores: 6, threads: 12, mhz: 2500, cache_kb: 18432,
        address_sizes: "46 bits physical, 48 bits virtual", flags: INTEL_FLAGS,
    },
    CpuProfile {
        key: "core-i7-13700k",
        label: "Intel Core i7-13700K (16C high)",
        desc: "Intel Core i7-13700K — 16-core/24-thread Raptor Lake. High-end desktop.",
        vendor_id: "GenuineIntel", family: 6, model: 183,
        model_name: "13th Gen Intel(R) Core(TM) i7-13700K",
        cores: 16, threads: 24, mhz: 3400, cache_kb: 30720,
        address_sizes: "46 bits physical, 48 bits virtual", flags: INTEL_FLAGS,
    },
    CpuProfile {
        key: "core-i9-14900k",
        label: "Intel Core i9-14900K (24C flagship)",
        desc: "Intel Core i9-14900K — 24-core/32-thread Raptor Lake. Flagship desktop.",
        vendor_id: "GenuineIntel", family: 6, model: 183,
        model_name: "14th Gen Intel(R) Core(TM) i9-14900K",
        cores: 24, threads: 32, mhz: 3200, cache_kb: 36864,
        address_sizes: "46 bits physical, 48 bits virtual", flags: INTEL_FLAGS,
    },
    CpuProfile {
        key: "xeon-gold-6338",
        label: "Intel Xeon Gold 6338 (32C server)",
        desc: "Intel Xeon Gold 6338 — 32-core/64-thread Ice Lake-SP. Server/datacenter.",
        vendor_id: "GenuineIntel", family: 6, model: 106,
        model_name: "Intel(R) Xeon(R) Gold 6338 CPU @ 2.00GHz",
        cores: 32, threads: 64, mhz: 2000, cache_kb: 49152,
        address_sizes: "46 bits physical, 57 bits virtual", flags: INTEL_FLAGS,
    },
    CpuProfile {
        key: "ryzen-3-3200g",
        label: "AMD Ryzen 3 3200G (4C budget)",
        desc: "AMD Ryzen 3 3200G — 4-core Zen+ APU with Vega graphics. Budget desktop.",
        vendor_id: "AuthenticAMD", family: 23, model: 24,
        model_name: "AMD Ryzen 3 3200G with Radeon Vega Graphics",
        cores: 4, threads: 4, mhz: 3600, cache_kb: 512,
        address_sizes: "43 bits physical, 48 bits virtual", flags: AMD_FLAGS,
    },
    CpuProfile {
        key: "ryzen-5-5600x",
        label: "AMD Ryzen 5 5600X (6C mid)",
        desc: "AMD Ryzen 5 5600X — 6-core/12-thread Zen 3. Mainstream desktop.",
        vendor_id: "AuthenticAMD", family: 25, model: 33,
        model_name: "AMD Ryzen 5 5600X 6-Core Processor",
        cores: 6, threads: 12, mhz: 3700, cache_kb: 512,
        address_sizes: "48 bits physical, 48 bits virtual", flags: AMD_FLAGS,
    },
    CpuProfile {
        key: "ryzen-9-7950x",
        label: "AMD Ryzen 9 7950X (16C high)",
        desc: "AMD Ryzen 9 7950X — 16-core/32-thread Zen 4. High-end desktop/HEDT.",
        vendor_id: "AuthenticAMD", family: 25, model: 97,
        model_name: "AMD Ryzen 9 7950X 16-Core Processor",
        cores: 16, threads: 32, mhz: 4500, cache_kb: 1024,
        address_sizes: "48 bits physical, 48 bits virtual", flags: AMD_FLAGS,
    },
    CpuProfile {
        key: "epyc-7763",
        label: "AMD EPYC 7763 (64C server)",
        desc: "AMD EPYC 7763 — 64-core/128-thread Zen 3. Top-tier server/datacenter.",
        vendor_id: "AuthenticAMD", family: 25, model: 1,
        model_name: "AMD EPYC 7763 64-Core Processor",
        cores: 64, threads: 128, mhz: 2450, cache_kb: 512,
        address_sizes: "48 bits physical, 48 bits virtual", flags: AMD_FLAGS,
    },
];

/// If `spec` names a built-in preset (`preset:<key>`) or a user-defined CPU
/// (`custom:<...>`), render its `/proc/cpuinfo`.
pub fn cpuinfo_for(spec: &str) -> Option<String> {
    if let Some(c) = CustomCpu::parse(spec) {
        return Some(generate(&c.view()));
    }
    let key = spec.strip_prefix("preset:")?;
    let p = CPU_PROFILES.iter().find(|p| p.key == key)?;
    Some(generate(&p.view()))
}

/// A user-defined CPU built with the TUI configurator. Serialized into the
/// config as `custom:<vendor>|<family>|<model>|<stepping>|<cores>|<threads>|<mhz>|<cache_kb>|<model name>`.
/// The model name is the last field so it may safely contain any character
/// except a newline. Flags and address sizes are derived from the vendor.
#[derive(Clone, PartialEq)]
pub struct CustomCpu {
    pub vendor_id: String,
    pub family: u32,
    pub model: u32,
    pub stepping: u32,
    pub cores: u32,
    pub threads: u32,
    pub mhz: u32,
    pub cache_kb: u32,
    pub model_name: String,
}

impl CustomCpu {
    /// A sensible starting point for the configurator (a generic 8-core Intel).
    pub fn starter() -> Self {
        CustomCpu {
            vendor_id: "GenuineIntel".to_string(),
            family: 6,
            model: 151,
            stepping: 1,
            cores: 8,
            threads: 16,
            mhz: 3200,
            cache_kb: 16384,
            model_name: "Custom CPU @ 3.20GHz".to_string(),
        }
    }

    /// Serialize into the `custom:...` config value.
    pub fn serialize(&self) -> String {
        format!(
            "custom:{}|{}|{}|{}|{}|{}|{}|{}|{}",
            self.vendor_id, self.family, self.model, self.stepping,
            self.cores, self.threads, self.mhz, self.cache_kb, self.model_name,
        )
    }

    /// Parse a `custom:...` config value. Returns None for anything else,
    /// including the bare legacy `custom` (raw-editor) value.
    pub fn parse(spec: &str) -> Option<CustomCpu> {
        let body = spec.strip_prefix("custom:")?;
        // 9 fields; the model name (last) may contain '|', so split with a cap.
        let mut it = body.splitn(9, '|');
        let vendor_id = it.next()?.to_string();
        let family = it.next()?.parse().ok()?;
        let model = it.next()?.parse().ok()?;
        let stepping = it.next()?.parse().ok()?;
        let cores = it.next()?.parse().ok()?;
        let threads = it.next()?.parse().ok()?;
        let mhz = it.next()?.parse().ok()?;
        let cache_kb = it.next()?.parse().ok()?;
        let model_name = it.next()?.to_string();
        Some(CustomCpu {
            vendor_id, family, model, stepping,
            cores, threads, mhz, cache_kb, model_name,
        })
    }

    /// Seed a custom CPU from a built-in `preset:<key>`, so the configurator can
    /// pre-fill from a preset the user was previously using. None for non-presets.
    pub fn from_preset(spec: &str) -> Option<CustomCpu> {
        let key = spec.strip_prefix("preset:")?;
        let p = CPU_PROFILES.iter().find(|p| p.key == key)?;
        Some(CustomCpu {
            vendor_id: p.vendor_id.to_string(),
            family: p.family, model: p.model, stepping: 1,
            cores: p.cores, threads: p.threads, mhz: p.mhz, cache_kb: p.cache_kb,
            model_name: p.model_name.to_string(),
        })
    }

    fn flags(&self) -> &'static str {
        if self.vendor_id == "AuthenticAMD" { AMD_FLAGS } else { INTEL_FLAGS }
    }

    fn view(&self) -> CpuView<'_> {
        CpuView {
            vendor_id: &self.vendor_id,
            family: self.family,
            model: self.model,
            stepping: self.stepping,
            model_name: &self.model_name,
            cores: self.cores.max(1),
            threads: self.threads.max(self.cores).max(1),
            mhz: self.mhz,
            cache_kb: self.cache_kb,
            address_sizes: "48 bits physical, 48 bits virtual",
            flags: self.flags(),
        }
    }
}

/// The values the CPUID-spoofing shim needs to present a fake CPU: 12-char
/// vendor id, brand string (the displayed model name), and the leaf-1 EAX that
/// encodes family/model/stepping.
pub struct CpuidSpoof {
    pub vendor: String,
    pub brand: String,
    pub fms: u32,
}

/// CPUID-spoof data for a config value, if it names a CPU. Covers the
/// `preset:<key>` profiles, a user-defined `custom:<...>`, and the legacy
/// "sample" (Intel i7-8550U).
pub fn cpuid_spoof_for(spec: &str) -> Option<CpuidSpoof> {
    if spec == "sample" {
        return Some(CpuidSpoof {
            vendor: "GenuineIntel".to_string(),
            brand: "Intel(R) Core(TM) i7-8550U CPU @ 1.80GHz".to_string(),
            fms: leaf1_eax(6, 142, 10),
        });
    }
    if let Some(c) = CustomCpu::parse(spec) {
        return Some(CpuidSpoof {
            vendor: c.vendor_id.clone(),
            brand: c.model_name.clone(),
            fms: leaf1_eax(c.family, c.model, c.stepping),
        });
    }
    let key = spec.strip_prefix("preset:")?;
    let p = CPU_PROFILES.iter().find(|p| p.key == key)?;
    Some(CpuidSpoof {
        vendor: p.vendor_id.to_string(),
        brand: p.model_name.to_string(),
        fms: leaf1_eax(p.family, p.model, 1),
    })
}

/// Pack family/model/stepping into the CPUID leaf-1 EAX layout.
fn leaf1_eax(family: u32, model: u32, stepping: u32) -> u32 {
    let base_family = if family >= 0xf { 0xf } else { family };
    let ext_family = family.saturating_sub(0xf);
    let base_model = model & 0xf;
    let ext_model = (model >> 4) & 0xf;
    (stepping & 0xf)
        | (base_model << 4)
        | (base_family << 8)
        | (ext_model << 16)
        | (ext_family << 20)
}

/// A borrowed view of the fields needed to render `/proc/cpuinfo`, shared by
/// the built-in [`CpuProfile`]s and the user-defined [`CustomCpu`].
struct CpuView<'a> {
    vendor_id: &'a str,
    family: u32,
    model: u32,
    stepping: u32,
    model_name: &'a str,
    cores: u32,
    threads: u32,
    mhz: u32,
    cache_kb: u32,
    address_sizes: &'a str,
    flags: &'a str,
}

impl CpuProfile {
    fn view(&self) -> CpuView<'_> {
        CpuView {
            vendor_id: self.vendor_id,
            family: self.family,
            model: self.model,
            stepping: 1,
            model_name: self.model_name,
            cores: self.cores,
            threads: self.threads,
            mhz: self.mhz,
            cache_kb: self.cache_kb,
            address_sizes: self.address_sizes,
            flags: self.flags,
        }
    }
}

/// Render a full `/proc/cpuinfo` body for a CPU — one block per thread.
fn generate(p: &CpuView) -> String {
    let threads_per_core = (p.threads / p.cores.max(1)).max(1);
    let bogomips = format!("{:.2}", p.mhz as f64 * 2.0);
    let mut out = String::new();
    for i in 0..p.threads {
        let core_id = i / threads_per_core;
        out.push_str(&format!(
            "processor\t: {i}\n\
             vendor_id\t: {vendor}\n\
             cpu family\t: {family}\n\
             model\t\t: {model}\n\
             model name\t: {name}\n\
             stepping\t: {stepping}\n\
             cpu MHz\t\t: {mhz}.000\n\
             cache size\t: {cache} KB\n\
             physical id\t: 0\n\
             siblings\t: {threads}\n\
             core id\t\t: {core_id}\n\
             cpu cores\t: {cores}\n\
             apicid\t\t: {i}\n\
             initial apicid\t: {i}\n\
             fpu\t\t: yes\n\
             fpu_exception\t: yes\n\
             cpuid level\t: 22\n\
             wp\t\t: yes\n\
             flags\t\t: {flags}\n\
             bogomips\t: {bogomips}\n\
             clflush size\t: 64\n\
             cache_alignment\t: 64\n\
             address sizes\t: {addr}\n\
             power management:\n\n",
            i = i,
            vendor = p.vendor_id,
            family = p.family,
            model = p.model,
            name = p.model_name,
            stepping = p.stepping,
            mhz = p.mhz,
            cache = p.cache_kb,
            threads = p.threads,
            core_id = core_id,
            cores = p.cores,
            flags = p.flags,
            bogomips = bogomips,
            addr = p.address_sizes,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_unique_and_render() {
        let mut keys = std::collections::HashSet::new();
        for p in CPU_PROFILES {
            assert!(keys.insert(p.key), "duplicate key {}", p.key);
            let text = cpuinfo_for(&format!("preset:{}", p.key)).expect("renders");
            // One block per thread, and the model name shows up.
            assert_eq!(text.matches("processor\t:").count(), p.threads as usize);
            assert!(text.contains(p.model_name));
            assert!(text.contains(p.vendor_id));
        }
    }

    #[test]
    fn unknown_preset_is_none() {
        assert!(cpuinfo_for("preset:nope").is_none());
        assert!(cpuinfo_for("/some/path").is_none());
        assert!(cpuinfo_for("sample").is_none());
    }

    #[test]
    fn custom_round_trips_and_renders() {
        let c = CustomCpu {
            vendor_id: "AuthenticAMD".to_string(),
            family: 25, model: 97, stepping: 2,
            cores: 12, threads: 24, mhz: 4200, cache_kb: 65536,
            model_name: "My Fancy Chip @ 4.20GHz | rev A".to_string(),
        };
        let spec = c.serialize();
        assert!(spec.starts_with("custom:"));
        // Round-trips even though the model name contains '|'.
        assert!(CustomCpu::parse(&spec).unwrap() == c);
        // Renders one block per thread with the custom name and vendor.
        let text = cpuinfo_for(&spec).expect("renders");
        assert_eq!(text.matches("processor\t:").count(), 24);
        assert!(text.contains("My Fancy Chip @ 4.20GHz | rev A"));
        assert!(text.contains("AuthenticAMD"));
        // And drives the CPUID shim.
        let sp = cpuid_spoof_for(&spec).expect("cpuid");
        assert_eq!(sp.vendor, "AuthenticAMD");
        assert_eq!(sp.fms, leaf1_eax(25, 97, 2));
    }

    #[test]
    fn bare_custom_is_not_a_custom_cpu() {
        // The legacy raw-editor value is a bare "custom", not "custom:...".
        assert!(CustomCpu::parse("custom").is_none());
        assert!(cpuinfo_for("custom").is_none());
    }
}
