//! Throwaway measurement: reserve decode tables the way iris's
//! src/ppmem/devmap.rs does, for several table widths, and report what it
//! costs. A table of B physical-address bits is 2^(B-12) entries of 16 bytes:
//! 32 bits = 16 MiB, 40 = 4 GiB, 43 = 32 GiB, 44 = 64 GiB of address space.
//!
//! Per table this does what `SparseDeviceMap::new` and a first fill do:
//! - reserve the whole table as inaccessible address space
//!   (Unix: mmap PROT_NONE, MAP_PRIVATE|MAP_ANONYMOUS|MAP_NORESERVE;
//!   Windows: VirtualAlloc2 MEM_RESERVE|MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS);
//! - build the per-page state vector (8 bytes per 64 KiB table page);
//! - map one 64 KiB shared page over the first and the last table page
//!   (Unix: MAP_SHARED|MAP_FIXED; Windows: split the placeholder, then
//!   MapViewOfFile3 MEM_REPLACE_PLACEHOLDER of a pagefile section), write it
//!   and read it back.
//!
//! usage: devmap-spike                 the default plan: widths 32 40 43 44, 1/8/64 tables
//!        devmap-spike BITS COUNT      one case
//!        devmap-spike BITS max [CAP]  reserve until failure (address-space ceiling)
use std::time::{Duration, Instant};

const PAGE: usize = 64 * 1024;

#[derive(Clone, Copy)]
#[allow(dead_code)]
enum PageState {
    Unmapped,
    Lazy(u32),
    Const(u32),
    Private(u32),
}

struct Table {
    base: *mut u8,
    bytes: usize,
    _pages: Vec<PageState>,
    #[allow(dead_code)]
    views: Vec<os::View>,
}

fn table_bytes(bits: u32) -> usize {
    (1usize << (bits - 12)) * 16
}

fn reserve(bytes: usize) -> Result<Table, String> {
    let base = os::reserve(bytes)?;
    Ok(Table { base, bytes, _pages: vec![PageState::Unmapped; bytes / PAGE], views: Vec::new() })
}

fn touch(t: &mut Table) -> Result<(), String> {
    for off in [0, t.bytes - PAGE] {
        let (p, v) = os::map_page(t.base, t.bytes, off)?;
        unsafe {
            std::ptr::write_volatile(p, 0xa5);
            if std::ptr::read_volatile(p) != 0xa5 {
                return Err("read-back mismatch".into());
            }
        }
        t.views.push(v);
    }
    Ok(())
}

fn release(t: Table) {
    let Table { base, bytes, views, .. } = t;
    os::release(base, bytes, views);
}

fn ms(d: Duration) -> String {
    format!("{:.3} ms", d.as_secs_f64() * 1e3)
}

fn case(bits: u32, n: usize) -> bool {
    let bytes = table_bytes(bits);
    println!("-- {bits}-bit table ({} MiB reserved each) x {n}", bytes >> 20);
    println!("   before:   {}", os::usage());
    let t0 = Instant::now();
    let mut v = Vec::new();
    for i in 0..n {
        match reserve(bytes) {
            Ok(t) => v.push(t),
            Err(e) => {
                println!("   FAIL reserving table {i} of {n}: {e}");
                for t in v {
                    release(t);
                }
                return false;
            }
        }
    }
    println!("   reserved: {} ; {}", ms(t0.elapsed()), os::usage());
    let t1 = Instant::now();
    for (i, t) in v.iter_mut().enumerate() {
        if let Err(e) = touch(t) {
            println!("   FAIL touching table {i}: {e}");
            for t in v {
                release(t);
            }
            return false;
        }
    }
    println!("   touched:  {} (2 pages per table) ; {}", ms(t1.elapsed()), os::usage());
    let t2 = Instant::now();
    for t in v {
        release(t);
    }
    println!("   released: {} ; {}", ms(t2.elapsed()), os::usage());
    true
}

fn max(bits: u32, cap: usize) {
    let bytes = table_bytes(bits);
    let t0 = Instant::now();
    let mut v = Vec::new();
    let mut why = String::from("cap reached");
    while v.len() < cap {
        match os::reserve(bytes) {
            Ok(p) => v.push(p),
            Err(e) => {
                why = e;
                break;
            }
        }
    }
    println!(
        "-- {bits}-bit ceiling: {} tables = {} GiB of address space in {} ({why})",
        v.len(),
        (v.len() * bytes) >> 30,
        ms(t0.elapsed())
    );
    for p in v {
        os::release(p, bytes, Vec::new());
    }
}

fn main() {
    println!("{}", os::describe());
    let a: Vec<String> = std::env::args().skip(1).collect();
    let mut ok = true;
    match a.as_slice() {
        [] => {
            for bits in [32, 40, 43, 44] {
                for n in [1, 8, 64] {
                    ok &= case(bits, n);
                }
            }
            max(43, 4096);
            max(40, 32768);
        }
        [b, m] if m == "max" => max(b.parse().unwrap(), 16384),
        [b, m, c] if m == "max" => max(b.parse().unwrap(), c.parse().unwrap()),
        [b, n] => ok = case(b.parse().unwrap(), n.parse().unwrap()),
        _ => panic!("usage: devmap-spike [BITS COUNT | BITS max [CAP]]"),
    }
    if !ok {
        std::process::exit(1);
    }
}

#[cfg(unix)]
mod os {
    use super::PAGE;

    pub struct View;

    fn err(what: &str) -> String {
        format!("{what}: {}", std::io::Error::last_os_error())
    }

    pub fn describe() -> String {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        format!("{} {} host page {page} B", std::env::consts::OS, std::env::consts::ARCH)
    }

    pub fn reserve(bytes: usize) -> Result<*mut u8, String> {
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(err("mmap(reserve)"));
        }
        Ok(p.cast())
    }

    pub fn map_page(base: *mut u8, _bytes: usize, off: usize) -> Result<(*mut u8, View), String> {
        let want = unsafe { base.add(off) };
        let p = unsafe {
            libc::mmap(
                want.cast(),
                PAGE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED || p.cast() != want {
            return Err(err("mmap(MAP_FIXED view)"));
        }
        Ok((want, View))
    }

    pub fn release(base: *mut u8, bytes: usize, _views: Vec<View>) {
        unsafe { libc::munmap(base.cast(), bytes) };
    }

    #[cfg(target_os = "linux")]
    pub fn usage() -> String {
        let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let m = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let pick = |t: &str, k: &str| {
            t.lines()
                .find(|l| l.starts_with(k))
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("?")
                .to_string()
        };
        format!(
            "VmSize {} kB, VmRSS {} kB, system Committed_AS {} kB",
            pick(&s, "VmSize:"),
            pick(&s, "VmRSS:"),
            pick(&m, "Committed_AS:")
        )
    }

    #[cfg(not(target_os = "linux"))]
    pub fn usage() -> String {
        let pid = std::process::id().to_string();
        let o = std::process::Command::new("ps").args(["-o", "vsz=,rss=", "-p", &pid]).output();
        let s = o.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        let v: Vec<&str> = s.split_whitespace().collect();
        format!("VSZ {} kB, RSS {} kB", v.first().unwrap_or(&"?"), v.get(1).unwrap_or(&"?"))
    }
}

#[cfg(windows)]
mod os {
    use super::PAGE;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile3, UnmapViewOfFile2, VirtualAlloc2, VirtualFree,
        MEMORY_MAPPED_VIEW_ADDRESS, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER,
        MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS, PAGE_READWRITE,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        GetPerformanceInfo, GetProcessMemoryInfo, PERFORMANCE_INFORMATION, PROCESS_MEMORY_COUNTERS,
        PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    const MEM_COALESCE_PLACEHOLDERS: u32 = 0x0000_0001;

    pub struct View {
        at: *mut u8,
        section: HANDLE,
    }

    fn err(what: &str) -> String {
        format!("{what}: {}", std::io::Error::last_os_error())
    }

    pub fn describe() -> String {
        let mut si: SYSTEM_INFO = unsafe { std::mem::zeroed() };
        unsafe { GetSystemInfo(&mut si) };
        format!(
            "windows {} page {} B, allocation granularity {} B, user VA {:#x}..{:#x}",
            std::env::consts::ARCH,
            si.dwPageSize,
            si.dwAllocationGranularity,
            si.lpMinimumApplicationAddress as usize,
            si.lpMaximumApplicationAddress as usize
        )
    }

    pub fn reserve(bytes: usize) -> Result<*mut u8, String> {
        let p = unsafe {
            VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                bytes,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            )
        };
        if p.is_null() {
            return Err(err("VirtualAlloc2(MEM_RESERVE_PLACEHOLDER)"));
        }
        Ok(p.cast())
    }

    /// Split a 64 KiB placeholder out of the reservation at `off` and map a
    /// fresh pagefile section over it, as map_windows.rs does.
    pub fn map_page(base: *mut u8, bytes: usize, off: usize) -> Result<(*mut u8, View), String> {
        let at = unsafe { base.add(off) };
        // The reservation may already be split by an earlier call; split
        // only what is still one piece with `at`. Callers here map the first
        // page, then the last, so the second split always has a prefix.
        if off > 0 && unsafe { VirtualFree(base.add(PAGE).cast(), bytes - 2 * PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) } == 0 {
            return Err(err("VirtualFree(split last page)"));
        }
        if off == 0 && unsafe { VirtualFree(at.cast(), PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) } == 0 {
            return Err(err("VirtualFree(split first page)"));
        }
        let section = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, PAGE as u32, std::ptr::null())
        };
        if section.is_null() {
            return Err(err("CreateFileMappingW"));
        }
        let got = unsafe {
            MapViewOfFile3(section, GetCurrentProcess(), at.cast(), 0, PAGE, MEM_REPLACE_PLACEHOLDER, PAGE_READWRITE, std::ptr::null_mut(), 0)
        };
        if got.Value.is_null() || got.Value.cast::<u8>() != at {
            return Err(err("MapViewOfFile3(MEM_REPLACE_PLACEHOLDER)"));
        }
        Ok((at, View { at, section }))
    }

    pub fn release(base: *mut u8, bytes: usize, views: Vec<View>) {
        let split = !views.is_empty();
        for v in views {
            unsafe {
                UnmapViewOfFile2(GetCurrentProcess(), MEMORY_MAPPED_VIEW_ADDRESS { Value: v.at.cast() }, MEM_PRESERVE_PLACEHOLDER);
                CloseHandle(v.section);
            }
        }
        unsafe {
            if split {
                VirtualFree(base.cast(), bytes, MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS);
            }
            VirtualFree(base.cast(), 0, MEM_RELEASE);
        }
    }

    pub fn usage() -> String {
        let mut pmc: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
        pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        unsafe {
            GetProcessMemoryInfo(GetCurrentProcess(), (&mut pmc as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(), pmc.cb)
        };
        let mut pi: PERFORMANCE_INFORMATION = unsafe { std::mem::zeroed() };
        pi.cb = std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32;
        unsafe { GetPerformanceInfo(&mut pi, pi.cb) };
        format!(
            "process commit (PrivateUsage) {} kB, working set {} kB, system commit {} / limit {} MB",
            pmc.PrivateUsage >> 10,
            pmc.WorkingSetSize >> 10,
            pi.CommitTotal * pi.PageSize >> 20,
            pi.CommitLimit * pi.PageSize >> 20
        )
    }
}
