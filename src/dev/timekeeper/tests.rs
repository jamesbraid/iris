use super::*;
use crate::traits::{BusDevice, Resettable};

#[test]
fn clock_freeze_stop_reset_and_rollover() {
    let t = Timekeeper::fuel_at(2067, 12, 31, 23, 59, 59).unwrap();
    t.write8(0x7f8, 0x42);
    t.write8(CONTROL as u64, 0x40);
    t.advance_for_test(2);
    assert_eq!(t.read8(0x7fff).data, 0x99);
    t.power_on();
    assert_eq!(t.read8(CONTROL as u64).data, 0);
    assert_eq!(t.read8(0x7fff).data, 0);
    assert_eq!(t.read8(0x7ffe).data, 1);
    assert_eq!(t.read8(0x7ffd).data, 1);
    assert_eq!(t.read8(0x7f8).data, 0x42);
    t.write8(CONTROL as u64, 0x80);
    t.write8(0x7ff9, 0x89);
    t.write8(CONTROL as u64, 0);
    t.advance_for_test(60);
    assert_eq!(t.read8(0x7ff9).data, 0x89);
}

#[test]
fn persisted_clock_accounts_for_elapsed_time_unless_stopped() {
    for stopped in [false, true] {
        let t = Timekeeper::fuel_at(2024, 2, 28, 23, 59, 59).unwrap();
        t.write8(0x7ff0, 0xa5);
        if stopped {
            t.write8(CONTROL as u64, 0x80);
            t.write8(0x7ff9, 0x85);
            t.write8(CONTROL as u64, 0);
        }
        let (image, reference) = t.persistence_image_at(100);
        let restored = Timekeeper::fuel_at(2026, 1, 1, 0, 0, 0).unwrap();
        restored.restore_image_at(&image, Some(&reference), 102).unwrap();
        assert_eq!(restored.read8(0x7ffd).data, if stopped { 0x28 } else { 0x29 });
        assert_eq!(restored.read8(0x7ff9).data, if stopped { 0x85 } else { 1 });
        assert_eq!(restored.read8(0x7ff0).data, 0xa5);
        assert!(restored.restore_image_at(&image[..2048], None, 102).is_err());
    }
}

#[test]
fn backed_storage_preserves_saved_state_and_rejects_invalid_files() {
    let dir = std::env::current_exe().unwrap().parent().unwrap()
        .join(format!("timekeeper-test-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("nvram");
    let t = Timekeeper::fuel_at(2026, 1, 1, 0, 0, 0).unwrap();
    t.set_nvram_file(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), vec![0; SIZE]);
    t.write8(0x7f8, 0x42);
    assert_eq!(std::fs::read(&path).unwrap()[0x7f8], 0x42);
    t.flush_nvram().unwrap();
    drop(t);
    // Saved clock state is authoritative even if the host date is out of range.
    let restored = Timekeeper::backed(&path, Timekeeper::fuel_at(2068, 1, 1, 0, 0, 0)).unwrap();
    restored.power_on();
    assert_eq!(restored.read8(0x7f8).data, 0x42);
    assert_eq!(restored.read8(0x7fff).data, 0x58);
    drop(restored);
    std::fs::remove_file(dir.join("nvram.clock")).unwrap();
    std::fs::write(&path, [0xa5; 2048]).unwrap();
    assert!(Timekeeper::backed(&path, Timekeeper::fuel_at(2026, 1, 1, 0, 0, 0)).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), vec![0xa5; 2048]);
    assert!(!dir.join("nvram.clock").exists());
    std::fs::remove_file(&path).unwrap();
    assert!(Timekeeper::backed(&path, Timekeeper::fuel_at(2068, 1, 1, 0, 0, 0)).is_err());
    assert!(!path.exists());
    std::fs::remove_dir(dir).unwrap();
}
