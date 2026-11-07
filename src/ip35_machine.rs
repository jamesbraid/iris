//! The Origin 3200 (IP35): one or two C-bricks, each a Bedrock node with an
//! R12000, and an I-brick on the first C-brick's XIO connector carrying the
//! base I/O. Two C-bricks are joined by a direct NUMAlink cable with no
//! router.
//!
//! Selected with `[machine] profile = "origin3200_one_c_ip35"` or
//! `"origin3200_two_c_ip35"`. The shared keys select the PROM, NVRAM, memory
//! and disks, and `[fuel]` selects the serial console and writable flash of
//! the first C-brick. The I-brick's base I/O is an XBridge with an IOC3, USB
//! and a QLogic Fibre Channel adapter at the bus slot where the Fuel has its
//! SCSI controller; the I-brick has no SCSI controller. Each C-brick has its own
//! L1; the first carries the console. The nodes are the shared Bedrock node
//! (`ip35::node`); what a brick adds is its processor row, its L1 identity
//! and, for the first, the I-brick.
//!
//! # What is approximated here
//!
//! - A C-brick holds two or four processors; the model runs one a node, so
//!   positions B, C and D are listed absent in the flash and a one-PIMM L1
//!   record is answered. Which PIMM count a one-processor C-brick would
//!   report is not established.
//! - Which low nibble of the board identification word a C-brick presents
//!   is a choice: 2 is the row for the 400 MHz, 8 MiB part.
//! - The I-brick's IEEE 1394 controller is not modelled; the PROM prints
//!   `No 1394 Firewire found on BASEIO` and carries on.
//! - The Fibre Channel slot runs 64-bit data phases. That is a property of
//!   the I-brick's board wiring, inferred from the PROM's behaviour alone.
//!
//! Board composition ported from hw/mips/sgi/sn1-node.c
//! in jamesbraid/qemu-sgi-origami.

use crate::config::{scsi_label, FuelConsole, MachineConfig};
use crate::cpu::mips_cache_v2::CpuModel as CacheCpuModel;
use crate::cpu::mips_exec::{CpuDevice, MipsCpu, MipsCpuConfig, MipsExecutor};
use crate::cpu::mips_tlb::MipsTlb;
use crate::cpu::parts::{self, Part};
use crate::dev::bedrock_flash::{disabled_positions_environment, FirmwareRecord, Flash, Header};
use crate::dev::bedrock_pi::{BedrockPi, Population};
use crate::dev::heart::{host_clock, Clock};
use crate::dev::ioc3::{Ioc3, Ioc3Config};
use crate::dev::ioc3_eth::Ioc3Eth;
use crate::dev::isp2200::Isp2200;
use crate::dev::l1::{AttachedBrick, ComputeNode, L1Config};
use crate::dev::odyssey::{PiGfx, PI_GFX_REGS};
use crate::dev::ohci::Ohci;
use crate::dev::prom_image;
use crate::dev::ps2::Ps2Controller;
use crate::dev::serial_host::CiSerialBackend;
use crate::dev::timekeeper::Timekeeper;
use crate::dev::usb_hid::{UsbKeyboard, UsbMouse};
use crate::dev::xbridge::XBridge;
use crate::devlog::{init_devlog, LogModule};
use crate::gdb_stub::CpuDebug;
use crate::gfx_display::GfxDisplay;
use crate::hptimer::TimerManager;
use crate::ip35::node::{connect_cpu, connect_serial_ports, Node, NodeConfig, SerialPort};
use crate::ip35::CPU_PHYS_MASK;
use crate::ip35_fabric::Fabric;
use crate::machine_state::GuestState;
use crate::monitor::{Monitor, DEFAULT_PORT as DEFAULT_MONITOR_PORT};
use crate::net::{GatewayConfig, Link, NatControl};
use crate::perf_monitor::PerfMonitor;
use crate::scsi::{attach_from_config, ScsiBus};
use crate::traits::{BusDevice, Device, MachineEvent, Resettable, BUS_OK};
use crate::workstation::{register_system_controller, DiscChanger, MachineRef, MachineSlot, Workstation};
use parking_lot::Mutex;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Arc};

/// The processors an Origin 3200 C-brick is built with: the 400 MHz R12000
/// with an 8 MiB secondary cache.
pub const CPUS: &[&Part] = &[&parts::R12000_400_CBRICK];

/// The memory of each C-brick in MiB: two 256 MiB banks.
const BANKS_MIB: [u32; 2] = [256, 256];

/// The C-brick's 400 MHz / 8 MiB PROM record selects these boot-mode inputs:
/// a mismatch with the processor's Config sends the PROM to its error routine
/// before it initialises GP.
const CBRICK_FIRMWARE: FirmwareRecord = FirmwareRecord {
    cpu_mode_word: 0x5414_a785,
    reset_mode: 0x0014_a785,
    cpu_clock_hz: 400_000_000,
    cpu_type: 0,
    header: Header::Refreshed,
};

/// Bits 15:12 of the board identification word are the product code (0 is a
/// C-brick to the PROM) and the low nibble the PROM's configuration row; 2 is
/// the 400 MHz, 8 MiB row (a choice: which nibble a C-brick presents is not
/// established).
const CBRICK_BOARD_ID: u64 = 2;
/// The L1's brick type character for a C-brick.
const CBRICK_L1_TYPE: u32 = 0x43;
/// The I-brick of the Origin 3200 in module 001i14, as the first C-brick's
/// L1 reports it.
const I_BRICK: AttachedBrick = AttachedBrick { bay: 14, type_code: 0x49 };

/// Where each C-brick sits: the rack bay its L1 reports (module 001c04 is bay
/// 4) and the synthetic serial of its L1.
const BAYS: [(u32, &str); 2] = [(4, "EMUN00"), (7, "EMUN01")];

/// How many C-bricks a profile has.
fn bricks(cfg: &MachineConfig) -> usize {
    match cfg.machine.profile {
        crate::config::MachineProfile::Origin3200TwoCIp35 => 2,
        _ => 1,
    }
}

/// The IOC3, storage controller and USB on the I-brick's XBridge.
pub(crate) struct BaseIo {
    pub xbridge: Arc<XBridge>,
    pub ioc3: Arc<Ioc3>,
    pub eth: Arc<Ioc3Eth>,
    /// The I-brick's Fibre Channel adapter, where the Fuel has its SCSI
    /// controller; the I-brick has none.
    pub storage: Arc<Isp2200>,
    pub usb: Arc<Ohci>,
    pub serial: [Arc<CiSerialBackend>; 3],
}

/// One C-brick: a Bedrock node, its processor and the state its processor's
/// batch hook polls.
struct CBrick {
    node: Node,
    cpu: Arc<dyn CpuDevice>,
    poll: Arc<NodePoll>,
}

/// What a node's processor brings up to date every batch: the node's clock
/// into its flash and PIs, and the USB controller of the I-brick on the first.
struct NodePoll {
    clock: Clock,
    last_clock: AtomicU64,
    running: AtomicBool,
    flash: Arc<Flash>,
    pi: [Arc<BedrockPi>; 2],
    usb: Option<Arc<Ohci>>,
    /// The node's node-field-zero routing and the memory it takes the direct
    /// window from.
    away: Option<(Arc<crate::ip35::ZeroAway>, Arc<crate::physical::Physical>)>,
}

/// What the first C-brick's L1 answers a config may replace.
#[derive(Clone, Debug, Default)]
struct L1Identity {
    revision: Option<[u32; 3]>,
    brick_serial: Option<String>,
}

pub struct Ip35Machine {
    /// The C-bricks; the first carries the console and the I-brick.
    bricks: Vec<Arc<CBrick>>,
    base: Arc<BaseIo>,
    timers: Arc<TimerManager>,
    monitor: Arc<Monitor>,
    monitor_port: u16,
    ci_serial: Option<Arc<CiSerialBackend>>,
    console: usize,
    ci: bool,
    scratch: Option<PathBuf>,
    timekeeper: Arc<Timekeeper>,
    nvram: String,
    frames: Option<Arc<AtomicU64>>,
    nat: Option<(Arc<NatControl>, (Ipv4Addr, Ipv4Addr, Ipv4Addr))>,
    events: Option<Receiver<MachineEvent>>,
    slot: Option<MachineSlot>,
    lifecycle: GuestState,
}

impl Ip35Machine {
    pub fn new(cfg: &MachineConfig) -> Result<Self, String> {
        cfg.validate()?;
        let prom = prom_image::load(Path::new(&cfg.prom), 8 * 1024 * 1024)
            .map_err(|e| format!("Origin 3200 {e}"))?;
        let timekeeper = Arc::new(Timekeeper::fuel_with_nvram_file(Path::new(&cfg.nvram))?);
        let part = cfg.cpu();
        let count = bricks(cfg);
        let fabric = (count == 2).then(Fabric::new);
        let timers = Arc::new(TimerManager::new());
        let (merged_tx, merged_rx) = channel();
        let mut nodes = Vec::new();
        for index in 0..count {
            let (bay, serial) = BAYS[index];
            let first = index == 0;
            let mut station_mac = cfg.network().mac;
            station_mac[5] = station_mac[5].wrapping_add(index as u8);
            let identity = if first {
                L1Identity {
                    revision: cfg.fuel.l1_revision.as_deref().map(crate::dev::l1::parse_revision).transpose()?,
                    brick_serial: cfg.fuel.brick_serial.clone(),
                }
            } else {
                L1Identity::default()
            };
            let mut l1 = L1Config {
                bay,
                type_code: CBRICK_L1_TYPE,
                mac: station_mac,
                brick_serial: Some(serial.into()),
                attached: first.then_some(I_BRICK),
                compute_node: Some(ComputeNode { index: index as u8, pimm_mask: 1 }),
                ..Default::default()
            };
            if let Some(revision) = identity.revision {
                l1.revision = Some(revision);
            }
            if let Some(serial) = identity.brick_serial {
                l1.brick_serial = Some(serial);
            }
            let node = Node::build_linked(
                NodeConfig {
                    name: "C-brick",
                    payload: &prom,
                    clock: host_clock(),
                    flash_path: cfg.fuel.flash.as_deref().filter(|_| first).map(Path::new),
                    banks_mib: &BANKS_MIB,
                    population: Population::SINGLE,
                    running: Population::SINGLE,
                    firmware: CBRICK_FIRMWARE,
                    // One processor runs: B, C and D hold none.
                    flash_records: disabled_positions_environment("BCD"),
                    board_id: CBRICK_BOARD_ID,
                    l1,
                },
                fabric.clone().map(|fabric| (fabric, index)),
            )?;
            // Each processor interface keeps its own graphics registers.
            for pi in &node.pi {
                pi.set_node_hooks(Arc::new(PiGraphics::default()));
            }
            nodes.push(node);
        }
        let base = Arc::new(BaseIo::new(&nodes[0], cfg, timekeeper.clone())?);
        let mut bricks_built = Vec::new();
        for (index, node) in nodes.into_iter().enumerate() {
            let clock = host_clock();
            let poll = Arc::new(NodePoll {
                last_clock: AtomicU64::new(clock()),
                clock,
                running: AtomicBool::new(false),
                flash: node.flash.clone(),
                pi: node.pi.clone(),
                usb: (index == 0).then(|| base.usb.clone()),
                away: node.away.clone().map(|away| (away, node.md.physical().clone())),
            });
            let cpu = spawn_cpu(&node, part, cfg, &timers, poll.clone(), count > 1);
            let rx = connect_cpu("C-brick", &node.md, &node.pi[0], &node.lb, &node.ni, &node.l1, &cpu);
            // Any node's reset or power request reaches the machine's main
            // thread; per-node reset is not modelled.
            let tx = merged_tx.clone();
            std::thread::Builder::new()
                .name(format!("ip35-events-{index}"))
                .spawn(move || {
                    for event in rx {
                        if tx.send(event).is_err() {
                            break;
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
            bricks_built.push(Arc::new(CBrick { node, cpu, poll }));
        }
        drop(merged_tx);
        let scratch = Self::attach_scsi(cfg, &base)?;
        let console = match cfg.fuel.console {
            FuelConsole::L1 => 0,
            FuelConsole::Ioc3A => 1,
            FuelConsole::Ioc3B => 2,
        };
        let ci_serial = cfg.ci.then(|| base.serial[console].clone());
        Self::connect_serial(cfg, &bricks_built[0], &base, console)?;
        let first = &bricks_built[0];
        let monitor = Monitor::new();
        monitor.register_device(init_devlog());
        let cpus = bricks_built.iter().map(|b| b.cpu.clone()).collect();
        monitor.register_device(Arc::new(crate::cpu::cpu_monitor::CpuMonitor::new(cpus)));
        monitor.register_device(PerfMonitor::new(
            first.cpu.running_flag(), first.cpu.cycles_ptr(), first.cpu.fasttick_count(), None, None,
        ));
        monitor.register_device(first.node.md.physical().clone());
        monitor.register_device(first.node.l1.clone());
        monitor.register_device(base.storage.clone());
        // `ps2 type` and friends: the keyboard and mouse from the monitor.
        monitor.register_device(base.ioc3.ps2());
        for brick in &bricks_built {
            brick.cpu.register_locks();
            brick.node.flash.register_locks();
            brick.node.md.register_locks();
        }
        base.storage.register_locks();
        base.ioc3.register_locks();
        base.xbridge.register_locks();
        let (mut nat, mut frames) = (None, None);
        let net = GatewayConfig::from_network(&cfg.network());
        if let Some(link) = Link::with_gateway(net.clone())? {
            frames = Some(link.frames());
            nat = Some((link.nat(), (net.client_ip, net.gateway_ip, net.netmask)));
            base.eth.attach(link);
        }
        eprintln!(
            "iris: Origin 3200, {} C-brick{} of 512 MiB, {}, serial console {:?}",
            count,
            if count == 1 { "" } else { "s" },
            part.label(),
            cfg.fuel.console
        );
        Ok(Self {
            bricks: bricks_built,
            base,
            timers,
            monitor: Arc::new(monitor),
            monitor_port: cfg.monitor_port.unwrap_or(DEFAULT_MONITOR_PORT),
            ci_serial,
            console,
            ci: cfg.ci,
            scratch,
            timekeeper,
            nvram: cfg.nvram.clone(),
            nat,
            frames,
            events: Some(merged_rx),
            slot: None,
            lifecycle: GuestState::Idle,
        })
    }

    fn attach_scsi(cfg: &MachineConfig, base: &BaseIo) -> Result<Option<PathBuf>, String> {
        let attached = attach_from_config(cfg, Some("fuel"), &|channel| {
            base.storage.bus_for_channel(channel)
                .ok_or_else(|| format!("controller {channel} (an Origin 3200 has the I-brick's one Fibre Channel loop, channel 0)"))
        })?;
        if let Some(device) = attached.daynaports.first() {
            return Err(format!("{}: kind = \"daynaport\" is not available on the Origin 3200",
                scsi_label(device.controller, device.id)));
        }
        Ok(attached.scratch)
    }

    fn connect_serial(cfg: &MachineConfig, first: &CBrick, base: &BaseIo, console: usize) -> Result<(), String> {
        let [a, b] = cfg.serial_ports();
        let ports = [
            SerialPort {
                label: "L1",
                port: cfg.fuel.l1_port.unwrap_or(8882),
                setting: "fuel.l1_port",
                ci: &base.serial[0],
                log: cfg.fuel.l1_log.clone(),
                connect: Box::new(|backend| first.node.l1.set_serial_backend(backend)),
            },
            SerialPort {
                label: "IOC3 A",
                port: a,
                setting: "serial_port_a",
                ci: &base.serial[1],
                log: None,
                connect: Box::new(|backend| base.ioc3.set_serial_backend(0, backend)),
            },
            SerialPort {
                label: "IOC3 B",
                port: b,
                setting: "serial_port_b",
                ci: &base.serial[2],
                log: None,
                connect: Box::new(|backend| base.ioc3.set_serial_backend(1, backend)),
            },
        ];
        connect_serial_ports(cfg, "Origin 3200", &ports, console)
    }

    fn first(&self) -> &Arc<CBrick> {
        &self.bricks[0]
    }
}

/// One C-brick's processor, wired to its node: timers, Count rate and the
/// JIT's compile pool. Its batch hook polls the node.
fn spawn_cpu(node: &Node, part: &'static Part, cfg: &MachineConfig, timers: &Arc<TimerManager>, poll: Arc<NodePoll>, smp: bool) -> Arc<dyn CpuDevice> {
    let count_hz = cfg.clock.count_hz(part).max(1);
    let cpu = crate::with_cpu!(part, true, false, C => start_cpu::<C>(node, part, timers, count_hz, poll, smp));
    #[cfg(feature = "jitv2")]
    {
        use crate::traits::Device as _;
        let cpu_device: Arc<dyn Device> = cpu.clone().as_device();
        let jit = cpu.jitv2();
        let mut g = jit.lock();
        g.compile_queue.set_thread_count(cfg.jitv2.threads.max(1));
        g.compile_queue.set_cpu(Arc::downgrade(&cpu_device));
        g.compile_queue.set_owner(Arc::downgrade(&jit));
        // Not under lockstep, as on the other machines: lockstep compiles
        // inline on the CPU thread, and a running pool both owns the
        // Codegen inline needs and deadlocks the CPU thread's flush.
        #[cfg(not(feature = "jitv2_lockstep"))]
        {
            let stats = g.stats.clone();
            g.compile_queue.start(code_bus(node), stats);
        }
        // The compile pool keeps only a weak handle on the CPU.
        std::mem::forget(cpu_device);
    }
    cpu
}

/// The cached view of a node's memory, as its processor sees it.
fn cached_bus(node: &Node) -> Arc<dyn BusDevice> {
    node.cached.clone()
}

/// The bus the JIT compiles code through; see [`Ip35Bus::code_view`].
#[cfg(feature = "jitv2")]
fn code_bus(node: &Node) -> Arc<dyn BusDevice> {
    node.bus.code_view(cached_bus(node))
}

fn start_cpu<C>(node: &Node, part: &'static Part, timers: &Arc<TimerManager>, count_hz: u64, poll: Arc<NodePoll>, smp: bool) -> Arc<dyn CpuDevice>
where
    C: CacheCpuModel + From<Arc<dyn BusDevice>> + 'static,
    MipsCpu<MipsTlb, C>: CpuDevice,
{
    let mut exec = build_cpu::<C>(node, part, smp);
    exec.core.set_timer_manager(timers.clone());
    exec.core.set_count_hz(count_hz);
    exec.core.count_write_samples = true;
    exec.batch_hook = Some(Box::new(move |_| poll.poll()));
    Arc::new(MipsCpu::new(*exec))
}

/// A processor of type `C` with the part's identity and the C-brick's
/// boot-mode inputs, on the node's bus.
fn build_cpu<C: CacheCpuModel + From<Arc<dyn BusDevice>>>(node: &Node, part: &Part, smp: bool) -> Box<MipsExecutor<MipsTlb, C>> {
    let cfg = MipsCpuConfig::for_model::<C>(Arc::new(crate::ip35::memory_map(&node.md.ram_extent())))
        .with_prid(part.prid());
    let mut cpu = Box::new(MipsExecutor::new(
        node.processor.clone(),
        MipsTlb::new(cfg.tlb_entries),
        &cfg,
    ));
    cpu.phys_mask = CPU_PHYS_MASK;
    // Processors on several nodes share memory: another's store cannot clear
    // this one's LLbit, so a store-conditional must be a host compare-exchange.
    cpu.core.smp = smp;
    // The C-brick's flash record selects these boot-mode inputs. A mismatch
    // enters the PROM's error routine before GP is initialized.
    cpu.core.cp0_config = CBRICK_FIRMWARE.reset_config0();
    cpu.cache = C::from(cached_bus(node));
    // The PROM fetches its own code uncached; with memory over its window
    // the cached plane answers RAM there, so the fetch is presented at the
    // PROM's own address and compiled from the view that holds it.
    cpu.set_uncached_fetch_alias(
        crate::ip35::RESET_WINDOW,
        crate::ip35::RESET_WINDOW_SIZE,
        crate::ip35::RESET_WINDOW_FETCH,
    );
    #[cfg(feature = "jitv2")]
    cpu.set_jitv2_code_bus(code_bus(node));
    // The PROM keeps its stack in cache lines it tags for a window with no
    // memory behind it yet; the controller answers that window while they stand.
    let memory = node.md.clone();
    cpu.cache.set_pd_tag_store_hook(Box::new(move |tag| memory.startup_tag_store(tag)));
    cpu
}

impl BaseIo {
    /// The I-brick's devices on `node`'s XIO connector.
    fn new(node: &Node, cfg: &MachineConfig, timekeeper: Arc<Timekeeper>) -> Result<Self, String> {
        let (md, io) = (&node.md, &node.io);
        let serial = std::array::from_fn(|_| Arc::new(CiSerialBackend::new()));
        node.l1.set_serial_backend(serial[0].clone());
        let xbridge = XBridge::new(md.dma_view());
        for widget in [0, 10, 14, 15] {
            io.connect_widget(widget, xbridge.widget(widget).unwrap());
        }
        let weak_io = Arc::downgrade(io);
        xbridge.set_interrupt_sender(Arc::new(move |packet| {
            if packet.widget != 10 {
                return false;
            }
            weak_io.upgrade().is_some_and(|io| {
                io.inbound_write(
                    packet.source_widget,
                    None,
                    packet.address,
                    8,
                    packet.vector as u64 | if packet.level { 1 << 8 } else { 0 },
                ) == BUS_OK
            })
        }));
        let weak_io = Arc::downgrade(io);
        xbridge.set_dma_sender(Arc::new(move |packet, bytes| {
            if packet.widget != 10 || packet.half_read_requested || packet.half_write_requested {
                dlog!(LogModule::Bedrock,
                    "Origin 3200: unsupported XBridge DMA destination or halfword placement {packet:?}"
                );
                return false;
            }
            let Some(io) = weak_io.upgrade() else {
                return false;
            };
            // Preserve the width of XBridge's naturally aligned packet legs,
            // including Hub registers whose narrower accesses are refused.
            let size = bytes.len();
            if matches!(size, 1 | 2 | 4 | 8) && packet.address & (size as u64 - 1) == 0 {
                if packet.write {
                    let value = bytes.iter().fold(0u64, |value, byte| (value << 8) | *byte as u64);
                    return io.inbound_write(
                        packet.source_widget, Some(packet.slot), packet.address, size as u8, value,
                    ) == BUS_OK;
                }
                let value = io.inbound_read(packet.source_widget, Some(packet.slot), packet.address, size as u8);
                if value.status != BUS_OK {
                    return false;
                }
                bytes.copy_from_slice(&value.data.to_be_bytes()[8 - size..]);
                return true;
            }
            for (i, byte) in bytes.iter_mut().enumerate() {
                let a = packet.address + i as u64;
                if packet.write {
                    if io.inbound_write(packet.source_widget, Some(packet.slot), a, 1, *byte as u64) != BUS_OK {
                        return false;
                    }
                } else {
                    let value = io.inbound_read(packet.source_widget, Some(packet.slot), a, 1);
                    if value.status != BUS_OK {
                        return false;
                    }
                    *byte = value.data as u8;
                }
            }
            true
        }));
        let mac = node.l1.station_mac();
        let mut ioc3_config = Ioc3Config::fuel(mac);
        ioc3_config.keyboard_mouse = cfg.fuel.ps2_input;
        let ioc3 = Arc::new(Ioc3::new(ioc3_config));
        let eth = Ioc3Eth::new_fuel(mac);
        ioc3.attach_eth(eth.clone());
        ioc3.attach_timekeeper(timekeeper);
        ioc3.set_serial_backend(0, serial[1].clone());
        ioc3.set_serial_backend(1, serial[2].clone());
        let storage = Arc::new(Isp2200::new_at(0, Isp2200::new_bus()));
        let heartbeat = Arc::new(AtomicU64::new(0));
        ioc3.set_heartbeat(heartbeat.clone());
        eth.set_heartbeat(heartbeat);
        xbridge.attach(15, 1, storage.clone());
        // Whether this slot runs 64-bit data phases is a property of the
        // I-brick's board wiring, which no held document gives. It is inferred
        // from the PROM's behaviour: the control block and rings it builds
        // are right only if the bridge reverses each doubleword of the
        // adapter's DMA, as it does on a slot running 64-bit data phases;
        // on a 32-bit slot the halves come out exchanged.
        xbridge.set_dma_64bit(15, 1, true);
        xbridge.attach(15, 4, ioc3.clone());
        let usb = Ohci::new();
        usb.set_clock(host_clock());
        if cfg.fuel.usb_input {
            // Host input reaches the USB devices through the same controller
            // object the GUI, VNC and CI already drive; each device drops
            // what its guest has not enabled.
            let keyboard = UsbKeyboard::new();
            let mouse = UsbMouse::new();
            usb.attach(0, keyboard.clone())?;
            usb.attach(1, mouse.clone())?;
            ioc3.ps2().add_sink(keyboard);
            ioc3.ps2().add_sink(mouse);
        }
        xbridge.attach(15, 5, usb.clone());
        for slot in [1, 4, 5] {
            xbridge.set_pci_irq(15, slot, 0, xbridge.pin_line(15, slot));
        }
        ioc3.connect_superio_interrupt(xbridge.pin_line(15, 0));
        let (reset_ioc3, reset_eth, reset_storage, reset_usb) = (ioc3.clone(), eth.clone(), storage.clone(), usb.clone());
        xbridge.set_reset_output(Arc::new(move |port| {
            if port == 15 {
                reset_ioc3.power_on();
                reset_eth.power_on();
                reset_storage.power_on();
                reset_usb.power_on();
            }
        }));
        Ok(Self { xbridge, ioc3, eth, storage, usb, serial })
    }

    fn power_on(&self) {
        self.xbridge.power_on();
        self.ioc3.power_on();
        self.eth.power_on();
        self.storage.power_on();
        self.usb.power_on();
    }

    fn start(&self) {
        self.ioc3.start();
        self.eth.start();
        self.storage.start();
        self.usb.start();
    }

    fn stop(&self) {
        self.usb.stop();
        self.storage.stop();
        self.eth.stop();
        self.ioc3.stop();
    }
}

impl CBrick {
    /// Power-on reset of the node's blocks; the processor is the caller's.
    fn power_on(&self) {
        let n = &self.node;
        for pi in &n.pi {
            pi.power_on();
        }
        n.md.power_on();
        n.lb.power_on();
        n.io.power_on();
        n.ni.power_on();
        n.xb.power_on();
        n.l1.power_on();
        n.flash.power_on();
    }

    fn start(&self) {
        self.poll.last_clock.store((self.poll.clock)(), Ordering::Release);
        self.node.md.start_clock();
        self.poll.running.store(true, Ordering::Release);
        for pi in &self.node.pi {
            pi.start();
        }
        self.node.l1.start();
    }

    fn stop(&self) {
        self.poll.poll();
        self.poll.running.store(false, Ordering::Release);
        self.node.md.stop_clock();
        self.node.l1.stop();
        for pi in &self.node.pi {
            pi.stop();
        }
    }
}

impl NodePoll {
    fn poll(&self) {
        if let Some((away, phys)) = &self.away {
            away.sync_window(phys);
        }
        if self.running.load(Ordering::Acquire) {
            let now = (self.clock)();
            let previous = self.last_clock.swap(now, Ordering::AcqRel);
            self.flash.advance_ns(now.saturating_sub(previous));
            if let Some(usb) = &self.usb {
                usb.sync_clock();
            }
            for pi in &self.pi {
                pi.poll();
            }
        }
    }
}

/// One processor interface's graphics flow-control registers (0x300 to
/// 0x348): stored as written, with no store counting.
#[derive(Default)]
struct PiGraphics(Mutex<PiGfx>);
impl crate::dev::bedrock_pi::NodeHooks for PiGraphics {
    fn gfx_read(&self, index: usize) -> Option<u64> {
        (index < PI_GFX_REGS as usize).then(|| self.0.lock().read(index as u32))
    }
    fn gfx_write(&self, index: usize, value: u64) -> bool {
        if index >= PI_GFX_REGS as usize {
            return false;
        }
        self.0.lock().write(index as u32, value);
        true
    }
    fn gfx_reset(&self) {
        *self.0.lock() = PiGfx::default();
    }
}

impl Workstation for Ip35Machine {
    fn start(&mut self) {
        self.lifecycle = GuestState::Starting;
        self.monitor.clone().start_server(format!("127.0.0.1:{}", self.monitor_port));
        self.restart_peripherals();
        if !self.ci {
            self.cpu_start();
        }
    }
    fn stop(&mut self) {
        for brick in &self.bricks {
            brick.cpu.clone().as_device().stop();
            brick.stop();
        }
        self.base.stop();
        if let Err(e) = self.timekeeper.flush_nvram() {
            dlog!(LogModule::Bedrock, "Origin 3200 NVRAM: {e}");
        }
        if self.lifecycle != GuestState::Idle {
            self.lifecycle = GuestState::Halted;
        }
    }
    fn reset(&mut self) {
        self.stop();
        for brick in &self.bricks {
            brick.cpu.power_on();
            brick.power_on();
        }
        self.base.power_on();
        self.timekeeper.power_on();
        self.restart_peripherals();
    }
    fn restart_peripherals(&mut self) {
        for brick in &self.bricks {
            brick.start();
        }
        self.base.start();
    }
    fn cpu_start(&self) {
        // Every node's PROM starts at power-on: each waits for the others'
        // link and discovery, and the first node gives up on a peer that
        // comes up after its second poll of the link.
        for brick in &self.bricks {
            brick.cpu.clone().as_device().start();
        }
    }
    fn cpu_is_running(&self) -> bool {
        self.first().cpu.clone().as_device().is_running()
    }
    fn machine_slot(&mut self) -> MachineSlot {
        let ptr = self as *mut Self as *mut dyn Workstation;
        self.slot
            .get_or_insert_with(|| Arc::new(Mutex::new(MachineRef(ptr))))
            .clone()
    }
    fn register_system_controller(&mut self) {
        let slot = self.machine_slot();
        let events = self
            .events
            .take()
            .expect("Origin 3200 system controller registered twice");
        register_system_controller(&self.monitor, slot, Some(events));
    }
    fn monitor_addr(&self) -> Option<SocketAddr> {
        self.monitor.bound_addr()
    }
    fn lifecycle(&self) -> GuestState {
        self.lifecycle
    }
    fn set_lifecycle(&mut self, state: GuestState) {
        self.lifecycle = state;
    }
    fn get_display(&self) -> Option<Arc<dyn GfxDisplay>> {
        None
    }
    fn register_monitor_device(&self, device: Arc<dyn Device>) {
        self.monitor.register_device(device);
    }
    fn get_ps2(&self) -> Arc<Ps2Controller> {
        self.base.ioc3.ps2()
    }
    fn get_timer_manager(&self) -> Arc<TimerManager> {
        self.timers.clone()
    }
    fn get_cpu_debug(&self) -> Arc<dyn CpuDebug> {
        self.first().cpu.clone().debug_adapter()
    }
    fn fasttick_count(&self) -> Arc<AtomicU64> {
        self.first().cpu.fasttick_count()
    }
    fn get_ci_serial(&self) -> Option<Arc<CiSerialBackend>> {
        self.ci_serial.clone()
    }
    fn inject_serial_console(&self, bytes: &[u8]) {
        self.base.serial[self.console].push_host(bytes);
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn save_nvram(&self, path: Option<&str>) -> Result<String, String> {
        if path.is_some_and(|p| p != self.nvram) {
            return Err("Origin 3200 NVRAM save uses the configured path".into());
        }
        self.timekeeper.flush_nvram()?;
        Ok(self.nvram.clone())
    }
    fn scratch_path(&self) -> Option<&Path> {
        self.scratch.as_deref()
    }
    fn disc_changer(&self) -> Option<Arc<dyn DiscChanger>> {
        Some(self.base.storage.clone() as Arc<dyn DiscChanger>)
    }
    fn pending_chd_sync_count(&self) -> usize {
        self.base.storage.bus_for_channel(0).unwrap().pending_chd_sync_count()
    }
    fn flush_disks(&self) -> std::io::Result<()> {
        self.base.storage.bus_for_channel(0).unwrap().flush()
    }
    fn sync_chd_disks(
        &self,
        only: Option<usize>,
        progress: &mut dyn FnMut(usize, usize, f32),
        cancel: &dyn Fn() -> bool,
    ) -> std::io::Result<usize> {
        let buses = vec![self.base.storage.bus_for_channel(0).unwrap()];
        ScsiBus::sync_chd_disks(&buses, only, progress, cancel)
    }
    fn net_frames(&self) -> Option<Arc<AtomicU64>> { self.frames.clone() }
    fn nat_control(&self) -> Option<Arc<NatControl>> {
        self.nat.as_ref().map(|(n, _)| n.clone())
    }
    fn nat_expected(&self) -> Option<(Ipv4Addr, Ipv4Addr, Ipv4Addr)> {
        self.nat.as_ref().map(|(_, a)| *a)
    }
}

impl Drop for Ip35Machine {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            slot.lock().0 = MachineRef::null();
        }
        self.stop();
        // The compile pools' requests point into the memory freed below.
        #[cfg(feature = "jitv2")]
        for brick in &self.bricks {
            crate::cpu::jitv2::Jitv2::stop_for_drop(&brick.cpu.jitv2(), || brick.cpu.clone().as_device().stop());
        }
        crate::workstation::flush_on_drop(self);
        self.monitor.shutdown();
        // The bus holds the devices, and the nodes hold its memory, which
        // owns the bus: let go, or none of it is freed.
        for brick in &self.bricks {
            brick.node.md.physical().release_devices();
        }
    }
}

#[cfg(test)]
mod tests;
