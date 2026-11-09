//! The `iris.toml` a scenario run boots with: the scenario's own `[iris]`
//! table plus the keys only the harness may set.

use std::path::Path;

use toml::{Table, Value};

use super::scenario::Scenario;

/// Host ports and paths the harness picked for one run.
pub struct Wiring<'a> {
    pub run_dir: &'a Path,
    pub prom: Option<&'a Path>,
    pub disk: &'a Path,
    pub monitor_port: u16,
    pub serial_ports: [u16; 2],
    /// Free loopback ports for `{port1}`..`{port4}`, for machines whose
    /// console or extra serial lines listen on their own host port.
    pub extra_ports: [u16; 4],
}

/// Where the harness has iris write the console. Phases are matched here.
pub const SERIAL_LOG: &str = "serial.log";
pub const CI_SOCKET: &str = "ci.sock";

/// Replace `{run}` in every string of the table, at any depth. A string that
/// is exactly `{portN}` becomes that port as an integer, since iris reads
/// ports as numbers.
fn substitute(v: &mut Value, run: &str, ports: &[u16; 4]) {
    match v {
        Value::String(s) => {
            let port = (1..=4).find(|n| *s == format!("{{port{n}}}"));
            match port {
                Some(n) => *v = Value::Integer(ports[n - 1].into()),
                None => *s = s.replace("{run}", run),
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| substitute(x, run, ports)),
        Value::Table(t) => t.iter_mut().for_each(|(_, x)| substitute(x, run, ports)),
        _ => {}
    }
}

fn path_value(p: &Path) -> Value {
    Value::String(p.to_string_lossy().into_owned())
}

/// The disk is always attached with `overlay = true`: iris opens the image
/// read-only and keeps the guest's writes in a sparse file in the run
/// directory (iris puts a CI run's overlay under `$TMPDIR`, which the harness
/// points there). The prepared image is never written.
pub fn render(s: &Scenario, w: &Wiring) -> Result<String, String> {
    let run = w.run_dir.to_string_lossy();
    let mut t: Table = s.iris.clone();
    t.iter_mut().for_each(|(_, v)| substitute(v, &run, &w.extra_ports));

    if let Some(p) = w.prom {
        t.insert("prom".into(), path_value(p));
    }
    t.insert("nvram".into(), path_value(&w.run_dir.join("nvram.bin")));
    t.insert("headless".into(), Value::Boolean(true));
    t.insert("no_audio".into(), Value::Boolean(true));
    t.insert("monitor_port".into(), Value::Integer(w.monitor_port.into()));
    t.insert("serial_port_a".into(), Value::Integer(w.serial_ports[0].into()));
    t.insert("serial_port_b".into(), Value::Integer(w.serial_ports[1].into()));
    t.insert("serial_log".into(), path_value(&w.run_dir.join(SERIAL_LOG)));
    t.insert("ci".into(), Value::Boolean(true));
    t.insert("ci_socket".into(), path_value(&w.run_dir.join(CI_SOCKET)));

    let mut disk = Table::new();
    disk.insert("path".into(), path_value(w.disk));
    disk.insert("cdrom".into(), Value::Boolean(false));
    disk.insert("overlay".into(), Value::Boolean(true));
    let mut scsi = Table::new();
    scsi.insert(s.assets.disk_id.to_string(), Value::Table(disk));
    t.insert("scsi".into(), Value::Table(scsi));

    toml::to_string(&t).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MachineConfig;

    fn scenario(extra_iris: &str) -> Scenario {
        Scenario::parse(&format!(
            r#"
            name = "m"
            speed = "quick"
            [assets]
            disk = "d.raw"
            prom = "p.bin"
            [iris.machine]
            profile = "o2_ip32"
            {extra_iris}
            [[phase]]
            name = "login"
            expect = "login:"
            timeout = 1
            "#
        ))
        .unwrap()
    }

    fn wiring<'a>(run: &'a Path, disk: &'a Path, prom: &'a Path) -> Wiring<'a> {
        Wiring { run_dir: run, prom: Some(prom), disk, monitor_port: 4001, serial_ports: [4002, 4003], extra_ports: [4004, 4005, 4006, 4007] }
    }

    #[test]
    fn the_disk_is_always_a_copy_on_write_overlay() {
        let (run, disk, prom) = (Path::new("/r"), Path::new("/d/master.raw"), Path::new("/p.bin"));
        let text = render(&scenario(""), &wiring(run, disk, prom)).unwrap();
        let v: Table = toml::from_str(&text).unwrap();
        let d = &v["scsi"]["1"];
        assert_eq!(d["overlay"].as_bool(), Some(true));
        assert_eq!(d["path"].as_str(), Some("/d/master.raw"));
        assert_eq!(v["ci"].as_bool(), Some(true));
        assert_eq!(v["serial_log"].as_str(), Some("/r/serial.log"));
        assert_eq!(v["machine"]["profile"].as_str(), Some("o2_ip32"));
    }

    #[test]
    fn run_placeholders_expand_at_any_depth() {
        let s = scenario("[iris.o2]\nflash = \"{run}/flash.bin\"\nlist = [\"{run}/a\"]\nl1_port = \"{port2}\"");
        let text = render(&s, &wiring(Path::new("/r"), Path::new("/d"), Path::new("/p"))).unwrap();
        let v: Table = toml::from_str(&text).unwrap();
        assert_eq!(v["o2"]["flash"].as_str(), Some("/r/flash.bin"));
        assert_eq!(v["o2"]["list"][0].as_str(), Some("/r/a"));
        assert_eq!(v["o2"]["l1_port"].as_integer(), Some(4005));
    }

    #[test]
    fn the_rendered_file_is_one_iris_itself_accepts() {
        let text = render(&scenario(""), &wiring(Path::new("/r"), Path::new("/d"), Path::new("/p"))).unwrap();
        let cfg: Result<MachineConfig, _> = toml::from_str(&text);
        assert!(cfg.is_ok(), "{:?}\n{text}", cfg.err());
    }
}
